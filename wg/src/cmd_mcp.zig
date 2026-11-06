//! `wg mcp` — the collaboration MCP surface, served over **stdio JSON-RPC**
//! (D70). The host's `/mcp` keeps the lanes that need host knowledge
//! (`repo_*`, `wal_ls`); the D1 collaboration tools and resources live here,
//! over the same `refs/collab/*` aggregation every other wg command reads.
//!
//! Contract (see `skills/wg/SKILL.md` §7):
//! * stdout carries **only** JSON-RPC messages, one per line; logs go to stderr.
//! * stdio EOF exits 0 after stopping the subscription poller.
//! * requests are handled serially (the local refs are not concurrently safe);
//!   only the best-effort subscription poller runs beside the loop.
//! * every read fetches `refs/collab/{inbox,meta}` first (`git fetch --prune`,
//!   the same lane `wg collab claim` step 1 uses) and **fails closed** when
//!   that fails; `fresh=false` (per call) or `--no-fetch` (process) reads the
//!   cached refs and reports `_meta.fetched_at` instead.
//! * `collab_publish` takes an entry the caller already signed, checks its
//!   structure and that `actor` is this worktree's identity, and pushes it to
//!   `refs/collab/inbox/<actor>/<uuid>`. Signatures are verified at aggregation
//!   time, never on write (D64 §write path) — and a raw `git push` can bypass
//!   every local check (D1_PROTOCOL §7.5).
//! * `collab_register` (`force?`) names this worktree identity's public key
//!   at `refs/collab/meta/principals/<principal>` and pushes it. A different
//!   already-registered key is refused unless `force:true`; rotation makes
//!   every past entry by that principal unverified.

const std = @import("std");
const args = @import("args.zig");
const cmd_collab = @import("cmd_collab.zig");
const cmd_ci = @import("cmd_ci.zig");
const collab = @import("d1/collab.zig");
const ci = @import("d1/ci.zig");
const entry_mod = @import("d1/entry.zig");
const collabreader = @import("collabreader.zig");
const gitx = @import("gitx.zig");
const identity = @import("identity.zig");
const jsonio = @import("jsonio.zig");
const util = @import("util.zig");

const Io = std.Io;
const Value = std.json.Value;
const Ctx = cmd_collab.Ctx;

const DEFAULT_SUBSCRIBE_INTERVAL_SECS: u64 = 5;
const DEFAULT_MAX_SUBSCRIPTIONS: usize = 32;
const MIN_SUBSCRIBE_INTERVAL_SECS: u64 = 1;

/// One JSON-RPC request line. The entry cap is 256 KiB; a request carries the
/// entry plus the envelope, and `entry` may be its escaped JSON text (an
/// attachment is base64 inside it), so the transport must hold roughly twice
/// the entry cap. A line beyond this is refused as one request and discarded,
/// never a reason to end the process.
const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

/// The MCP protocol version this server answers when the client names none.
const DEFAULT_PROTOCOL_VERSION = "2024-11-05";

// ---- SipHash-1-3 (Rust's `DefaultHasher`) -----------------------------------------------
//
// The host's `resources/read` board version is `b:<witness>:claim:<expires_at>`
// where `<witness>` is Rust's `DefaultHasher` over (column index, card id,
// last_oid, status, owner) for every card. Reproducing it here keeps the two
// implementations' version strings comparable across the wire — `SipHasher13`
// with the fixed (0, 0) keys, over the same byte stream (`usize` little-endian,
// each `str` followed by 0xff).
const Sip13 = struct {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    buf: [8]u8 = undefined,
    buf_len: usize = 0,
    total: usize = 0,

    fn init() Sip13 {
        return .{
            .v0 = 0x736f6d6570736575,
            .v1 = 0x646f72616e646f6d,
            .v2 = 0x6c7967656e657261,
            .v3 = 0x7465646279746573,
        };
    }

    fn round(s: *Sip13) void {
        s.v0 +%= s.v1;
        s.v1 = std.math.rotl(u64, s.v1, 13);
        s.v1 ^= s.v0;
        s.v0 = std.math.rotl(u64, s.v0, 32);
        s.v2 +%= s.v3;
        s.v3 = std.math.rotl(u64, s.v3, 16);
        s.v3 ^= s.v2;
        s.v0 +%= s.v3;
        s.v3 = std.math.rotl(u64, s.v3, 21);
        s.v3 ^= s.v0;
        s.v2 +%= s.v1;
        s.v1 = std.math.rotl(u64, s.v1, 17);
        s.v1 ^= s.v2;
        s.v2 = std.math.rotl(u64, s.v2, 32);
    }

    fn compress(s: *Sip13, m: u64) void {
        s.v3 ^= m;
        s.round();
        s.v0 ^= m;
    }

    fn write(s: *Sip13, bytes: []const u8) void {
        s.total += bytes.len;
        var i: usize = 0;
        if (s.buf_len > 0) {
            while (s.buf_len < 8 and i < bytes.len) : (i += 1) {
                s.buf[s.buf_len] = bytes[i];
                s.buf_len += 1;
            }
            if (s.buf_len == 8) {
                s.compress(std.mem.readInt(u64, s.buf[0..8], .little));
                s.buf_len = 0;
            }
        }
        while (i + 8 <= bytes.len) : (i += 8) {
            s.compress(std.mem.readInt(u64, bytes[i..][0..8], .little));
        }
        while (i < bytes.len) : (i += 1) {
            s.buf[s.buf_len] = bytes[i];
            s.buf_len += 1;
        }
    }

    fn writeUsize(s: *Sip13, x: usize) void {
        var b: [8]u8 = undefined;
        std.mem.writeInt(u64, &b, @intCast(x), .little);
        s.write(&b);
    }

    /// Rust's `Hash for str`: the bytes then the 0xff terminator.
    fn writeStr(s: *Sip13, str: []const u8) void {
        s.write(str);
        s.write(&[_]u8{0xff});
    }

    fn finish(s: *Sip13) u64 {
        var tail = [_]u8{ 0, 0, 0, 0, 0, 0, 0, 0 };
        @memcpy(tail[0..s.buf_len], s.buf[0..s.buf_len]);
        var b: u64 = @as(u64, @intCast(s.total & 0xff)) << 56;
        b |= std.mem.readInt(u64, &tail, .little);
        s.v3 ^= b;
        s.round();
        s.v0 ^= b;
        s.v2 ^= 0xff;
        s.round();
        s.round();
        s.round();
        return s.v0 ^ s.v1 ^ s.v2 ^ s.v3;
    }
};

test "siphash13 matches Rust's DefaultHasher byte-for-byte" {
    // Reference values printed by:
    //   let mut h = DefaultHasher::new(); 0usize.hash(&mut h);
    //   "card-1".hash(&mut h); ... ("abc123","open","bob","card-2",...)
    var h = Sip13.init();
    h.writeUsize(0);
    h.writeStr("card-1");
    h.writeStr("abc123");
    h.writeStr("open");
    h.writeStr("bob");
    h.writeUsize(1);
    h.writeStr("card-2");
    h.writeStr("def456");
    h.writeStr("needs-review");
    h.writeStr("alice");
    try std.testing.expectEqual(@as(u64, 0x3eabf49141e1985a), h.finish());

    var h2 = Sip13.init();
    h2.writeStr("");
    try std.testing.expectEqual(@as(u64, 0x30406ea523c53def), h2.finish());

    var h3 = Sip13.init();
    h3.write(&[_]u8{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 });
    try std.testing.expectEqual(@as(u64, 0xcb7e0c6c167ecd44), h3.finish());
}

// ---- server state -----------------------------------------------------------------------

const Sub = struct {
    uri: []const u8,
    version: []const u8,
};

const Server = struct {
    c: *Ctx,
    repo: []const u8,
    remote: []const u8,
    /// `--no-fetch` is a hard gate: no per-call argument can re-enable the
    /// network (an air-gapped/offline process stays offline).
    no_fetch: bool,
    fresh_default: bool,
    interval_ms: u64,
    max_subs: usize,
    git_mutex: std.Io.Mutex = .init,
    subs_mutex: std.Io.Mutex = .init,
    out_mutex: std.Io.Mutex = .init,
    subs: util.List(Sub) = undefined,
    last_fetch: std.atomic.Value(i64) = std.atomic.Value(i64).init(0),
    stop: std.atomic.Value(bool) = std.atomic.Value(bool).init(false),
    poller: ?std.Thread = null,

    fn writeLine(self: *Server, alloc: std.mem.Allocator, bytes: []const u8) void {
        _ = alloc;
        self.out_mutex.lockUncancelable(self.c.io);
        defer self.out_mutex.unlock(self.c.io);
        std.Io.File.stdout().writeStreamingAll(self.c.io, bytes) catch {};
        std.Io.File.stdout().writeStreamingAll(self.c.io, "\n") catch {};
    }
};

pub fn run(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const interval = p.u64_of("subscribe-interval", DEFAULT_SUBSCRIBE_INTERVAL_SECS) catch return error.BadFlagValue;
    if (interval < MIN_SUBSCRIBE_INTERVAL_SECS) {
        std.debug.print("wg mcp: --subscribe-interval must be >= {d}s (D55: each tick is one refs read per subscription)\n", .{MIN_SUBSCRIBE_INTERVAL_SECS});
        return error.BadFlagValue;
    }
    const max_subs = p.u64_of("max-subscriptions", DEFAULT_MAX_SUBSCRIPTIONS) catch return error.BadFlagValue;
    if (max_subs < 1) {
        std.debug.print("wg mcp: --max-subscriptions must be >= 1\n", .{});
        return error.BadFlagValue;
    }

    var server = Server{
        .c = c,
        .repo = repo,
        .remote = remote,
        .no_fetch = p.boolean("no-fetch"),
        .fresh_default = !p.boolean("no-fetch"),
        .interval_ms = interval * 1000,
        .max_subs = @intCast(max_subs),
        .subs = util.List(Sub).init(c.alloc),
    };
    // The clock's shared `std.Io.Threaded` is created lazily; create it before
    // the poller thread exists so two threads never race its initialization.
    _ = util.now_ts();
    server.poller = std.Thread.spawn(.{}, pollLoop, .{&server}) catch null;
    if (server.poller == null) {
        std.debug.print("wg mcp: could not start the subscription poller; subscriptions will not notify\n", .{});
    }
    const code = serveLoop(&server) catch |err| blk: {
        std.debug.print("wg mcp: {t}\n", .{err});
        break :blk 1;
    };
    server.stop.store(true, .release);
    if (server.poller) |t| t.join();
    return code;
}

/// Consume the remainder of an oversized line, including its newline. The
/// reader's buffer is full and holds no delimiter, so drop it and keep filling;
/// when the tail finally fits it is consumed too.
fn discardLine(r: *Io.Reader) void {
    while (true) {
        _ = r.takeDelimiterInclusive('\n') catch |err| switch (err) {
            error.StreamTooLong => {
                r.tossBuffered();
                continue;
            },
            else => return,
        };
        return;
    }
}

fn serveLoop(s: *Server) !u8 {
    const rbuf = s.c.alloc.alloc(u8, MAX_LINE_BYTES) catch return error.OutOfMemory;
    defer s.c.alloc.free(rbuf);
    var reader = std.Io.File.stdin().reader(s.c.io, rbuf);
    while (true) {
        const raw_line = reader.interface.takeDelimiterInclusive('\n') catch |err| switch (err) {
            error.EndOfStream => break,
            // Too long to be one message: drain it, answer once, and keep the
            // lane (including its subscriptions) alive.
            error.StreamTooLong => {
                discardLine(&reader.interface);
                var arena_state = std.heap.ArenaAllocator.init(s.c.alloc);
                defer arena_state.deinit();
                const msg = std.fmt.allocPrint(arena_state.allocator(), "request line exceeds {d} KiB", .{MAX_LINE_BYTES / 1024}) catch "request line too long";
                writeRpcError(s, arena_state.allocator(), .null, -32700, msg);
                continue;
            },
            else => break,
        };
        const trimmed = std.mem.trim(u8, raw_line, " \t\r\n");
        if (trimmed.len == 0) continue;
        handleLine(s, trimmed);
    }
    return 0;
}

// ---- JSON-RPC ---------------------------------------------------------------------------

fn writeRpcResult(s: *Server, a: std.mem.Allocator, id: Value, result: Value) void {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "jsonrpc", .{ .string = "2.0" });
    jsonio.obj_put(&o, a, "id", id);
    jsonio.obj_put(&o, a, "result", result);
    s.writeLine(a, jsonio.to_string(a, .{ .object = o }));
}

fn writeRpcError(s: *Server, a: std.mem.Allocator, id: Value, code: i64, message: []const u8) void {
    var err_obj = jsonio.new_object();
    jsonio.obj_put(&err_obj, a, "code", .{ .integer = code });
    jsonio.obj_put(&err_obj, a, "message", .{ .string = message });
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "jsonrpc", .{ .string = "2.0" });
    jsonio.obj_put(&o, a, "id", id);
    jsonio.obj_put(&o, a, "error", .{ .object = err_obj });
    s.writeLine(a, jsonio.to_string(a, .{ .object = o }));
}

fn handleLine(s: *Server, line: []const u8) void {
    var arena_state = std.heap.ArenaAllocator.init(s.c.alloc);
    defer arena_state.deinit();
    const a = arena_state.allocator();

    const v = std.json.parseFromSliceLeaky(Value, a, line, .{}) catch {
        writeRpcError(s, a, .null, -32700, "parse error");
        return;
    };
    if (v != .object) {
        writeRpcError(s, a, .null, -32600, "invalid request: not an object");
        return;
    }
    const obj = v.object;
    const has_id = obj.contains("id");
    const id: Value = obj.get("id") orelse .null;
    const method_value = obj.get("method") orelse {
        if (has_id) writeRpcError(s, a, id, -32600, "invalid request: no method");
        return;
    };
    const method = switch (method_value) {
        .string => |m| m,
        else => {
            if (has_id) writeRpcError(s, a, id, -32600, "invalid request: method is not a string");
            return;
        },
    };
    if (std.mem.startsWith(u8, method, "notifications/")) return;
    if (!has_id) return; // a request without an id is a notification: never answered
    const params: Value = obj.get("params") orelse .{ .object = jsonio.new_object() };

    if (std.mem.eql(u8, method, "initialize")) {
        const protocol = switch (params) {
            .object => |o| switch (o.get("protocolVersion") orelse .null) {
                .string => |p| p,
                else => DEFAULT_PROTOCOL_VERSION,
            },
            else => DEFAULT_PROTOCOL_VERSION,
        };
        writeRpcResult(s, a, id, initializeResult(a, protocol));
        return;
    }
    if (std.mem.eql(u8, method, "ping")) {
        writeRpcResult(s, a, id, .{ .object = jsonio.new_object() });
        return;
    }
    if (std.mem.eql(u8, method, "tools/list")) {
        writeRpcResult(s, a, id, toolsList(a));
        return;
    }
    if (std.mem.eql(u8, method, "tools/call")) {
        handleToolCall(s, a, id, params);
        return;
    }
    if (std.mem.eql(u8, method, "resources/list")) {
        const result = resourcesList(s, a) catch |e| {
            writeRpcError(s, a, id, -32603, std.fmt.allocPrint(a, "resources/list failed: {t}", .{e}) catch "resources/list failed");
            return;
        };
        writeRpcResult(s, a, id, result);
        return;
    }
    if (std.mem.eql(u8, method, "resources/templates/list")) {
        writeRpcResult(s, a, id, templatesList(a));
        return;
    }
    if (std.mem.eql(u8, method, "resources/read")) {
        handleResourceRead(s, a, id, params);
        return;
    }
    if (std.mem.eql(u8, method, "resources/subscribe")) {
        handleSubscribe(s, a, id, params);
        return;
    }
    if (std.mem.eql(u8, method, "resources/unsubscribe")) {
        handleUnsubscribe(s, a, id, params);
        return;
    }
    writeRpcError(s, a, id, -32601, "method not found");
}

fn initializeResult(a: std.mem.Allocator, protocol: []const u8) Value {
    var caps = jsonio.new_object();
    jsonio.obj_put(&caps, a, "tools", .{ .object = jsonio.new_object() });
    var res = jsonio.new_object();
    jsonio.obj_put(&res, a, "subscribe", .{ .bool = true });
    jsonio.obj_put(&res, a, "listChanged", .{ .bool = true });
    jsonio.obj_put(&caps, a, "resources", .{ .object = res });
    var info = jsonio.new_object();
    jsonio.obj_put(&info, a, "name", .{ .string = "wg" });
    jsonio.obj_put(&info, a, "version", .{ .string = util.version });
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "protocolVersion", .{ .string = protocol });
    jsonio.obj_put(&o, a, "capabilities", .{ .object = caps });
    jsonio.obj_put(&o, a, "serverInfo", .{ .object = info });
    jsonio.obj_put(&o, a, "instructions", .{ .string = INSTRUCTIONS });
    return .{ .object = o };
}

const INSTRUCTIONS =
    \\wg is the collaboration MCP surface of a walgit checkout. It reads and
    \\writes the repository's D1 layer (`refs/collab/*`) locally.
    \\
    \\Rules that cost a wrong call if skipped:
    \\* Read tools fetch `refs/collab/{inbox,meta}` first and fail closed when
    \\  the remote is unreachable. `fresh:false` reads the cached refs; the
    \\  answer's `_meta.fetched_at` says when they were last fetched.
    \\* `collab_publish` takes an entry you already signed with your own key.
    \\  `actor` must be this worktree's identity principal; the signature is
    \\  checked when entries are aggregated, never on write.
    \\* `collab_register` (optional `force`) registers this worktree
    \\  identity's public key. A different existing key is refused unless
    \\  forced — rotation makes every past entry by that principal unverified.
    \\  Run `wg collab join --principal <name>` first.
    \\* Claiming a card is read board -> publish `card_claim` -> re-read board
    \\  (`wg collab claim` does exactly this); a live claim by someone else
    \\  means stand down, not start work.
    \\* Subscriptions are polls: an update notification means "re-read", never
    \\  "here is the diff"
;

// ---- tool definitions -------------------------------------------------------------------

const ToolKind = enum {
    collab_board,
    collab_thread,
    collab_report,
    ci_status,
    collab_publish,
    collab_register,

    fn name(self: ToolKind) []const u8 {
        return @tagName(self);
    }
};

const all_tools = [_]ToolKind{
    .collab_board,
    .collab_thread,
    .collab_report,
    .ci_status,
    .collab_publish,
    .collab_register,
};

fn toolDescription(k: ToolKind) []const u8 {
    return switch (k) {
        .collab_board => "The D1 work-unit board (threads projected under the checkout's .walgit/board.toml at HEAD). Read-only: moving a card is a signed `status` entry. Reads fetch refs/collab/* unless `fresh:false`.",
        .collab_thread => "One collab thread: parent-ordered entries with per-entry verification and `broken_refs` (references this checkout cannot resolve), plus the PR view and merge-rule verdict when the thread carries a `patch`.",
        .collab_report => "Read-only dashboard over refs/collab/*: threads, PR status, verification health, CI runs and per-actor/per-kind activity.",
        .ci_status => "CI runs of the checkout (`docs/D1_CI_PROTOCOL.md` §8.3): every run the ledger carries, with attempts, live claim winner, state and effective conclusion. One run with `id`. Claim/report with signed `ci_claim`/`ci_result` entries through `collab_publish`.",
        .collab_publish => "Publish one **already signed** collab entry to refs/collab/inbox/<actor>/<uuid>. Build and sign it with your own key (`wg collab entry` does both). `actor` must be this worktree's identity principal. The signature is NOT checked on write — aggregation verifies it, so re-read the thread and confirm `verified: true`. The `status=done` gate is enforced here (the same rule as `wg collab entry`).",
        .collab_register => "Register this worktree identity's public key at refs/collab/meta/principals/<principal> and push it. Optional `force` (default false): a different already-registered key is refused unless forced, because rotating makes every past entry by that principal unverified. Identity missing => refused; run `wg collab join --principal <name>` first.",
    };
}

fn toolsList(a: std.mem.Allocator) Value {
    var arr = std.json.Array.init(a);
    for (all_tools) |k| {
        var t = jsonio.new_object();
        jsonio.obj_put(&t, a, "name", .{ .string = k.name() });
        jsonio.obj_put(&t, a, "description", .{ .string = toolDescription(k) });
        jsonio.obj_put(&t, a, "inputSchema", toolSchema(a, k));
        arr.append(.{ .object = t }) catch @panic("oom");
    }
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "tools", .{ .array = arr });
    return .{ .object = o };
}

fn toolSchema(a: std.mem.Allocator, k: ToolKind) Value {
    var props = jsonio.new_object();
    var required = std.json.Array.init(a);
    switch (k) {
        .collab_board, .collab_report => {
            putFreshProp(&props, a);
        },
        .collab_thread => {
            var idp = jsonio.new_object();
            jsonio.obj_put(&idp, a, "type", .{ .string = "string" });
            jsonio.obj_put(&idp, a, "description", .{ .string = "thread id" });
            jsonio.obj_put(&props, a, "id", .{ .object = idp });
            required.append(.{ .string = "id" }) catch @panic("oom");
            putFreshProp(&props, a);
        },
        .ci_status => {
            var idp = jsonio.new_object();
            jsonio.obj_put(&idp, a, "type", .{ .string = "string" });
            jsonio.obj_put(&idp, a, "description", .{ .string = "one run id" });
            jsonio.obj_put(&props, a, "id", .{ .object = idp });
            putFreshProp(&props, a);
        },
        .collab_publish => {
            var ep = jsonio.new_object();
            var types = std.json.Array.init(a);
            types.append(.{ .string = "object" }) catch @panic("oom");
            types.append(.{ .string = "string" }) catch @panic("oom");
            jsonio.obj_put(&ep, a, "type", .{ .array = types });
            jsonio.obj_put(&ep, a, "description", .{ .string = "the signed entry: {version, kind, id, actor, ts, parent, refs?, body, sig} (an object, or its JSON text)" });
            jsonio.obj_put(&props, a, "entry", .{ .object = ep });
            required.append(.{ .string = "entry" }) catch @panic("oom");
        },
        .collab_register => {
            var fp = jsonio.new_object();
            jsonio.obj_put(&fp, a, "type", .{ .string = "boolean" });
            jsonio.obj_put(&fp, a, "description", .{ .string = "rotate an already-registered, different key (default false). Rotation makes every past signature by this principal unverified; prefer an explicit `wg collab principal-register`." });
            jsonio.obj_put(&props, a, "force", .{ .object = fp });
        },
    }
    var schema = jsonio.new_object();
    jsonio.obj_put(&schema, a, "type", .{ .string = "object" });
    jsonio.obj_put(&schema, a, "properties", .{ .object = props });
    jsonio.obj_put(&schema, a, "required", .{ .array = required });
    jsonio.obj_put(&schema, a, "additionalProperties", .{ .bool = false });
    return .{ .object = schema };
}

fn putFreshProp(props: *std.json.ObjectMap, a: std.mem.Allocator) void {
    var fp = jsonio.new_object();
    jsonio.obj_put(&fp, a, "type", .{ .string = "boolean" });
    jsonio.obj_put(&fp, a, "description", .{ .string = "fetch refs/collab/* before reading (default true); false reads the cached refs and reports `_meta.fetched_at`. A failed fetch when true is a tool error (fail closed)." });
    jsonio.obj_put(props, a, "fresh", .{ .object = fp });
}

// ---- tool call dispatch -----------------------------------------------------------------

const Outcome = union(enum) {
    ok: struct { text: []const u8, fresh: bool, fetched_at: i64 },
    err: []const u8,
};

const ToolCall = union(enum) {
    invalid: []const u8,
    out: Outcome,
};

fn handleToolCall(s: *Server, a: std.mem.Allocator, id: Value, params: Value) void {
    const o = switch (params) {
        .object => |o| o,
        else => {
            writeRpcError(s, a, id, -32602, "`params` must be an object");
            return;
        },
    };
    const name = switch (o.get("name") orelse .null) {
        .string => |n| n,
        else => {
            writeRpcError(s, a, id, -32602, "`name` is required");
            return;
        },
    };
    const arguments: Value = o.get("arguments") orelse .{ .object = jsonio.new_object() };
    const call = callTool(s, a, name, arguments);
    switch (call) {
        .invalid => |m| writeRpcError(s, a, id, -32602, m),
        .out => |outcome| {
            var result = jsonio.new_object();
            var content = std.json.Array.init(a);
            var block = jsonio.new_object();
            jsonio.obj_put(&block, a, "type", .{ .string = "text" });
            switch (outcome) {
                .ok => |ok| {
                    jsonio.obj_put(&block, a, "text", .{ .string = ok.text });
                    content.append(.{ .object = block }) catch @panic("oom");
                    jsonio.obj_put(&result, a, "content", .{ .array = content });
                    var meta = jsonio.new_object();
                    jsonio.obj_put(&meta, a, "fresh", .{ .bool = ok.fresh });
                    if (ok.fetched_at > 0) jsonio.obj_put(&meta, a, "fetched_at", .{ .integer = ok.fetched_at });
                    jsonio.obj_put(&result, a, "_meta", .{ .object = meta });
                },
                .err => |m| {
                    jsonio.obj_put(&block, a, "text", .{ .string = m });
                    content.append(.{ .object = block }) catch @panic("oom");
                    jsonio.obj_put(&result, a, "content", .{ .array = content });
                    jsonio.obj_put(&result, a, "isError", .{ .bool = true });
                },
            }
            writeRpcResult(s, a, id, .{ .object = result });
        },
    }
}

fn callTool(s: *Server, a: std.mem.Allocator, name: []const u8, arguments: Value) ToolCall {
    var kind: ?ToolKind = null;
    for (all_tools) |k| {
        if (std.mem.eql(u8, k.name(), name)) kind = k;
    }
    const k = kind orelse return .{ .invalid = std.fmt.allocPrint(a, "unknown tool: {s}", .{name}) catch "unknown tool" };
    const o = switch (arguments) {
        .object => |o| o,
        else => return .{ .invalid = "`arguments` must be an object" },
    };
    // Strict like the host surface: an unknown argument is a request error.
    var it = o.iterator();
    while (it.next()) |e| {
        const allowed = switch (k) {
            .collab_board, .collab_report => std.mem.eql(u8, e.key_ptr.*, "fresh"),
            .collab_thread, .ci_status => std.mem.eql(u8, e.key_ptr.*, "id") or std.mem.eql(u8, e.key_ptr.*, "fresh"),
            .collab_publish => std.mem.eql(u8, e.key_ptr.*, "entry"),
            .collab_register => std.mem.eql(u8, e.key_ptr.*, "force"),
        };
        if (!allowed) {
            return .{ .invalid = std.fmt.allocPrint(a, "unknown argument `{s}` for `{s}`", .{ e.key_ptr.*, name }) catch "unknown argument" };
        }
    }
    if (o.get("fresh")) |v| {
        if (v != .bool) return .{ .invalid = "`fresh` must be a boolean" };
    }
    if (o.get("force")) |v| {
        if (v != .bool) return .{ .invalid = "`force` must be a boolean" };
    }
    const fresh = freshFor(s, o);
    const force = switch (o.get("force") orelse .null) {
        .bool => |b| b,
        else => false,
    };
    s.git_mutex.lockUncancelable(s.c.io);
    defer s.git_mutex.unlock(s.c.io);
    return switch (k) {
        .collab_board => toolBoard(s, a, fresh),
        .collab_thread => toolThread(s, a, o, fresh),
        .collab_report => toolReport(s, a, fresh),
        .ci_status => toolCiStatus(s, a, o, fresh),
        .collab_publish => toolPublish(s, a, o),
        .collab_register => toolRegister(s, a, force),
    };
}

fn freshFor(s: *Server, o: std.json.ObjectMap) bool {
    // `--no-fetch` wins over any per-call `fresh:true` (SKILL §7 says
    // process-wide, so it is a gate, not a default).
    if (s.no_fetch) return false;
    if (o.get("fresh")) |v| {
        return switch (v) {
            .bool => |b| b,
            else => s.fresh_default,
        };
    }
    return s.fresh_default;
}

fn maybeFetch(s: *Server, a: std.mem.Allocator, fresh: bool) ?[]const u8 {
    if (!fresh) return null;
    gitx.fetch_collab_bounded(a, s.c.io, s.repo, s.remote) catch |e| {
        const detail = gitErrorDetail(e);
        return std.fmt.allocPrint(a, "cannot read remote {s} ({s}): {s} — fail closed; pass fresh:false to read the cached refs", .{ s.remote, @errorName(e), detail }) catch "fetch failed";
    };
    s.last_fetch.store(util.now_ts(), .release);
    return null;
}

fn fetchedAt(s: *Server) i64 {
    return s.last_fetch.load(.acquire);
}

/// A git failure as a tool message. A bounded fetch/push that ran out of
/// time has no stderr to quote (`git` was killed), so it gets its own text.
fn gitErrorDetail(e: anyerror) []const u8 {
    return switch (e) {
        error.Timeout => "timed out waiting for the remote (ref fetches and pushes are bounded)",
        error.GitFailed => if (gitx.last_stderr.len > 0) gitx.last_stderr else "git failed",
        else => @errorName(e),
    };
}

fn toolBoard(s: *Server, a: std.mem.Allocator, fresh: bool) ToolCall {
    if (maybeFetch(s, a, fresh)) |m| return .{ .out = .{ .err = m } };
    const agg = loadAgg(s, a) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const board = collab.build_board(a, agg.loaded.entries, agg.loaded.principals, &agg.rules, &agg.board_def, util.now_ts()) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const compact = board.to_json(a);
    const text = prettyFromJson(a, compact) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    return .{ .out = .{ .ok = .{ .text = text, .fresh = fresh, .fetched_at = fetchedAt(s) } } };
}

fn toolThread(s: *Server, a: std.mem.Allocator, o: std.json.ObjectMap, fresh: bool) ToolCall {
    const id = switch (o.get("id") orelse .null) {
        .string => |x| x,
        else => return .{ .invalid = "`id` is required" },
    };
    if (id.len == 0) return .{ .invalid = "`id` must not be empty" };
    if (maybeFetch(s, a, fresh)) |m| return .{ .out = .{ .err = m } };
    const agg = loadAgg(s, a) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const v = threadValue(a, id, agg.loaded.entries, agg.loaded.principals, &agg.rules) catch |e| switch (e) {
        error.NoThread => return .{ .out = .{ .err = std.fmt.allocPrint(a, "no collab thread {s}", .{id}) catch "no such thread" } },
        else => return .{ .out = .{ .err = aggError(a, e) } },
    };
    return .{ .out = .{ .ok = .{ .text = jsonio.to_string_pretty(a, v), .fresh = fresh, .fetched_at = fetchedAt(s) } } };
}

fn toolReport(s: *Server, a: std.mem.Allocator, fresh: bool) ToolCall {
    if (maybeFetch(s, a, fresh)) |m| return .{ .out = .{ .err = m } };
    const agg = loadAgg(s, a) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const report = collab.build_report(a, agg.loaded.entries, agg.loaded.principals, &agg.rules, util.now_ts()) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const v = reportValue(a, &report) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    return .{ .out = .{ .ok = .{ .text = jsonio.to_string_pretty(a, v), .fresh = fresh, .fetched_at = fetchedAt(s) } } };
}

fn toolCiStatus(s: *Server, a: std.mem.Allocator, o: std.json.ObjectMap, fresh: bool) ToolCall {
    const want_id: ?[]const u8 = switch (o.get("id") orelse .null) {
        .string => |x| x,
        .null => null,
        else => return .{ .invalid = "`id` must be a string" },
    };
    if (maybeFetch(s, a, fresh)) |m| return .{ .out = .{ .err = m } };
    const agg = loadAgg(s, a) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const ci_slice = ci.ci_entries(a, agg.loaded.entries) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const runs = ci.collect_runs(a, ci_slice, agg.loaded.principals, util.now_ts()) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const repo_id = repoId(s, a);
    const compact = ciStatusJson(a, repo_id, runs, want_id) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    const text = prettyFromJson(a, compact) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
    return .{ .out = .{ .ok = .{ .text = text, .fresh = fresh, .fetched_at = fetchedAt(s) } } };
}

/// `{"repo": "<owner>/<repo>", "runs": [RunView…]}` — the host
/// `GET …/api-browser/collab/ci[?id=]` shape, not the `wg ci status` wrapper
/// (which carries `now` instead of `repo`).
fn ciStatusJson(a: std.mem.Allocator, repo_id: []const u8, runs: []const ci.RunEntry, want_id: ?[]const u8) ![]const u8 {
    var runs_json = util.List(u8).init(a);
    runs_json.append('[') catch @panic("oom");
    var first = true;
    for (runs) |kv| {
        if (want_id) |id| {
            if (!std.mem.eql(u8, kv.id, id)) continue;
        }
        if (!first) runs_json.append(',') catch @panic("oom");
        first = false;
        const one = try cmd_ci.run_view_json(a, &kv.view);
        runs_json.append_slice(one) catch @panic("oom");
    }
    runs_json.append(']') catch @panic("oom");
    var out = util.List(u8).init(a);
    out.append_slice("{\"repo\":") catch @panic("oom");
    jsonio.write_string(&out, repo_id);
    out.append_slice(",\"runs\":") catch @panic("oom");
    out.append_slice(runs_json.items) catch @panic("oom");
    out.append('}') catch @panic("oom");
    return out.items;
}

fn aggError(a: std.mem.Allocator, e: anyerror) []const u8 {
    return std.fmt.allocPrint(a, "collaboration aggregation failed: {t}", .{e}) catch "collaboration aggregation failed";
}

fn prettyFromJson(a: std.mem.Allocator, bytes: []const u8) ![]const u8 {
    const v = try std.json.parseFromSliceLeaky(Value, a, bytes, .{});
    return jsonio.to_string_pretty(a, v);
}

// ---- aggregation ------------------------------------------------------------------------

const Agg = struct {
    loaded: collabreader.Loaded,
    rules: collab.MergeRules,
    board_def: collab.BoardDef,
};

fn loadAgg(s: *Server, a: std.mem.Allocator) !Agg {
    const reader = collabreader.Reader.init(a, s.c.io, s.repo);
    const loaded = try reader.load(a);
    const rules = collabreader.load_merge_rules(a, s.c.io, s.repo, null) catch |e| {
        std.debug.print("wg mcp: merge rules document is invalid — fail closed ({t})\n", .{e});
        return error.BadRules;
    };
    const board_def = collabreader.load_board_def(a, s.c.io, s.repo, null) catch |e| {
        std.debug.print("wg mcp: board definition is invalid — fail closed ({t})\n", .{e});
        return error.BadBoard;
    };
    return .{ .loaded = loaded, .rules = rules, .board_def = board_def };
}

/// `{id, entries:[{oid, principal, verified, entry, broken_refs}], pr}` —
/// exactly the host `GET …/api-browser/collab/threads/{id}` shape.
fn threadValue(a: std.mem.Allocator, id: []const u8, entries: []const entry_mod.EntryRef, principals: *const entry_mod.Principals, rules: *const collab.MergeRules) !Value {
    var filtered = util.List(entry_mod.EntryRef).init(a);
    for (entries) |e| {
        if (std.mem.eql(u8, e.entry.id, id)) try filtered.append(e);
    }
    if (filtered.items.len == 0) return error.NoThread;
    const ordered = try collab.thread(a, filtered.items);

    var known = std.StringHashMap(void).init(a);
    for (entries) |e| known.put(e.oid, {}) catch @panic("oom");

    var entry_values = std.json.Array.init(a);
    for (ordered) |r| {
        var broken = std.json.Array.init(a);
        const referenced = referencedOids(a, r.entry.body);
        for (referenced) |oid| {
            if (!known.contains(oid)) broken.append(.{ .string = oid }) catch @panic("oom");
        }
        var eo = jsonio.new_object();
        jsonio.obj_put(&eo, a, "oid", .{ .string = r.oid });
        jsonio.obj_put(&eo, a, "principal", .{ .string = r.principal });
        jsonio.obj_put(&eo, a, "verified", .{ .bool = r.is_verified(a, principals) });
        jsonio.obj_put(&eo, a, "entry", cmd_collab.entry_value(a, &r.entry));
        jsonio.obj_put(&eo, a, "broken_refs", .{ .array = broken });
        entry_values.append(.{ .object = eo }) catch @panic("oom");
    }

    var pr_value: Value = .null;
    var has_patch = false;
    for (filtered.items) |e| {
        if (std.mem.eql(u8, e.entry.kind, "patch")) has_patch = true;
    }
    if (has_patch) {
        const pr = try collab.pr_view(a, filtered.items, principals);
        const eval = try collab.merge_rule_eval(a, rules, &pr);
        pr_value = .{ .object = try prMergeValue(a, &pr, &eval) };
    }

    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "id", .{ .string = id });
    jsonio.obj_put(&o, a, "entries", .{ .array = entry_values });
    jsonio.obj_put(&o, a, "pr", pr_value);
    return .{ .object = o };
}

/// Rust's `referenced_oids`: `related` then `depends_on`, arrays of strings in
/// document order.
fn referencedOids(a: std.mem.Allocator, body: Value) []const []const u8 {
    var out = util.List([]const u8).init(a);
    const o = switch (body) {
        .object => |o| o,
        else => return out.items,
    };
    for ([_][]const u8{ "related", "depends_on" }) |key| {
        const v = o.get(key) orelse continue;
        const arr = switch (v) {
            .array => |x| x,
            else => continue,
        };
        for (arr.items) |item| {
            switch (item) {
                .string => |s| out.append(s) catch @panic("oom"),
                else => {},
            }
        }
    }
    return out.items;
}

fn prMergeValue(a: std.mem.Allocator, pr: *const collab.PrView, eval: *const collab.MergeEval) !std.json.ObjectMap {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "pr", try prValue(a, pr));
    jsonio.obj_put(&o, a, "merge", try mergeEvalValue(a, eval));
    return o;
}

fn prValue(a: std.mem.Allocator, pr: *const collab.PrView) !Value {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "id", .{ .string = pr.id });
    jsonio.obj_put(&o, a, "base", if (pr.base) |b| .{ .string = b } else .null);
    jsonio.obj_put(&o, a, "head", if (pr.head) |h| .{ .string = h } else .null);
    jsonio.obj_put(&o, a, "status", .{ .string = pr.status });
    jsonio.obj_put(&o, a, "reviews", try reviewsValue(a, pr.reviews));
    jsonio.obj_put(&o, a, "human_approvals", try reviewsValue(a, pr.human_approvals));
    jsonio.obj_put(&o, a, "authors", try stringArray(a, pr.authors));
    jsonio.obj_put(&o, a, "unverified", try stringArray(a, pr.unverified));
    return .{ .object = o };
}

fn reviewsValue(a: std.mem.Allocator, reviews: []const collab.Review) !Value {
    var arr = std.json.Array.init(a);
    for (reviews) |r| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "actor", .{ .string = r.actor });
        jsonio.obj_put(&o, a, "decision", .{ .string = r.decision });
        jsonio.obj_put(&o, a, "ts", .{ .integer = r.ts });
        jsonio.obj_put(&o, a, "oid", .{ .string = r.oid });
        arr.append(.{ .object = o }) catch @panic("oom");
    }
    return .{ .array = arr };
}

fn stringArray(a: std.mem.Allocator, items: []const []const u8) !Value {
    var arr = std.json.Array.init(a);
    for (items) |s| arr.append(.{ .string = s }) catch @panic("oom");
    return .{ .array = arr };
}

fn mergeEvalValue(a: std.mem.Allocator, eval: *const collab.MergeEval) !Value {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "allowed", .{ .bool = eval.allowed });
    jsonio.obj_put(&o, a, "reason", .{ .string = eval.reason });
    jsonio.obj_put(&o, a, "satisfied_by", try stringArray(a, eval.satisfied_by));
    return .{ .object = o };
}

/// The host report shape: `by_actor` / `by_kind` are `[[key, n], …]` pairs
/// (Rust's `Vec<(String, usize)>`), not objects.
fn reportValue(a: std.mem.Allocator, r: *const collab.Report) !Value {
    var threads = std.json.Array.init(a);
    for (r.threads) |t| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "id", .{ .string = t.id });
        jsonio.obj_put(&o, a, "title", .{ .string = t.title });
        jsonio.obj_put(&o, a, "entries", .{ .integer = @intCast(t.entries) });
        jsonio.obj_put(&o, a, "verified", .{ .integer = @intCast(t.verified) });
        jsonio.obj_put(&o, a, "last_ts", .{ .integer = t.last_ts });
        jsonio.obj_put(&o, a, "kinds", try stringArray(a, t.kinds));
        threads.append(.{ .object = o }) catch @panic("oom");
    }
    var prs = std.json.Array.init(a);
    for (r.prs) |p| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "id", .{ .string = p.id });
        jsonio.obj_put(&o, a, "title", .{ .string = p.title });
        jsonio.obj_put(&o, a, "base", if (p.base) |b| .{ .string = b } else .null);
        jsonio.obj_put(&o, a, "head", if (p.head) |h| .{ .string = h } else .null);
        jsonio.obj_put(&o, a, "status", .{ .string = p.status });
        jsonio.obj_put(&o, a, "approvals", .{ .integer = @intCast(p.approvals) });
        jsonio.obj_put(&o, a, "merge_allowed", .{ .bool = p.merge_allowed });
        jsonio.obj_put(&o, a, "merge_reason", .{ .string = p.merge_reason });
        prs.append(.{ .object = o }) catch @panic("oom");
    }
    var runs = std.json.Array.init(a);
    for (r.runs) |run_item| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "id", .{ .string = run_item.id });
        jsonio.obj_put(&o, a, "task", .{ .string = run_item.task });
        jsonio.obj_put(&o, a, "repo_ref", .{ .string = run_item.repo_ref });
        jsonio.obj_put(&o, a, "commit", .{ .string = run_item.commit });
        jsonio.obj_put(&o, a, "state", .{ .string = run_item.state });
        jsonio.obj_put(&o, a, "conclusion", if (run_item.conclusion) |c| .{ .string = c } else .null);
        jsonio.obj_put(&o, a, "runner", if (run_item.runner) |x| .{ .string = x } else .null);
        jsonio.obj_put(&o, a, "claims", .{ .integer = @intCast(run_item.claims) });
        jsonio.obj_put(&o, a, "last_ts", .{ .integer = run_item.last_ts });
        runs.append(.{ .object = o }) catch @panic("oom");
    }
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "threads", .{ .array = threads });
    jsonio.obj_put(&o, a, "prs", .{ .array = prs });
    jsonio.obj_put(&o, a, "runs", .{ .array = runs });
    jsonio.obj_put(&o, a, "total_entries", .{ .integer = @intCast(r.total_entries) });
    jsonio.obj_put(&o, a, "verified_entries", .{ .integer = @intCast(r.verified_entries) });
    jsonio.obj_put(&o, a, "unverified_entries", .{ .integer = @intCast(r.unverified_entries) });
    jsonio.obj_put(&o, a, "missing_principals", .{ .integer = @intCast(r.missing_principals) });
    jsonio.obj_put(&o, a, "by_actor", try countsValue(a, r.by_actor));
    jsonio.obj_put(&o, a, "by_kind", try countsValue(a, r.by_kind));
    return .{ .object = o };
}

fn countsValue(a: std.mem.Allocator, counts: []const collab.Report.Count) !Value {
    var arr = std.json.Array.init(a);
    for (counts) |c| {
        var pair = std.json.Array.init(a);
        pair.append(.{ .string = c.key }) catch @panic("oom");
        pair.append(.{ .integer = @intCast(c.n) }) catch @panic("oom");
        arr.append(.{ .array = pair }) catch @panic("oom");
    }
    return .{ .array = arr };
}

/// `owner/repo` as the host's CI endpoint reports it, from the checkout's
/// remote URL. A local-path remote has no owner: fall back to the directory
/// basename so the field is never empty.
fn repoId(s: *Server, a: std.mem.Allocator) []const u8 {
    const url = gitx.remote_url(a, s.c.io, s.repo, s.remote) catch return baseName(s.repo);
    return repoIdFromUrl(a, url) catch baseName(s.repo);
}

fn repoIdFromUrl(a: std.mem.Allocator, url: []const u8) ![]const u8 {
    const no_git = if (std.mem.endsWith(u8, url, ".git")) url[0 .. url.len - 4] else url;
    var path: []const u8 = no_git;
    if (std.mem.indexOf(u8, no_git, "://")) |i| {
        const rest = no_git[i + 3 ..];
        path = if (std.mem.indexOfScalar(u8, rest, '/')) |slash| rest[slash + 1 ..] else return error.NoPath;
    } else if (std.mem.indexOfScalar(u8, no_git, ':')) |colon| {
        path = no_git[colon + 1 ..];
    } else {
        // A plain local path (`/tmp/origin.git`, `../origin.git`) carries no
        // owner: the directory basename is the most honest answer.
        return std.fmt.allocPrint(a, "{s}", .{baseName(no_git)});
    }
    var parts = std.mem.splitScalar(u8, std.mem.trim(u8, path, "/"), '/');
    var last: ?[]const u8 = null;
    var prev: ?[]const u8 = null;
    while (parts.next()) |p| {
        if (p.len == 0) continue;
        prev = last;
        last = p;
    }
    if (last == null) return error.NoPath;
    if (prev) |p| return std.fmt.allocPrint(a, "{s}/{s}", .{ p, last.? });
    return std.fmt.allocPrint(a, "{s}", .{last.?});
}

fn baseName(path: []const u8) []const u8 {
    const trimmed = std.mem.trimEnd(u8, path, "/");
    if (std.mem.lastIndexOfScalar(u8, trimmed, '/')) |i| return trimmed[i + 1 ..];
    return trimmed;
}

// ---- resources --------------------------------------------------------------------------

const UriKind = union(enum) {
    board,
    thread: []const u8,
};

fn parseResourceUri(a: std.mem.Allocator, uri: []const u8) ?UriKind {
    if (std.mem.eql(u8, uri, "walgit://collab/board")) return .board;
    const prefix = "walgit://collab/thread/";
    if (std.mem.startsWith(u8, uri, prefix)) {
        const raw = uri[prefix.len..];
        if (raw.len == 0) return null;
        return .{ .thread = percentDecode(a, raw) orelse return null };
    }
    return null;
}

fn hexDigit(c: u8) ?u8 {
    return switch (c) {
        '0'...'9' => c - '0',
        'a'...'f' => c - 'a' + 10,
        'A'...'F' => c - 'A' + 10,
        else => null,
    };
}

/// Decode `%XX` escapes in a resource URI path segment (a generic client
/// percent-encodes `@`, `-`, etc.). `+` is not a space in a path. An
/// incomplete or non-hex escape is not the resource it appears to be: `null`,
/// which the callers turn into "not a wg collab resource URI".
fn percentDecode(a: std.mem.Allocator, s: []const u8) ?[]const u8 {
    if (std.mem.indexOfScalar(u8, s, '%') == null) return s;
    var out = util.List(u8).init(a);
    var i: usize = 0;
    while (i < s.len) {
        if (s[i] == '%') {
            if (i + 2 >= s.len) return null;
            const hi = hexDigit(s[i + 1]) orelse return null;
            const lo = hexDigit(s[i + 2]) orelse return null;
            out.append((hi << 4) | lo) catch return null;
            i += 3;
        } else {
            out.append(s[i]) catch return null;
            i += 1;
        }
    }
    return out.items;
}

fn resourcesList(s: *Server, a: std.mem.Allocator) !Value {
    _ = s;
    // The index is static (the board resource); reading it touches no refs and
    // needs no fetch. `resources/read` and `subscribe` do fetch.
    var arr = std.json.Array.init(a);
    var r = jsonio.new_object();
    jsonio.obj_put(&r, a, "uri", .{ .string = "walgit://collab/board" });
    jsonio.obj_put(&r, a, "name", .{ .string = "collaboration board" });
    jsonio.obj_put(&r, a, "description", .{ .string = "The D1 board projection over refs/collab/* (this checkout)" });
    jsonio.obj_put(&r, a, "mimeType", .{ .string = "application/json" });
    arr.append(.{ .object = r }) catch @panic("oom");
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "resources", .{ .array = arr });
    return .{ .object = o };
}

fn templatesList(a: std.mem.Allocator) Value {
    var arr = std.json.Array.init(a);
    var board = jsonio.new_object();
    jsonio.obj_put(&board, a, "uriTemplate", .{ .string = "walgit://collab/board" });
    jsonio.obj_put(&board, a, "name", .{ .string = "board" });
    jsonio.obj_put(&board, a, "description", .{ .string = "The D1 board projection over refs/collab/*" });
    jsonio.obj_put(&board, a, "mimeType", .{ .string = "application/json" });
    arr.append(.{ .object = board }) catch @panic("oom");
    var thread = jsonio.new_object();
    jsonio.obj_put(&thread, a, "uriTemplate", .{ .string = "walgit://collab/thread/{thread}" });
    jsonio.obj_put(&thread, a, "name", .{ .string = "thread" });
    jsonio.obj_put(&thread, a, "description", .{ .string = "One thread: its signed entries, projected status and reviews" });
    jsonio.obj_put(&thread, a, "mimeType", .{ .string = "application/json" });
    arr.append(.{ .object = thread }) catch @panic("oom");
    var o = jsonio.new_object();
    jsonio.obj_put(&o, a, "resourceTemplates", .{ .array = arr });
    return .{ .object = o };
}

const ReadAnswer = struct {
    body: []const u8,
    version: []const u8,
    fresh: bool,
};

fn resourceRead(s: *Server, a: std.mem.Allocator, uri: []const u8, fresh: bool) !ReadAnswer {
    const kind = parseResourceUri(a, uri) orelse return error.BadUri;
    if (maybeFetch(s, a, fresh)) |m| {
        // `maybeFetch` returns an owned message; carry it as an error the
        // handler can print verbatim.
        std.debug.print("wg mcp: {s}\n", .{m});
        return error.FetchFailed;
    }
    const agg = try loadAgg(s, a);
    const now = util.now_ts();
    const board = try collab.build_board(a, agg.loaded.entries, agg.loaded.principals, &agg.rules, &agg.board_def, now);
    switch (kind) {
        .board => {
            const text = try prettyFromJson(a, board.to_json(a));
            return .{ .body = text, .version = try boardVersion(a, &board, now), .fresh = fresh };
        },
        .thread => |id| {
            const v = threadValue(a, id, agg.loaded.entries, agg.loaded.principals, &agg.rules) catch |e| switch (e) {
                error.NoThread => return error.NoThread,
                else => return e,
            };
            return .{
                .body = jsonio.to_string_pretty(a, v),
                .version = try threadVersion(a, &board, id),
                .fresh = fresh,
            };
        },
    }
}

/// The host's board version: `b:<witness>[:claim:<earliest live expiry>]`.
/// The witness is `SipHash-1-3` over (column index, id, last_oid, status,
/// owner) per card — the same byte stream the host's `DefaultHasher` sees.
fn boardVersion(a: std.mem.Allocator, board: *const collab.Board, now: i64) ![]const u8 {
    var h = Sip13.init();
    var earliest: ?i64 = null;
    for (board.columns, 0..) |col, column_index| {
        h.writeUsize(column_index);
        for (col.cards) |card| {
            h.writeStr(card.id);
            h.writeStr(card.last_oid);
            h.writeStr(card.status);
            h.writeStr(card.owner);
            if (card.claim) |claim| {
                if (claim.expires_at >= now) {
                    earliest = if (earliest) |e| @min(e, claim.expires_at) else claim.expires_at;
                }
            }
        }
    }
    const witness = h.finish();
    if (earliest) |e| return std.fmt.allocPrint(a, "b:{x:0>16}:claim:{d}", .{ witness, e });
    return std.fmt.allocPrint(a, "b:{x:0>16}", .{witness});
}

/// The host's thread version: that thread's own tip (`h:<oid>`), or `h:-`
/// while the board does not carry it.
fn threadVersion(a: std.mem.Allocator, board: *const collab.Board, id: []const u8) ![]const u8 {
    for (board.columns) |col| {
        for (col.cards) |card| {
            if (std.mem.eql(u8, card.id, id)) return std.fmt.allocPrint(a, "h:{s}", .{card.last_oid});
        }
    }
    return std.fmt.allocPrint(a, "h:-", .{});
}

fn handleResourceRead(s: *Server, a: std.mem.Allocator, id: Value, params: Value) void {
    const uri = switch (params) {
        .object => |o| switch (o.get("uri") orelse .null) {
            .string => |u| u,
            else => {
                writeRpcError(s, a, id, -32602, "`uri` is required");
                return;
            },
        },
        else => {
            writeRpcError(s, a, id, -32602, "`params` must be an object");
            return;
        },
    };
    s.git_mutex.lockUncancelable(s.c.io);
    const answer = resourceRead(s, a, uri, s.fresh_default);
    s.git_mutex.unlock(s.c.io);
    const ans = answer catch |e| {
        switch (e) {
            error.BadUri => writeRpcError(s, a, id, -32602, "not a wg collab resource URI"),
            error.NoThread => writeRpcError(s, a, id, -32602, "no such collab thread"),
            error.FetchFailed => writeRpcError(s, a, id, -32603, "cannot fetch refs/collab/* — fail closed (start `wg mcp --no-fetch` to read the cached refs)"),
            else => writeRpcError(s, a, id, -32603, std.fmt.allocPrint(a, "resource read failed: {t}", .{e}) catch "resource read failed"),
        }
        return;
    };
    var contents = std.json.Array.init(a);
    var c = jsonio.new_object();
    jsonio.obj_put(&c, a, "uri", .{ .string = uri });
    jsonio.obj_put(&c, a, "mimeType", .{ .string = "application/json" });
    jsonio.obj_put(&c, a, "text", .{ .string = ans.body });
    var meta = jsonio.new_object();
    jsonio.obj_put(&meta, a, "version", .{ .string = ans.version });
    const at = fetchedAt(s);
    if (at > 0) jsonio.obj_put(&meta, a, "fetched_at", .{ .integer = at });
    jsonio.obj_put(&c, a, "_meta", .{ .object = meta });
    contents.append(.{ .object = c }) catch @panic("oom");
    var result = jsonio.new_object();
    jsonio.obj_put(&result, a, "contents", .{ .array = contents });
    writeRpcResult(s, a, id, .{ .object = result });
}

fn handleSubscribe(s: *Server, a: std.mem.Allocator, id: Value, params: Value) void {
    const uri = switch (params) {
        .object => |o| switch (o.get("uri") orelse .null) {
            .string => |u| u,
            else => {
                writeRpcError(s, a, id, -32602, "`uri` is required");
                return;
            },
        },
        else => {
            writeRpcError(s, a, id, -32602, "`params` must be an object");
            return;
        },
    };
    if (parseResourceUri(a, uri) == null) {
        writeRpcError(s, a, id, -32602, "not a wg collab resource URI");
        return;
    }
    s.git_mutex.lockUncancelable(s.c.io);
    // The initial version is a read: it follows the same fresh policy and
    // fails closed, exactly like a `resources/read`.
    const answer = resourceRead(s, a, uri, s.fresh_default);
    s.git_mutex.unlock(s.c.io);
    const ans = answer catch |e| {
        switch (e) {
            error.NoThread => writeRpcError(s, a, id, -32602, "no such collab thread"),
            error.FetchFailed => writeRpcError(s, a, id, -32603, "cannot fetch refs/collab/* — fail closed (start `wg mcp --no-fetch` to read the cached refs)"),
            else => writeRpcError(s, a, id, -32603, std.fmt.allocPrint(a, "subscribe failed: {t}", .{e}) catch "subscribe failed"),
        }
        return;
    };
    s.subs_mutex.lockUncancelable(s.c.io);
    defer s.subs_mutex.unlock(s.c.io);
    for (s.subs.items) |sub| {
        if (std.mem.eql(u8, sub.uri, uri)) {
            writeRpcResult(s, a, id, .{ .object = jsonio.new_object() });
            return;
        }
    }
    if (s.subs.items.len >= s.max_subs) {
        writeRpcError(s, a, id, -32602, std.fmt.allocPrint(a, "at most {d} subscriptions (--max-subscriptions)", .{s.max_subs}) catch "too many subscriptions");
        return;
    }
    const owned_uri = s.c.alloc.dupe(u8, uri) catch {
        writeRpcError(s, a, id, -32603, "out of memory");
        return;
    };
    const owned_version = s.c.alloc.dupe(u8, ans.version) catch {
        writeRpcError(s, a, id, -32603, "out of memory");
        return;
    };
    s.subs.append(.{ .uri = owned_uri, .version = owned_version }) catch {
        writeRpcError(s, a, id, -32603, "out of memory");
        return;
    };
    writeRpcResult(s, a, id, .{ .object = jsonio.new_object() });
}

fn handleUnsubscribe(s: *Server, a: std.mem.Allocator, id: Value, params: Value) void {
    const uri = switch (params) {
        .object => |o| switch (o.get("uri") orelse .null) {
            .string => |u| u,
            else => {
                writeRpcError(s, a, id, -32602, "`uri` is required");
                return;
            },
        },
        else => {
            writeRpcError(s, a, id, -32602, "`params` must be an object");
            return;
        },
    };
    s.subs_mutex.lockUncancelable(s.c.io);
    var i: usize = 0;
    while (i < s.subs.items.len) {
        if (std.mem.eql(u8, s.subs.items[i].uri, uri)) {
            _ = s.subs.items[i];
            var j = i;
            while (j + 1 < s.subs.items.len) : (j += 1) s.subs.items[j] = s.subs.items[j + 1];
            s.subs.items = s.subs.items[0 .. s.subs.items.len - 1];
            break;
        }
        i += 1;
    }
    s.subs_mutex.unlock(s.c.io);
    writeRpcResult(s, a, id, .{ .object = jsonio.new_object() });
}

// ---- subscription poller ----------------------------------------------------------------

fn pollLoop(s: *Server) void {
    while (!s.stop.load(.acquire)) {
        std.Io.sleep(s.c.io, Io.Duration.fromMilliseconds(@intCast(s.interval_ms)), .awake) catch {};
        if (s.stop.load(.acquire)) break;
        pollTick(s);
    }
}

fn pollTick(s: *Server) void {
    // No subscribers: an idle stdio server must not fetch the remote every
    // tick. A subscription added right after this check is covered next tick.
    s.subs_mutex.lockUncancelable(s.c.io);
    const idle = s.subs.items.len == 0;
    s.subs_mutex.unlock(s.c.io);
    if (idle) return;

    var arena_state = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();

    s.git_mutex.lockUncancelable(s.c.io);
    defer s.git_mutex.unlock(s.c.io);
    if (s.fresh_default) {
        // Best-effort for a poller: a failed fetch is retried next tick; the
        // versions then simply do not move (no notification, never a bogus one).
        if (maybeFetch(s, a, true)) |_| {}
    }

    // Build the projection once per tick, whatever the number of
    // subscriptions: two subscriptions on one repository must not each pay
    // the whole aggregation.
    const now = util.now_ts();
    const board_opt: ?collab.Board = blk: {
        const agg = loadAgg(s, a) catch break :blk null;
        const b = collab.build_board(a, agg.loaded.entries, agg.loaded.principals, &agg.rules, &agg.board_def, now) catch break :blk null;
        break :blk b;
    };

    s.subs_mutex.lockUncancelable(s.c.io);
    var notify = util.List([]const u8).init(a);
    var drop = util.List([]const u8).init(a);
    var drop_list_changed = false;
    if (board_opt) |board| {
        var i: usize = 0;
        while (i < s.subs.items.len) {
            const sub = &s.subs.items[i];
            const kind = parseResourceUri(a, sub.uri) orelse {
                drop.append(sub.uri) catch @panic("oom");
                i += 1;
                continue;
            };
            const v = switch (kind) {
                .board => boardVersion(a, &board, now) catch {
                    drop.append(sub.uri) catch @panic("oom");
                    drop_list_changed = true;
                    i += 1;
                    continue;
                },
                .thread => |id| threadVersion(a, &board, id) catch {
                    drop.append(sub.uri) catch @panic("oom");
                    drop_list_changed = true;
                    i += 1;
                    continue;
                },
            };
            if (!std.mem.eql(u8, v, sub.version)) {
                notify.append(sub.uri) catch @panic("oom");
                sub.version = s.c.alloc.dupe(u8, v) catch sub.version;
            }
            i += 1;
        }
    } else {
        for (s.subs.items) |sub| drop.append(sub.uri) catch @panic("oom");
        drop_list_changed = true;
    }
    // Drop failed subscriptions (in-place compaction: the registry lives in
    // the process arena, never in this tick's temporary one).
    if (drop.items.len > 0) {
        var write: usize = 0;
        for (s.subs.items) |sub| {
            var gone = false;
            for (drop.items) |d| {
                if (std.mem.eql(u8, d, sub.uri)) gone = true;
            }
            if (!gone) {
                s.subs.items[write] = sub;
                write += 1;
            }
        }
        s.subs.items = s.subs.items[0..write];
    }
    s.subs_mutex.unlock(s.c.io);

    for (notify.items) |uri| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "jsonrpc", .{ .string = "2.0" });
        jsonio.obj_put(&o, a, "method", .{ .string = "notifications/resources/updated" });
        var params = jsonio.new_object();
        jsonio.obj_put(&params, a, "uri", .{ .string = uri });
        jsonio.obj_put(&o, a, "params", .{ .object = params });
        s.writeLine(a, jsonio.to_string(a, .{ .object = o }));
    }
    if (drop_list_changed) {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, a, "jsonrpc", .{ .string = "2.0" });
        jsonio.obj_put(&o, a, "method", .{ .string = "notifications/resources/list_changed" });
        jsonio.obj_put(&o, a, "params", .{ .object = jsonio.new_object() });
        s.writeLine(a, jsonio.to_string(a, .{ .object = o }));
    }
}

// ---- write tools ------------------------------------------------------------------------

fn toolPublish(s: *Server, a: std.mem.Allocator, o: std.json.ObjectMap) ToolCall {
    const entry_arg = o.get("entry") orelse .null;
    const entry_value_js = switch (entry_arg) {
        .object => entry_arg,
        .string => |text| std.json.parseFromSliceLeaky(Value, a, text, .{}) catch {
            return .{ .invalid = "`entry` is not JSON" };
        },
        else => return .{ .invalid = "`entry` must be an object or its JSON text" },
    };
    // Preserve the client's bytes when it sent the signed entry as text: the
    // blob oid must equal the one `wg collab entry` wrote, or the same entry
    // is duplicated (and any `related`/`depends_on` citing that oid breaks).
    const blob_bytes: []const u8 = switch (entry_arg) {
        .string => |text| text,
        .object => jsonio.to_string(a, entry_value_js),
        else => unreachable,
    };

    const ident = localIdentity(s, a) catch |e| {
        return .{ .out = .{ .err = identityError(a, e) } };
    };
    const parsed = validatePublishEntry(a, entry_value_js, ident.principal) catch |e| {
        return .{ .out = .{ .err = publishErrorText(a, e) } };
    };

    const kind = parsed.kind;
    const body = parsed.body;
    // The done gate (§7.5): identical to `wg collab entry` / the thin API.
    if (std.mem.eql(u8, kind, "status")) {
        const st = switch (body) {
            .object => |bo| switch (bo.get("status") orelse .null) {
                .string => |x| x,
                else => "",
            },
            else => "",
        };
        if (std.mem.eql(u8, st, "done")) {
            gitx.fetch_collab_bounded(a, s.c.io, s.repo, s.remote) catch |e| {
                const detail = gitErrorDetail(e);
                return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot fetch refs/collab/* before the done gate ({s}): {s}", .{ @errorName(e), detail }) catch "cannot fetch before the done gate" } };
            };
            const reader = collabreader.Reader.init(a, s.c.io, s.repo);
            const loaded = reader.load(a) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
            var group = util.List(entry_mod.EntryRef).init(a);
            for (loaded.entries) |er| {
                if (std.mem.eql(u8, er.entry.id, parsed.id)) group.append(er) catch @panic("oom");
            }
            const ordered = collab.thread(a, group.items) catch |e| return .{ .out = .{ .err = aggError(a, e) } };
            collab.validate_status_transition(a, ordered, loaded.principals) catch {
                return .{ .out = .{ .err = "status transition rejected: transition → done requires the thread at needs-review with a verified non-author approve (D1_PROTOCOL §7.5); if the reviewer's key lives only in the host registry, run `wg collab principal-fetch` and retry" } };
            };
        }
    }

    if (blob_bytes.len > entry_mod.COLLAB_ENTRY_MAX_BYTES) {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "entry is {d} bytes; the cap is {d} KiB (D1_PROTOCOL §13)", .{ blob_bytes.len, entry_mod.COLLAB_ENTRY_MAX_BYTES / 1024 }) catch "entry too big" } };
    }
    const actor = parsed.actor;
    const oid = gitx.write_blob(a, s.c.io, s.repo, blob_bytes) catch |e| {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot write the entry blob: {t}", .{e}) catch "cannot write the entry blob" } };
    };
    const ref_name = std.fmt.allocPrint(a, "refs/collab/inbox/{s}/{s}", .{ actor, identity.entry_uuid(a, s.c.io) }) catch @panic("oom");
    const spec = std.fmt.allocPrint(a, "{s}:{s}", .{ oid, ref_name }) catch @panic("oom");
    gitx.push(a, s.c.io, s.repo, s.remote, .{ .refspecs = &.{spec} }) catch |e| {
        const detail = gitErrorDetail(e);
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot push {s} to {s}: {s}", .{ ref_name, s.remote, detail }) catch "push failed" } };
    };
    gitx.update_ref(a, s.c.io, s.repo, ref_name, oid) catch {};
    var result = jsonio.new_object();
    jsonio.obj_put(&result, a, "ref", .{ .string = ref_name });
    jsonio.obj_put(&result, a, "oid", .{ .string = oid });
    return .{ .out = .{ .ok = .{ .text = jsonio.to_string_pretty(a, .{ .object = result }), .fresh = false, .fetched_at = fetchedAt(s) } } };
}

fn toolRegister(s: *Server, a: std.mem.Allocator, force: bool) ToolCall {
    const ident = localIdentity(s, a) catch |e| {
        return .{ .out = .{ .err = identityError(a, e) } };
    };
    const dir = identity.project_identity_dir(a, s.c.io, s.repo) catch |e| {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot locate this worktree's identity dir: {t}", .{e}) catch "cannot locate identity" } };
    };
    const key_path = identity.resolve_key_path(a, dir, ident.principal, ident.key) catch |e| {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot resolve the identity key: {t}", .{e}) catch "cannot resolve key" } };
    };
    const key = identity.read_signing_key(a, s.c.io, key_path) catch |e| {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot read identity key {s}: {t}", .{ key_path, e }) catch "cannot read key" } };
    };
    const public_key = entry_mod.public_key_b64(a, key);
    // Read the registry fresh before touching it: a stale local ref would let
    // a rotation slip past the guard. Fail closed — registration is a write.
    gitx.fetch_collab_bounded(a, s.c.io, s.repo, s.remote) catch |e| {
        const detail = gitErrorDetail(e);
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot fetch refs/collab/meta before registering {s} ({s}): {s}", .{ ident.principal, @errorName(e), detail }) catch "cannot fetch before registering" } };
    };
    const registered = cmd_collab.registered_public_key(s.c, s.repo, ident.principal) catch {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "the registration for {s} is malformed (public_key missing or not a string); run `wg collab principal-register` explicitly", .{ident.principal}) catch "bad registration" } };
    };
    if (rotationRefused(registered, public_key, force)) {
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "principal {s} is already registered with a different public key; rotating makes every past signature by this principal unverified — re-run with force:true, or rotate explicitly: `wg collab principal-register --principal {s} --key <key> --push <remote>`", .{ ident.principal, ident.principal }) catch "rotation refused" } };
    }
    const result = cmd_collab.publish_principal(s.c, s.repo, ident.principal, public_key, s.remote) catch |e| {
        const detail = gitErrorDetail(e);
        return .{ .out = .{ .err = std.fmt.allocPrint(a, "cannot register {s}: {s}", .{ ident.principal, detail }) catch "registration failed" } };
    };
    var v = jsonio.new_object();
    jsonio.obj_put(&v, a, "ref", .{ .string = result.ref_name });
    jsonio.obj_put(&v, a, "oid", .{ .string = result.oid });
    return .{ .out = .{ .ok = .{ .text = jsonio.to_string_pretty(a, .{ .object = v }), .fresh = false, .fetched_at = fetchedAt(s) } } };
}

/// `collab_register` must not silently rotate: the same guard `wg collab
/// join` applies (§4.3). Rotation makes every past entry by the principal
/// unverified, and the log is append-only.
fn rotationRefused(registered: ?[]const u8, next: []const u8, force: bool) bool {
    if (force) return false;
    const current = registered orelse return false;
    return !std.mem.eql(u8, current, next);
}

fn localIdentity(s: *Server, a: std.mem.Allocator) !identity.Identity {
    const dir = try identity.project_identity_dir(a, s.c.io, s.repo);
    return identity.read_identity_at(a, s.c.io, dir);
}

fn identityError(a: std.mem.Allocator, e: anyerror) []const u8 {
    const hint = switch (e) {
        error.IdentityReadFailed => "this worktree has no D1 identity — run `wg collab join --principal <name>` first",
        error.IdentityBad => "the worktree identity file is malformed — fix or rebind it before publishing",
        else => "cannot read this worktree's identity",
    };
    return std.fmt.allocPrint(a, "{s} ({t})", .{ hint, e }) catch hint;
}

const PublishError = error{
    EntryNotObject,
    MissingField,
    WrongType,
    EmptyField,
    ActorNotRefnameSafe,
    ActorMismatch,
    MissingSig,
    BadSig,
    OutOfMemory,
};

/// The same structural parse the server's write path performs
/// (`serde_json::from_value::<Entry>`): required fields present and typed,
/// `version` in `u32` range, `refs` an object|null of string|null members,
/// `body` present (any JSON value), `sig` defaulting to `""`, unknown fields
/// ignored. On top of that, this tool requires a signature and the caller's
/// own actor — the MCP write-side discipline.
fn validatePublishEntry(a: std.mem.Allocator, v: Value, caller: []const u8) PublishError!entry_mod.Entry {
    const parsed = entry_mod.parse_entry(v) catch |e| return switch (e) {
        error.NotAnObject => error.EntryNotObject,
        error.MissingField => error.MissingField,
        error.WrongType => error.WrongType,
        error.OutOfMemory => error.OutOfMemory,
    };
    if (parsed.kind.len == 0 or parsed.actor.len == 0) return error.EmptyField;
    if (!entry_mod.ref_segment_ok(parsed.actor)) return error.ActorNotRefnameSafe;
    if (!std.mem.eql(u8, parsed.actor, caller)) return error.ActorMismatch;
    // `sig` may be absent/empty on the wire type (serde default), but an
    // unsigned entry is exactly what this tool refuses.
    if (parsed.sig.len == 0) return error.MissingSig;
    if (!sigLooksLikeEd25519(a, parsed.sig)) return error.BadSig;
    return parsed;
}

fn sigLooksLikeEd25519(a: std.mem.Allocator, sig: []const u8) bool {
    const prefix = "ed25519:";
    if (!std.mem.startsWith(u8, sig, prefix)) return false;
    const b64 = std.mem.trim(u8, sig[prefix.len..], " \t\r\n");
    const bytes = util.b64_decode(a, b64) catch return false;
    return bytes.len == 64;
}

fn publishErrorText(a: std.mem.Allocator, e: anyerror) []const u8 {
    const msg: []const u8 = switch (e) {
        error.EntryNotObject => "`entry` must be a JSON object",
        error.MissingField => "`entry` is missing a required field (version/kind/id/actor/ts/parent/body/sig)",
        error.WrongType => "`entry` has a field of the wrong type",
        error.EmptyField => "`entry.kind` and `entry.actor` must not be empty",
        error.ActorNotRefnameSafe => "`entry.actor` is not a refname-safe segment",
        error.ActorMismatch => "`entry.actor` is not this worktree's identity principal — publish as the principal whose key you hold",
        error.MissingSig => "`entry.sig` is empty — sign the canonical bytes with your own key (`wg collab entry` does both)",
        error.BadSig => "`entry.sig` is not `ed25519:` + a base64 64-byte signature",
        error.OutOfMemory => "entry is too large to inspect",
        else => "entry rejected",
    };
    return std.fmt.allocPrint(a, "{s}", .{msg}) catch "entry rejected";
}

// ---- tests ------------------------------------------------------------------------------

test "tool list is the collaboration surface and its schemas are strict objects" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    const v = toolsList(a);
    const tools = v.object.get("tools").?.array.items;
    try std.testing.expectEqual(@as(usize, all_tools.len), tools.len);
    const expected = [_][]const u8{ "collab_board", "collab_thread", "collab_report", "ci_status", "collab_publish", "collab_register" };
    for (expected) |name| {
        var found = false;
        for (tools) |t| {
            if (std.mem.eql(u8, t.object.get("name").?.string, name)) found = true;
        }
        try std.testing.expect(found);
    }
    for (tools) |t| {
        const schema = t.object.get("inputSchema").?.object;
        try std.testing.expectEqualStrings("object", schema.get("type").?.string);
        try std.testing.expect(schema.get("properties") != null);
        try std.testing.expect(schema.get("required") != null);
        // Same strictness as the host schemas.
        try std.testing.expectEqual(false, schema.get("additionalProperties").?.bool);
    }
    // collab_register has exactly one optional argument: `force`.
    for (tools) |t| {
        if (std.mem.eql(u8, t.object.get("name").?.string, "collab_register")) {
            const props = t.object.get("inputSchema").?.object.get("properties").?.object;
            try std.testing.expectEqual(@as(usize, 1), props.count());
            try std.testing.expect(props.get("force") != null);
            try std.testing.expectEqual(@as(usize, 0), t.object.get("inputSchema").?.object.get("required").?.array.items.len);
        }
    }
}

test "publish validation refuses unsigned, foreign-actor and malformed entries" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    const good_sig = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
    const base = "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"" ++ good_sig ++ "\"}";
    const v = try std.json.parseFromSliceLeaky(Value, a, base, .{});
    _ = try validatePublishEntry(a, v, "alice");

    try std.testing.expectError(error.ActorMismatch, validatePublishEntry(a, v, "bob"));

    const unsigned = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"\"}", .{});
    try std.testing.expectError(error.MissingSig, validatePublishEntry(a, unsigned, "alice"));

    const bad_sig = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"ed25519:short\"}", .{});
    try std.testing.expectError(error.BadSig, validatePublishEntry(a, bad_sig, "alice"));

    const missing_kind = try std.json.parseFromSliceLeaky(Value, a, "{\"id\":\"c1\",\"actor\":\"alice\"}", .{});
    try std.testing.expectError(error.MissingField, validatePublishEntry(a, missing_kind, "alice"));

    const bad_actor = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"a/b\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"" ++ good_sig ++ "\"}", .{});
    try std.testing.expectError(error.ActorNotRefnameSafe, validatePublishEntry(a, bad_actor, "a/b"));

    // Same parse as the server's serde: refs must be object|null of
    // string|null, version must fit u32, body may be any JSON value, and
    // unknown fields are ignored (not rejected).
    const bad_refs = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"refs\":{\"base\":7},\"body\":{},\"sig\":\"" ++ good_sig ++ "\"}", .{});
    try std.testing.expectError(error.WrongType, validatePublishEntry(a, bad_refs, "alice"));
    const bad_version = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":-1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"" ++ good_sig ++ "\"}", .{});
    try std.testing.expectError(error.WrongType, validatePublishEntry(a, bad_version, "alice"));
    const array_body = try std.json.parseFromSliceLeaky(Value, a, "{\"version\":1,\"kind\":\"comment\",\"id\":\"c1\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":[1],\"unknown\":true,\"sig\":\"" ++ good_sig ++ "\"}", .{});
    _ = try validatePublishEntry(a, array_body, "alice");
}

test "collab_register refuses a silent key rotation unless forced" {
    const current = "ed25519:OLD";
    const next = "ed25519:NEW";
    try std.testing.expect(rotationRefused(current, next, false));
    try std.testing.expect(!rotationRefused(current, next, true));
    try std.testing.expect(!rotationRefused(current, current, false));
    try std.testing.expect(!rotationRefused(null, next, false));
}

test "thread value carries the host shape: id, ordered entries, broken_refs, pr" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    var principals = entry_mod.Principals.init(a);
    try principals.put("alice", "not-a-real-key");
    const issue_body = try std.json.parseFromSliceLeaky(Value, a, "{\"title\":\"the card\"}", .{});
    const entries = [_]entry_mod.EntryRef{
        .{ .oid = "1111111111111111111111111111111111111111", .principal = "alice", .entry = .{
            .version = 1,
            .kind = "issue",
            .id = "card-1",
            .actor = "alice",
            .ts = 10,
            .parent = "",
            .refs = null,
            .body = issue_body,
            .sig = "",
        } },
        .{ .oid = "2222222222222222222222222222222222222222", .principal = "alice", .entry = .{
            .version = 1,
            .kind = "comment",
            .id = "card-1",
            .actor = "alice",
            .ts = 20,
            .parent = "1111111111111111111111111111111111111111",
            .refs = null,
            .body = try std.json.parseFromSliceLeaky(Value, a, "{\"related\":[\"deadbeef\"]}", .{}),
            .sig = "",
        } },
    };
    const v = try threadValue(a, "card-1", &entries, &principals, &collab.MergeRules{});
    try std.testing.expectEqualStrings("card-1", v.object.get("id").?.string);
    const list = v.object.get("entries").?.array.items;
    try std.testing.expectEqual(@as(usize, 2), list.len);
    // parent-ordered, and the unresolved oid is reported per entry.
    try std.testing.expectEqual(@as(usize, 1), list[1].object.get("broken_refs").?.array.items.len);
    try std.testing.expectEqualStrings("deadbeef", list[1].object.get("broken_refs").?.array.items[0].string);
    try std.testing.expect(v.object.get("pr").? == .null); // no patch
    // Unverified entries are shown, never dropped.
    try std.testing.expect(!list[0].object.get("verified").?.bool);
}

test "board version is the Rust witness+claim rule" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    var cards = util.List(collab.BoardCard).init(a);
    try cards.append(.{
        .id = "card-1",
        .title = "",
        .prose = "",
        .actor = "",
        .status = "open",
        .owner = "",
        .worktree = "",
        .branch = "",
        .work = "",
        .created_ts = 0,
        .last_ts = 0,
        .entries = 0,
        .verified = 0,
        .unverified = 0,
        .kinds = &.{},
        .last_oid = "abc123",
        .merge = null,
        .claim = null,
    });
    var columns = [_]collab.BoardColumn{.{ .name = "open", .cards = cards.items }};
    const board = collab.Board{ .columns = &columns };
    const no_claim = try boardVersion(a, &board, 100);
    // `b:` + the 16-hex witness above (0usize, then the four card strings).
    var h = Sip13.init();
    h.writeUsize(0);
    h.writeStr("card-1");
    h.writeStr("abc123");
    h.writeStr("open");
    h.writeStr("");
    const expected = try std.fmt.allocPrint(a, "b:{x:0>16}", .{h.finish()});
    try std.testing.expectEqualStrings(expected, no_claim);

    columns[0].cards = cards.items;
    var with_claim_cards = util.List(collab.BoardCard).init(a);
    try with_claim_cards.append(.{
        .id = "card-1",
        .title = "",
        .prose = "",
        .actor = "",
        .status = "in-progress",
        .owner = "bob",
        .worktree = "",
        .branch = "",
        .work = "",
        .created_ts = 0,
        .last_ts = 0,
        .entries = 0,
        .verified = 0,
        .unverified = 0,
        .kinds = &.{},
        .last_oid = "abc123",
        .merge = null,
        .claim = .{ .state = "claimed", .oid = "x", .actor = "bob", .ts = 90, .ttl = 30, .expires_at = 120 },
    });
    var claim_columns = [_]collab.BoardColumn{.{ .name = "open", .cards = with_claim_cards.items }};
    const claim_board = collab.Board{ .columns = &claim_columns };
    const live = try boardVersion(a, &claim_board, 100);
    try std.testing.expect(std.mem.endsWith(u8, live, ":claim:120"));
    const expired = try boardVersion(a, &claim_board, 121);
    try std.testing.expect(!std.mem.endsWith(u8, expired, ":claim:120"));
}

test "the resource URI grammar is the two collab resources" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    try std.testing.expect(parseResourceUri(a, "walgit://collab/board") != null);
    const t = parseResourceUri(a, "walgit://collab/thread/card-1").?;
    try std.testing.expectEqualStrings("card-1", t.thread);
    try std.testing.expect(parseResourceUri(a, "walgit://refs/acme/x") == null);
    try std.testing.expect(parseResourceUri(a, "walgit://collab/thread/") == null);
    // A generic client percent-encodes `-` and `@`; decode `%XX`, reject a
    // malformed escape rather than reading some other thread.
    const decoded = parseResourceUri(a, "walgit://collab/thread/cc%2Dai%40example").?;
    try std.testing.expectEqualStrings("cc-ai@example", decoded.thread);
    try std.testing.expect(parseResourceUri(a, "walgit://collab/thread/card%2") == null);
    try std.testing.expect(parseResourceUri(a, "walgit://collab/thread/card%zz") == null);
}

test "fresh policy: the per-call argument wins over the process default" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    const o = struct {
        fn with(al: std.mem.Allocator, v: Value) std.json.ObjectMap {
            var m = jsonio.new_object();
            jsonio.obj_put(&m, al, "fresh", v);
            return m;
        }
    }.with;
    var srv = Server{
        .c = undefined,
        .repo = ".",
        .remote = "origin",
        .no_fetch = false,
        .fresh_default = true,
        .interval_ms = 1000,
        .max_subs = 1,
        .subs = undefined,
    };
    try std.testing.expect(freshFor(&srv, jsonio.new_object()));
    srv.fresh_default = false;
    try std.testing.expect(!freshFor(&srv, jsonio.new_object()));
    try std.testing.expect(freshFor(&srv, o(a, .{ .bool = true })));
    try std.testing.expect(!freshFor(&srv, o(a, .{ .bool = false })));
    // `--no-fetch` is a hard gate: neither the default nor `fresh:true` can
    // re-enable the network.
    srv.no_fetch = true;
    srv.fresh_default = true;
    try std.testing.expect(!freshFor(&srv, jsonio.new_object()));
    try std.testing.expect(!freshFor(&srv, o(a, .{ .bool = true })));
}

test "ci_status answers the host shape {repo, runs}, optionally filtered by id" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    const body = try std.json.parseFromSliceLeaky(Value, a,
        \\{"task":"test","ref":"refs/heads/main","commit":"c0ffee","ttl":300,"attempt":1}
    , .{});
    const entries = [_]entry_mod.EntryRef{
        .{ .oid = "0123456789abcdef0123456789abcdef01234567", .principal = "ci", .entry = .{
            .version = 1,
            .kind = "ci_claim",
            .id = "ci-run-1",
            .actor = "ci",
            .ts = 1_000,
            .parent = "",
            .refs = null,
            .body = body,
            .sig = "",
        } },
        .{ .oid = "89abcdef0123456789abcdef0123456789abcdef01", .principal = "ci", .entry = .{
            .version = 1,
            .kind = "ci_claim",
            .id = "ci-run-2",
            .actor = "ci",
            .ts = 2_000,
            .parent = "",
            .refs = null,
            .body = body,
            .sig = "",
        } },
    };
    var principals = entry_mod.Principals.init(a);
    const ci_slice = try ci.ci_entries(a, &entries);
    const runs = try ci.collect_runs(a, ci_slice, &principals, 2_000);

    const all = try std.json.parseFromSliceLeaky(Value, a, try ciStatusJson(a, "acme/widgets", runs, null), .{});
    try std.testing.expectEqualStrings("acme/widgets", all.object.get("repo").?.string);
    try std.testing.expectEqual(@as(usize, 2), all.object.get("runs").?.array.items.len);
    // `now` never appears: this is the host wrapper, not `wg ci status`.
    try std.testing.expect(all.object.get("now") == null);

    const one = try std.json.parseFromSliceLeaky(Value, a, try ciStatusJson(a, "acme/widgets", runs, "ci-run-2"), .{});
    const list = one.object.get("runs").?.array.items;
    try std.testing.expectEqual(@as(usize, 1), list.len);
    try std.testing.expectEqualStrings("ci-run-2", list[0].object.get("id").?.string);
}

test "collab_thread carries the PR view when the thread has a patch" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    var principals = entry_mod.Principals.init(a);
    const mk = struct {
        fn entry(al: std.mem.Allocator, kind: []const u8, oid: []const u8, ts: i64, parent: []const u8, body_json: []const u8, base: ?[]const u8, head: ?[]const u8) entry_mod.EntryRef {
            const body = std.json.parseFromSliceLeaky(Value, al, body_json, .{}) catch unreachable;
            return .{ .oid = oid, .principal = "alice", .entry = .{
                .version = 1,
                .kind = kind,
                .id = "pr-card",
                .actor = "alice",
                .ts = ts,
                .parent = parent,
                .refs = if (base == null and head == null) null else .{ .base = base, .head = head },
                .body = body,
                .sig = "",
            } };
        }
    }.entry;
    const entries = [_]entry_mod.EntryRef{
        mk(a, "issue", "1111111111111111111111111111111111111111", 10, "", "{\"title\":\"pr\"}", null, null),
        mk(a, "patch", "2222222222222222222222222222222222222222", 20, "1111111111111111111111111111111111111111", "{\"message\":\"x\"}", "refs/heads/feat", "refs/heads/feat"),
        mk(a, "review", "3333333333333333333333333333333333333333", 30, "2222222222222222222222222222222222222222", "{\"decision\":\"approve\"}", null, null),
    };
    const v = try threadValue(a, "pr-card", &entries, &principals, &collab.MergeRules{});
    try std.testing.expectEqual(@as(usize, 3), v.object.get("entries").?.array.items.len);
    const pr = v.object.get("pr").?.object;
    try std.testing.expectEqualStrings("pr-card", pr.get("pr").?.object.get("id").?.string);
    try std.testing.expectEqualStrings("refs/heads/feat", pr.get("pr").?.object.get("base").?.string);
    try std.testing.expectEqualStrings("open", pr.get("pr").?.object.get("status").?.string);
    // merge evaluation rides along: the base is unprotected, so allowed.
    try std.testing.expect(pr.get("merge").?.object.get("allowed").?.bool);
    try std.testing.expectEqual(@as(usize, 1), pr.get("pr").?.object.get("reviews").?.array.items.len);
}

test "report value is the host shape: pair arrays, no host-only keys" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    var threads = [_]collab.ReportThread{.{ .id = "t1", .title = "T", .entries = 2, .verified = 1, .last_ts = 5, .kinds = blk: {
        var kinds = [_][]const u8{ "issue", "status" };
        break :blk kinds[0..];
    } }};
    var prs = [_]collab.ReportPr{.{ .id = "t1", .title = "T", .base = "refs/heads/main", .head = "refs/heads/feat", .status = "open", .approvals = 0, .merge_allowed = false, .merge_reason = "needs approvals" }};
    var by_actor = [_]collab.Report.Count{.{ .key = "alice", .n = 2 }};
    var by_kind = [_]collab.Report.Count{.{ .key = "issue", .n = 1 }};
    const report = collab.Report{
        .threads = threads[0..],
        .prs = prs[0..],
        .runs = &.{},
        .total_entries = 2,
        .verified_entries = 1,
        .unverified_entries = 1,
        .missing_principals = 1,
        .by_actor = by_actor[0..],
        .by_kind = by_kind[0..],
    };
    const v = try reportValue(a, &report);
    const compact = jsonio.to_string(a, v);
    // `by_actor` is a `[[key, n], …]` pair array (Rust's Vec<(String, usize)>).
    try std.testing.expect(std.mem.indexOf(u8, compact, "\"by_actor\":[[\"alice\",2]]") != null);
    const pr = v.object.get("prs").?.array.items[0].object;
    try std.testing.expectEqualStrings("refs/heads/main", pr.get("base").?.string);
    try std.testing.expectEqual(@as(i64, 0), pr.get("approvals").?.integer);
}

test "repo id comes from the remote URL, and a local path falls back" {
    const a = std.testing.allocator;
    const r1 = try repoIdFromUrl(a, "http://127.0.0.1:8081/acme/widgets.git");
    defer a.free(r1);
    try std.testing.expectEqualStrings("acme/widgets", r1);
    const r2 = try repoIdFromUrl(a, "git@host:acme/widgets.git");
    defer a.free(r2);
    try std.testing.expectEqualStrings("acme/widgets", r2);
    const r3 = try repoIdFromUrl(a, "/tmp/origin.git");
    defer a.free(r3);
    try std.testing.expectEqualStrings("origin", r3);
}
