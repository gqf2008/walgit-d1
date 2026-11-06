//! `wgl collab` — the D1 collaboration command surface
//! (`docs/D1_PROTOCOL.md`), ported from the retired `walgit collab` family.
//! Read commands aggregate the local `refs/collab/*` offline (§3.2); writes
//! are local refs pushed over receive-pack, and the host registry commands
//! speak HTTP (§4.4).

const std = @import("std");
const args = @import("args.zig");
const collab = @import("d1/collab.zig");
const ci_mod = @import("d1/ci.zig");
const entry_mod = @import("d1/entry.zig");
const snapshot_mod = @import("d1/snapshot.zig");
const ci_runs = @import("d1/ci.zig");
const gitx = @import("gitx.zig");
const identity = @import("identity.zig");
const jsonio = @import("jsonio.zig");
const collabreader = @import("collabreader.zig");
const render = @import("render.zig");
const httpc = @import("httpc.zig");
const util = @import("util.zig");

const Io = std.Io;
const EntryRef = entry_mod.EntryRef;
const Entry = entry_mod.Entry;
const Principals = entry_mod.Principals;
const Value = std.json.Value;

/// One command run: the process's Io, its environment, and one arena the
/// whole run allocates from.
pub const Ctx = struct {
    alloc: std.mem.Allocator,
    io: Io,
    env: *std.process.Environ.Map,

    pub fn stdout(self: *const Ctx, bytes: []const u8) void {
        std.Io.File.stdout().writeStreamingAll(self.io, bytes) catch {};
    }

    pub fn stdout_line(self: *const Ctx, bytes: []const u8) void {
        var out: util.List(u8) = .init(self.alloc);
        out.append_slice(bytes) catch @panic("oom");
        out.append('\n') catch @panic("oom");
        self.stdout(out.items);
    }

    pub fn env_or(self: *const Ctx, name: []const u8, default: []const u8) []const u8 {
        if (self.env.get(name)) |v| {
            const t = std.mem.trim(u8, v, " \t");
            if (t.len > 0) return t;
        }
        return default;
    }

    pub fn token_opt(self: *const Ctx, flag_token: ?[]const u8) ?[]const u8 {
        if (flag_token) |t| {
            if (t.len > 0) return t;
        }
        if (self.env.get("WALGIT_TOKEN")) |v| {
            const t = std.mem.trim(u8, v, " \t");
            if (t.len > 0) return t;
        }
        return null;
    }

    /// The runtime session id: `KI_AGENT_ID`, else `WALGIT_SESSION` — a
    /// coordination label, not a credential.
    pub fn session(self: *const Ctx) ?[]const u8 {
        for ([_][]const u8{ "KI_AGENT_ID", "WALGIT_SESSION" }) |name| {
            if (self.env.get(name)) |v| {
                const t = std.mem.trim(u8, v, " \t");
                if (t.len > 0 and t.len <= 128) return t;
            }
        }
        return null;
    }
};

// ---- read commands -----------------------------------------------------------------------

pub fn ls(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    var ids: util.List([]const u8) = .init(c.alloc);
    for (loaded.entries) |e| {
        var dup = false;
        for (ids.items) |id| {
            if (std.mem.eql(u8, id, e.entry.id)) dup = true;
        }
        if (!dup) ids.append(e.entry.id) catch @panic("oom");
    }
    std.mem.sort([]const u8, ids.items, {}, lt_str);
    var out: util.List(u8) = .init(c.alloc);
    for (ids.items) |id| {
        out.append_slice(id) catch @panic("oom");
        out.append('\n') catch @panic("oom");
    }
    c.stdout(out.items);
    return 0;
}

fn lt_str(_: void, a: []const u8, b: []const u8) bool {
    return std.mem.order(u8, a, b) == .lt;
}

pub fn thread(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const id = try p.required_positional("id", 0);
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    var filtered: util.List(EntryRef) = .init(c.alloc);
    for (loaded.entries) |e| {
        if (std.mem.eql(u8, e.entry.id, id)) try filtered.append(e);
    }
    if (filtered.items.len == 0) {
        std.debug.print("wgl: no entries for thread {s}\n", .{id});
        return error.NoThread;
    }
    const ordered = try collab.thread(c.alloc, filtered.items);
    // json!-shaped output: every object's keys sorted (serde_json BTreeMap).
    var arr = std.json.Array.init(c.alloc);
    for (ordered) |r| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, c.alloc, "oid", .{ .string = r.oid });
        jsonio.obj_put(&o, c.alloc, "principal", .{ .string = r.principal });
        jsonio.obj_put(&o, c.alloc, "verified", .{ .bool = r.is_verified(c.alloc, loaded.principals) });
        jsonio.obj_put(&o, c.alloc, "entry", entry_value(c.alloc, &r.entry));
        arr.append(.{ .object = o }) catch @panic("oom");
    }
    const out = jsonio.to_string_pretty(c.alloc, .{ .array = arr });
    c.stdout_line(out);
    return 0;
}

/// An entry as a json! value would serialize it (struct → sorted BTreeMap).
fn entry_value(alloc: std.mem.Allocator, e: *const Entry) Value {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, alloc, "version", .{ .integer = e.version });
    jsonio.obj_put(&o, alloc, "kind", .{ .string = e.kind });
    jsonio.obj_put(&o, alloc, "id", .{ .string = e.id });
    jsonio.obj_put(&o, alloc, "actor", .{ .string = e.actor });
    jsonio.obj_put(&o, alloc, "ts", .{ .integer = e.ts });
    jsonio.obj_put(&o, alloc, "parent", .{ .string = e.parent });
    if (e.refs) |refs| {
        var ro = jsonio.new_object();
        if (refs.base) |b| jsonio.obj_put(&ro, alloc, "base", .{ .string = b });
        if (refs.head) |h| jsonio.obj_put(&ro, alloc, "head", .{ .string = h });
        jsonio.obj_put(&o, alloc, "refs", .{ .object = ro });
    }
    jsonio.obj_put(&o, alloc, "body", e.body);
    jsonio.obj_put(&o, alloc, "sig", .{ .string = e.sig });
    return .{ .object = o };
}

pub fn thread_heads(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    const heads = try collab.thread_head_map(c.alloc, loaded.entries);
    var o = jsonio.new_object();
    for (heads) |h| {
        jsonio.obj_put(&o, c.alloc, h.id, .{ .string = h.head });
    }
    c.stdout_line(jsonio.to_string(c.alloc, .{ .object = o }));
    return 0;
}

pub fn pr(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const id = try p.required_positional("id", 0);
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    var filtered: util.List(EntryRef) = .init(c.alloc);
    for (loaded.entries) |e| {
        if (std.mem.eql(u8, e.entry.id, id)) try filtered.append(e);
    }
    const pr_view = try collab.pr_view(c.alloc, filtered.items, loaded.principals);
    const rules = collabreader.load_merge_rules(c.alloc, c.io, repo, p.opt("rules")) catch {
        std.debug.print("wgl: merge rules document is invalid — fail closed\n", .{});
        return error.BadRules;
    };
    const eval = try collab.merge_rule_eval(c.alloc, &rules, &pr_view);

    var pr_obj = jsonio.new_object();
    jsonio.obj_put(&pr_obj, c.alloc, "id", .{ .string = pr_view.id });
    if (pr_view.base) |b| jsonio.obj_put(&pr_obj, c.alloc, "base", .{ .string = b }) else jsonio.obj_put(&pr_obj, c.alloc, "base", .null);
    if (pr_view.head) |h| jsonio.obj_put(&pr_obj, c.alloc, "head", .{ .string = h }) else jsonio.obj_put(&pr_obj, c.alloc, "head", .null);
    jsonio.obj_put(&pr_obj, c.alloc, "status", .{ .string = pr_view.status });
    jsonio.obj_put(&pr_obj, c.alloc, "reviews", reviews_value(c.alloc, pr_view.reviews));
    jsonio.obj_put(&pr_obj, c.alloc, "human_approvals", reviews_value(c.alloc, pr_view.human_approvals));
    {
        var arr = std.json.Array.init(c.alloc);
        for (pr_view.authors) |a| arr.append(.{ .string = a }) catch @panic("oom");
        jsonio.obj_put(&pr_obj, c.alloc, "authors", .{ .array = arr });
    }
    {
        var arr = std.json.Array.init(c.alloc);
        for (pr_view.unverified) |a| arr.append(.{ .string = a }) catch @panic("oom");
        jsonio.obj_put(&pr_obj, c.alloc, "unverified", .{ .array = arr });
    }
    var merge_obj = jsonio.new_object();
    jsonio.obj_put(&merge_obj, c.alloc, "allowed", .{ .bool = eval.allowed });
    jsonio.obj_put(&merge_obj, c.alloc, "reason", .{ .string = eval.reason });
    {
        var arr = std.json.Array.init(c.alloc);
        for (eval.satisfied_by) |a| arr.append(.{ .string = a }) catch @panic("oom");
        jsonio.obj_put(&merge_obj, c.alloc, "satisfied_by", .{ .array = arr });
    }
    var root = jsonio.new_object();
    jsonio.obj_put(&root, c.alloc, "pr", .{ .object = pr_obj });
    jsonio.obj_put(&root, c.alloc, "merge", .{ .object = merge_obj });
    c.stdout_line(jsonio.to_string_pretty(c.alloc, .{ .object = root }));
    return 0;
}

fn reviews_value(alloc: std.mem.Allocator, reviews: []collab.Review) Value {
    var arr = std.json.Array.init(alloc);
    for (reviews) |r| {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, alloc, "actor", .{ .string = r.actor });
        jsonio.obj_put(&o, alloc, "decision", .{ .string = r.decision });
        jsonio.obj_put(&o, alloc, "ts", .{ .integer = r.ts });
        jsonio.obj_put(&o, alloc, "oid", .{ .string = r.oid });
        arr.append(.{ .object = o }) catch @panic("oom");
    }
    return .{ .array = arr };
}

pub fn board(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const format = p.get("format", "text");
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    const rules = collabreader.load_merge_rules(c.alloc, c.io, repo, p.opt("rules")) catch {
        std.debug.print("wgl: merge rules document is invalid — fail closed\n", .{});
        return error.BadRules;
    };
    const board_def = collabreader.load_board_def(c.alloc, c.io, repo, p.opt("board")) catch {
        std.debug.print("wgl: board definition is invalid — fail closed (never a silently mis-folded board)\n", .{});
        return error.BadBoard;
    };
    const b = try collab.build_board(c.alloc, loaded.entries, loaded.principals, &rules, &board_def, util.now_ts());
    if (std.mem.eql(u8, format, "text")) {
        c.stdout(render.render_board_text(c.alloc, &b));
    } else if (std.mem.eql(u8, format, "markdown")) {
        c.stdout(render.render_board_markdown(c.alloc, &b));
    } else if (std.mem.eql(u8, format, "json")) {
        // The wire form: exactly the bytes the server endpoint serializes,
        // so the two independent clients can be diffed byte-for-byte.
        c.stdout(b.to_json(c.alloc));
    } else if (std.mem.eql(u8, format, "hash")) {
        const bytes = b.to_json(c.alloc);
        var digest: [32]u8 = undefined;
        std.crypto.hash.sha2.Sha256.hash(bytes, &digest, .{});
        c.stdout_line(util.to_hex(c.alloc, &digest));
    } else {
        std.debug.print("wgl: unknown board format {s} (text|markdown|json|hash)\n", .{format});
        return error.BadFormat;
    }
    return 0;
}

pub fn report(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const format = p.get("format", "text");
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    const rules = collabreader.load_merge_rules(c.alloc, c.io, repo, p.opt("rules")) catch {
        std.debug.print("wgl: merge rules document is invalid — fail closed\n", .{});
        return error.BadRules;
    };
    const now = util.now_ts();
    const r = try collab.build_report(c.alloc, loaded.entries, loaded.principals, &rules, now);

    // `--remote` is the only network touch: an unreachable remote is an
    // error, never a silent `unpushed: 0`.
    var unpushed: ?render.Unpushed = null;
    if (p.opt("remote")) |remote| {
        const local = try reader.syncable_refs();
        const remote_refs = gitx.ls_remote(c.alloc, c.io, repo, remote) catch {
            std.debug.print("wgl: cannot read remote {s} — refusing to report unpushed: 0\n", .{remote});
            return error.RemoteUnreachable;
        };
        var count: usize = 0;
        for (local) |l| {
            var found = false;
            for (remote_refs) |rr| {
                if (std.mem.eql(u8, rr.name, l.name) and std.mem.eql(u8, rr.oid, l.oid)) {
                    found = true;
                    break;
                }
            }
            if (!found) count += 1;
        }
        unpushed = .{ .remote = remote, .count = count };
    }

    // The truncated-fold banner (§9.2): visible red, never silent history.
    var banner: []const u8 = "";
    if (try reader.snapshot_blob()) |snap| {
        if (snapshot_mod.parse_snapshot(c.alloc, snap.bytes) catch null) |parsed| {
            if (!parsed.complete) {
                banner = std.fmt.allocPrint(c.alloc, "WARNING: folded history truncated — {d} oldest entry(ies) were dropped at the last fold (snapshot complete=false)\n\n", .{parsed.dropped_entries}) catch @panic("oom");
            }
        }
    }

    var buf: util.List(u8) = .init(c.alloc);
    // The CLI's CI section reads only the ci-kind slice — exactly what the
    // retired `walgit collab report` rendered (collab_cmd.rs run_report).
    const ci_slice = try ci_mod.ci_entries(c.alloc, loaded.entries);
    const now_runs = try ci_mod.collect_runs(c.alloc, ci_slice, loaded.principals, now);
    if (std.mem.eql(u8, format, "text")) {
        buf.append_slice(banner) catch @panic("oom");
        buf.append_slice(render.render_report_text(c.alloc, &r, unpushed)) catch @panic("oom");
        buf.append_slice("ci runs\n") catch @panic("oom");
        buf.append_slice(render.ci_runs_text(c.alloc, now_runs)) catch @panic("oom");
    } else if (std.mem.eql(u8, format, "markdown")) {
        buf.append_slice(banner) catch @panic("oom");
        buf.append_slice(render.render_report_markdown(c.alloc, &r, unpushed)) catch @panic("oom");
        buf.append_slice("## CI runs\n\n") catch @panic("oom");
        buf.append_slice(render.ci_runs_markdown(c.alloc, now_runs)) catch @panic("oom");
    } else if (std.mem.eql(u8, format, "html")) {
        var html = render.render_report_html(c.alloc, &r, unpushed);
        if (std.mem.indexOf(u8, banner, "truncated") != null) {
            html = std.mem.replaceOwned(u8, c.alloc, html, "<body>", "<body><p style=\"color:#b00\">WARNING: folded history truncated</p>") catch @panic("oom");
        }
        _ = &html;
        const section = render.ci_runs_html(c.alloc, now_runs);
        const full = std.mem.replaceOwned(u8, c.alloc, html, "</body>", std.fmt.allocPrint(c.alloc, "{s}</body>", .{section}) catch @panic("oom")) catch @panic("oom");
        buf.append_slice(full) catch @panic("oom");
    } else {
        std.debug.print("wgl: unknown report format {s} (text|markdown|html)\n", .{format});
        return error.BadFormat;
    }
    c.stdout(buf.items);
    return 0;
}

// ---- write commands -----------------------------------------------------------------------

pub fn entry(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const resolved = try identity.resolve_identity(c.alloc, c.io, repo, p.opt("actor"), p.opt("key"));
    try identity.ref_segment("entry.actor", resolved.principal);

    const body_text = try p.required("body");
    const body_v = std.json.parseFromSliceLeaky(Value, c.alloc, body_text, .{}) catch {
        std.debug.print("wgl: --body must be JSON: {s}\n", .{body_text});
        return error.BadBody;
    };
    if (body_v != .object) {
        std.debug.print("wgl: --body must be a JSON object to attach related/depends-on\n", .{});
        return error.BadBody;
    }
    var body = body_v.object;
    // Attribute the write to the runtime session when the host provides
    // one; an explicit caller value wins.
    if (c.session()) |session| {
        if (!body.contains("session")) {
            jsonio.obj_put(&body, c.alloc, "session", .{ .string = session });
        }
    }
    if (p.many("related").len > 0) {
        var arr = std.json.Array.init(c.alloc);
        for (p.many("related")) |o| arr.append(.{ .string = o }) catch @panic("oom");
        body.put(c.alloc, "related", .{ .array = arr }) catch @panic("oom");
    }
    if (p.many("depends-on").len > 0) {
        var arr = std.json.Array.init(c.alloc);
        for (p.many("depends-on")) |o| arr.append(.{ .string = o }) catch @panic("oom");
        body.put(c.alloc, "depends_on", .{ .array = arr }) catch @panic("oom");
    }
    if (p.many("attach").len > 0) {
        var arr = std.json.Array.init(c.alloc);
        for (p.many("attach")) |path| {
            var d = std.Io.Dir.cwd();
            const bytes = d.readFileAlloc(c.io, path, c.alloc, .limited(64 * 1024 + 1)) catch {
                std.debug.print("wgl: --attach {s}: read failed\n", .{path});
                return error.AttachReadFailed;
            };
            if (bytes.len > 64 * 1024) {
                std.debug.print("wgl: --attach {s}: {d} bytes exceeds the {d} KiB per-file cap\n", .{ path, bytes.len, 64 * 1024 / 1024 });
                return error.AttachTooBig;
            }
            var digest: [32]u8 = undefined;
            std.crypto.hash.sha2.Sha256.hash(bytes, &digest, .{});
            var o = jsonio.new_object();
            const base = std.fs.path.basename(path);
            jsonio.obj_put(&o, c.alloc, "filename", .{ .string = if (base.len > 0) base else "file" });
            jsonio.obj_put(&o, c.alloc, "sha256", .{ .string = util.to_hex(c.alloc, &digest) });
            jsonio.obj_put(&o, c.alloc, "content_b64", .{ .string = util.b64_encode(c.alloc, bytes) });
            arr.append(.{ .object = o }) catch @panic("oom");
        }
        body.put(c.alloc, "attachments", .{ .array = arr }) catch @panic("oom");
    }

    var e = Entry{
        .version = 1,
        .kind = try p.required("kind"),
        .id = try p.required("id"),
        .actor = resolved.principal,
        .ts = util.now_ts(),
        .parent = p.get("parent", ""),
        .refs = blk: {
            const base = p.opt("base");
            const head = p.opt("head");
            if (base == null and head == null) break :blk null;
            break :blk .{ .base = base, .head = head };
        },
        .body = .{ .object = body },
        .sig = "",
    };

    // The write-side done gate (§7.5): identical to the server's — the
    // thread must be at needs-review with a verified non-author approve.
    if (std.mem.eql(u8, e.kind, "status")) {
        if (collab.body_str(e.body, "status")) |st| {
            if (std.mem.eql(u8, st, "done")) {
                const reader = collabreader.Reader.init(c.alloc, c.io, repo);
                const loaded = reader.load(c.alloc) catch return error.LoadFailed;
                var group: util.List(EntryRef) = .init(c.alloc);
                for (loaded.entries) |er| {
                    if (std.mem.eql(u8, er.entry.id, e.id)) try group.append(er);
                }
                const ordered = try collab.thread(c.alloc, group.items);
                collab.validate_status_transition(c.alloc, ordered, loaded.principals) catch |err| {
                    if (err == error.TransitionNoApprove) {
                        std.debug.print("wgl: status transition rejected: transition → done requires a verified approve review in the thread\n提示: approve reviewer 的 key 若仅在 host registry,先 `wgl collab principal-fetch` 再试\n", .{});
                    } else {
                        std.debug.print("wgl: status transition rejected: transition → done requires current status needs-review (got {s})\n", .{collab.card_status(ordered)});
                    }
                    return err;
                };
            }
        }
    }

    const key = identity.read_signing_key(c.alloc, c.io, resolved.key_path) catch |err| {
        std.debug.print("wgl: key {s}: {t}\n", .{ resolved.key_path, err });
        return error.BadKey;
    };
    try collabreader.ensure_actor_key_binding(c.alloc, c.io, repo, resolved.principal, resolved.key_path, key);
    entry_mod.sign_entry(c.alloc, &e, key);
    const content = entry_mod.entry_json(c.alloc, &e, true);
    const oid = try gitx.write_blob(c.alloc, c.io, repo, content);
    const ref_name = try std.fmt.allocPrint(c.alloc, "refs/collab/inbox/{s}/{s}", .{ resolved.principal, identity.entry_uuid(c.alloc, c.io) });
    try gitx.update_ref(c.alloc, c.io, repo, ref_name, oid);
    if (p.opt("push")) |remote| {
        try gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = &.{ref_name} });
    } else {
        std.debug.print("note: {s} written to this checkout only — no remote sees it until `--push <remote>`, or `git push <remote> 'refs/collab/inbox/*:refs/collab/inbox/*'`\n", .{ref_name});
    }
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} {s}", .{ ref_name, oid }));

    // Opportunistic fold (§9.4): the writer already holds the key, so it can
    // keep the inbox under the server's read budget. Best-effort.
    const auto_fold = p.boolean("auto-fold");
    if (auto_fold and p.opt("push") != null) {
        const threshold = p.u64_of("fold-threshold", 10_000) catch 10_000;
        run_auto_fold(c, repo, resolved.principal, resolved.key_path, p.opt("push").?, threshold);
    }
    return 0;
}

fn run_auto_fold(c: *Ctx, repo: []const u8, actor: []const u8, key_path: []const u8, remote: []const u8, threshold: u64) void {
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const n = reader.inbox_refs() catch |err| {
        std.debug.print("wgl: collab auto-fold skipped (cannot list inbox): {t}\n", .{err});
        return;
    };
    if (n.len < threshold) return;
    gc_inner(c, repo, actor, key_path, remote, false) catch |err| {
        std.debug.print("wgl: collab auto-fold skipped (entry is published): {t}\n", .{err});
        return;
    };
    const msg = std.fmt.allocPrint(c.alloc, "collab auto-fold: folded {d} inbox ref(s) (threshold {d})", .{ n.len, threshold }) catch return;
    c.stdout_line(msg);
}

pub fn join(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const principal = try p.required("principal");
    try identity.ref_segment("principal", principal);
    const force = p.boolean("force");
    const dir = try identity.project_identity_dir(c.alloc, c.io, repo);
    const identity_path = try std.fs.path.join(c.alloc, &.{ dir, "identity" });

    var d = std.Io.Dir.cwd();
    const identity_exists = (d.access(c.io, identity_path, .{}) catch error.FileNotFound) != error.FileNotFound;
    if (identity_exists) {
        if (!force) {
            const existing = identity.read_identity_at(c.alloc, c.io, dir) catch |e| {
                std.debug.print("wgl: read identity {s}: {t}\n", .{ identity_path, e });
                return e;
            };
            if (!std.mem.eql(u8, existing.principal, principal)) {
                std.debug.print("wgl: this project is already bound to principal \"{s}\" ({s}); pass --force to rebind\n", .{ existing.principal, identity_path });
                return error.AlreadyBound;
            }
        }
    }

    // Load the candidate key without mutating anything: an existing file is
    // read, a missing default is generated in memory, and a missing explicit
    // `--key` is an error so a typo cannot silently fork the identity.
    const explicit_key = p.opt("key") != null;
    const key_path: []u8 = blk: {
        if (p.opt("key")) |k| {
            if (std.fs.path.isAbsolute(k)) break :blk c.alloc.dupe(u8, k) catch @panic("oom");
            break :blk std.fs.path.resolve(c.alloc, &.{ ".", k }) catch return error.OutOfMemory;
        }
        break :blk try identity.resolve_key_path(c.alloc, dir, principal, null);
    };
    var seed_to_write: ?[32]u8 = null;
    var key: identity.SigningKey = undefined;
    if ((std.Io.Dir.cwd().access(c.io, key_path, .{}) catch error.FileNotFound) != error.FileNotFound) {
        key = identity.read_signing_key(c.alloc, c.io, key_path) catch |err| {
            std.debug.print("wgl: existing key {s}: {t}\n", .{ key_path, err });
            return err;
        };
    } else if (explicit_key) {
        std.debug.print("wgl: --key {s} does not exist\n", .{key_path});
        return error.KeyMissing;
    } else {
        var seed: [32]u8 = undefined;
        c.io.random(&seed);
        key = identity.SigningKey.generateDeterministic(seed) catch return error.BadSeed;
        seed_to_write = seed;
    }
    const public_key = entry_mod.public_key_b64(c.alloc, key);

    // Rotation is destructive (§4.3): refuse it without --force, before any
    // local mutation.
    if (!force) {
        if (registered_public_key(c, repo, principal) catch null) |registered| {
            if (!std.mem.eql(u8, registered, public_key)) {
                std.debug.print("wgl: principal {s} is already registered with a different public key; rotating it makes every past signature by this principal unverified — pass --force to rotate\n", .{principal});
                return error.RotationGuard;
            }
        }
    }

    // All guards passed: only now mutate local state.
    try identity.make_private_dir(c.alloc, c.io, dir);
    if (seed_to_write) |*seed| {
        if (std.fs.path.dirname(key_path)) |parent| {
            identity.make_private_dir(c.alloc, c.io, parent) catch {};
        }
        const hex_seed = util.to_hex(c.alloc, seed);
        const with_nl = std.fmt.allocPrint(c.alloc, "{s}\n", .{hex_seed}) catch @panic("oom");
        try identity.write_private_file(c.alloc, c.io, key_path, with_nl);
    }
    {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, c.alloc, "principal", .{ .string = principal });
        const key_str: []const u8 = blk: {
            if (std.mem.startsWith(u8, key_path, dir) and key_path.len > dir.len + 1) {
                break :blk key_path[dir.len + 1 ..];
            }
            break :blk key_path;
        };
        jsonio.obj_put(&o, c.alloc, "key", .{ .string = key_str });
        const doc = jsonio.to_string_pretty(c.alloc, .{ .object = o });
        try identity.write_private_file(c.alloc, c.io, identity_path, doc);
    }

    const result = try publish_principal(c, repo, principal, public_key, p.opt("push"));
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} {s} principal={s} key={s} identity={s}", .{ result.ref_name, result.oid, principal, key_path, identity_path }));
    return 0;
}

/// The public key currently registered for `principal` in this repository,
/// if any. A present-but-malformed ref is an error — a missing/non-string
/// `public_key` must not silently skip the rotation guard.
fn registered_public_key(c: *Ctx, repo: []const u8, principal: []const u8) !?[]const u8 {
    const ref_name = try std.fmt.allocPrint(c.alloc, "refs/collab/meta/principals/{s}", .{principal});
    const blob = gitx.cat_blob(c.alloc, c.io, repo, ref_name) catch return null;
    const v = std.json.parseFromSliceLeaky(Value, c.alloc, blob, .{}) catch return error.BadRegistration;
    const o = switch (v) {
        .object => |o| o,
        else => return error.BadRegistration,
    };
    return switch (o.get("public_key") orelse return error.BadRegistration) {
        .string => |s| s,
        else => error.BadRegistration,
    };
}

const PublishResult = struct { ref_name: []const u8, oid: []const u8 };

/// Write/replace the registration blob and its ref (§4.3). Rotation pushes
/// a forced refspec (`+`): the update is a forced one by construction — the
/// signer rotated the key — and the ref points at a blob, not a commit.
fn publish_principal(c: *Ctx, repo: []const u8, principal: []const u8, public_key: []const u8, push: ?[]const u8) !PublishResult {
    var o = jsonio.new_object();
    jsonio.obj_put(&o, c.alloc, "version", .{ .integer = 1 });
    jsonio.obj_put(&o, c.alloc, "principal", .{ .string = principal });
    jsonio.obj_put(&o, c.alloc, "public_key", .{ .string = public_key });
    jsonio.obj_put(&o, c.alloc, "registered_at", .{ .integer = util.now_ts() });
    const content = jsonio.to_string_pretty(c.alloc, .{ .object = o });
    const oid = try gitx.write_blob(c.alloc, c.io, repo, content);
    const ref_name = try std.fmt.allocPrint(c.alloc, "refs/collab/meta/principals/{s}", .{principal});
    try gitx.update_ref(c.alloc, c.io, repo, ref_name, oid);
    if (push) |remote| {
        const spec = try std.fmt.allocPrint(c.alloc, "+{s}", .{ref_name});
        try gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = &.{spec} });
    }
    return .{ .ref_name = ref_name, .oid = oid };
}

pub fn principal_register(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const resolved = try identity.resolve_identity(c.alloc, c.io, repo, p.opt("principal"), p.opt("key"));
    try identity.ref_segment("principal", resolved.principal);
    const key = identity.read_signing_key(c.alloc, c.io, resolved.key_path) catch return error.BadKey;
    const public_key = entry_mod.public_key_b64(c.alloc, key);
    const result = try publish_principal(c, repo, resolved.principal, public_key, p.opt("push"));
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} {s}", .{ result.ref_name, result.oid }));
    return 0;
}

pub fn principal_revoke(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const principal = try p.required("principal");
    try identity.ref_segment("principal", principal);
    const ref_name = try std.fmt.allocPrint(c.alloc, "refs/collab/meta/principals/{s}", .{principal});
    try gitx.update_ref(c.alloc, c.io, repo, ref_name, null);
    if (p.opt("push")) |remote| {
        // The tombstone is a ref deletion; deleting the local ref first
        // leaves no source for a bare push, so send the deletion refspec.
        const spec = try std.fmt.allocPrint(c.alloc, ":{s}", .{ref_name});
        try gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = &.{spec} });
    }
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} revoked", .{ref_name}));
    return 0;
}

pub fn principal_fetch(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const remote_url = gitx.remote_url(c.alloc, c.io, repo, remote) catch return error.RemoteUrlFailed;
    const root = gitx.host_root(c.alloc, remote_url) catch {
        std.debug.print("wgl: remote URL \"{s}\" has no scheme\n", .{remote_url});
        return error.NoScheme;
    };
    const url = try std.fmt.allocPrint(c.alloc, "{s}/api/v1/principals", .{root});
    var headers: util.List(std.http.Header) = .init(c.alloc);
    defer headers.deinit();
    var auth_buf: ?[]u8 = null;
    if (c.token_opt(p.opt("token"))) |t| {
        auth_buf = try std.fmt.allocPrint(c.alloc, "Bearer {s}", .{t});
        try headers.append(.{ .name = "authorization", .value = auth_buf.? });
    }
    try headers.append(.{ .name = "accept", .value = "application/json" });
    const resp = httpc.request(c.alloc, c.io, .GET, url, null, headers.items) catch |err| {
        std.debug.print("wgl: GET {s} failed: {t}\n", .{ url, err });
        return err;
    };
    if (resp.status / 100 != 2) {
        std.debug.print("wgl: GET {s} -> {d}: {s}\n", .{ url, resp.status, resp.body });
        return error.HttpFailed;
    }
    const v = std.json.parseFromSliceLeaky(Value, c.alloc, resp.body, .{}) catch return error.BadRegistryJson;
    const o = switch (v) {
        .object => |o| o,
        else => return error.BadRegistryJson,
    };

    // Purge cached principals the host no longer lists — otherwise the
    // offline cache would keep trusting a revoked key.
    const cached = try gitx.for_each_ref(c.alloc, c.io, repo, &.{"refs/walgit/principals"});
    for (cached) |tip| {
        const principal = tip.name[entry_mod.HOST_PRINCIPALS_PREFIX.len..];
        if (!o.contains(principal)) {
            try gitx.update_ref(c.alloc, c.io, repo, tip.name, null);
        }
    }

    var n: usize = 0;
    var it = o.iterator();
    while (it.next()) |kv| {
        const principal = kv.key_ptr.*;
        const public_key = switch (kv.value_ptr.*) {
            .string => |s| s,
            else => continue,
        };
        try identity.ref_segment("principal", principal);
        var doc = jsonio.new_object();
        jsonio.obj_put(&doc, c.alloc, "version", .{ .integer = 1 });
        jsonio.obj_put(&doc, c.alloc, "principal", .{ .string = principal });
        jsonio.obj_put(&doc, c.alloc, "public_key", .{ .string = public_key });
        jsonio.obj_put(&doc, c.alloc, "registered_at", .{ .integer = 0 });
        const content = jsonio.to_string_pretty(c.alloc, .{ .object = doc });
        const oid = try gitx.write_blob(c.alloc, c.io, repo, content);
        const ref_name = try std.fmt.allocPrint(c.alloc, "refs/walgit/principals/{s}", .{principal});
        try gitx.update_ref(c.alloc, c.io, repo, ref_name, oid);
        n += 1;
    }
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "cached {d} host principal(s) under refs/walgit/principals/*", .{n}));
    return 0;
}

// ---- card claims (§8.4) ---------------------------------------------------------------------

const CLAIM_STAND_DOWN_EXIT: u8 = 2;

pub fn claim(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const id = try p.required("id");
    const remote = p.get("push", "origin");
    const ttl = p.u64_of("ttl", collab.CARD_CLAIM_TTL_DEFAULT_SECS) catch return error.BadTtl;
    const format = p.get("format", "text");
    if (ttl < collab.CARD_CLAIM_TTL_MIN_SECS or ttl > collab.CARD_CLAIM_TTL_MAX_SECS) {
        std.debug.print("wgl: --ttl {d} outside [{d}, {d}] seconds\n", .{ ttl, collab.CARD_CLAIM_TTL_MIN_SECS, collab.CARD_CLAIM_TTL_MAX_SECS });
        return error.BadTtl;
    }
    if (!std.mem.eql(u8, format, "text") and !std.mem.eql(u8, format, "json")) {
        std.debug.print("wgl: unknown claim format {s} (text|json)\n", .{format});
        return error.BadFormat;
    }
    const resolved = try identity.resolve_identity(c.alloc, c.io, repo, p.opt("actor"), p.opt("key"));
    try identity.ref_segment("claim.actor", resolved.principal);
    const session: ?[]const u8 = blk: {
        if (p.opt("session")) |s| {
            const t = std.mem.trim(u8, s, " \t");
            if (t.len > 0) {
                if (t.len > 128) {
                    std.debug.print("wgl: --session is {d} bytes, over the 128-byte cap\n", .{t.len});
                    return error.SessionTooLong;
                }
                break :blk t;
            }
        }
        break :blk c.session();
    };
    const key = identity.read_signing_key(c.alloc, c.io, resolved.key_path) catch return error.BadKey;

    // Step 1: read the log as the remote has it now.
    try gitx.fetch_collab(c.alloc, c.io, repo, remote);
    try collabreader.ensure_actor_key_binding(c.alloc, c.io, repo, resolved.principal, resolved.key_path, key);
    const view_now = try claim_view_now(c, repo, id);
    if (!collab.card_claim_governs(view_now.status)) {
        std.debug.print("wgl: card {s} is {s} — not claimable; only open/in-progress cards take claims\n", .{ id, view_now.status });
        return error.NotClaimable;
    }
    // Restart recovery is scoped to one session (§8.4): the remembered
    // claim must still be the live winner, by this identity, with the same
    // session.
    if (session) |s| {
        if (read_claim_state(c, repo, id, s)) |prev| {
            if (std.mem.eql(u8, prev.session, s)) {
                if (view_now.view.winner) |winner| {
                    if (std.mem.eql(u8, winner.oid, prev.claim) and
                        std.mem.eql(u8, winner.actor, resolved.principal) and
                        std.mem.eql(u8, winner.session, s))
                    {
                        print_claim(c, format, true, id, winner.actor, winner.oid, winner.expires_at(), true);
                        return 0;
                    }
                }
            }
        }
    }
    if (view_now.view.winner) |winner| {
        return stand_down(c, id, &winner, format);
    }

    // Step 2: publish the claim — a root entry, parent "" (a new round of
    // the race, not a reply).
    var body = jsonio.new_object();
    jsonio.obj_put(&body, c.alloc, "ttl", .{ .integer = @intCast(ttl) });
    const opt_fields = [_]struct { name: []const u8, value: []const u8 }{
        .{ .name = "worktree", .value = p.get("worktree", "") },
        .{ .name = "branch", .value = p.get("branch", "") },
        .{ .name = "work", .value = p.get("work", "") },
        .{ .name = "session", .value = session orelse "" },
    };
    for (opt_fields) |f| {
        if (f.value.len > 0) {
            jsonio.obj_put(&body, c.alloc, f.name, .{ .string = f.value });
        }
    }
    var e = Entry{
        .version = 1,
        .kind = entry_mod.CARD_CLAIM_KIND,
        .id = id,
        .actor = resolved.principal,
        .ts = util.now_ts(),
        .parent = "",
        .refs = null,
        .body = .{ .object = body },
        .sig = "",
    };
    entry_mod.sign_entry(c.alloc, &e, key);
    const content = entry_mod.entry_json(c.alloc, &e, true);
    if (content.len > entry_mod.COLLAB_ENTRY_MAX_BYTES) {
        std.debug.print("wgl: claim entry is {d} bytes, over the {d}-byte collab entry cap\n", .{ content.len, entry_mod.COLLAB_ENTRY_MAX_BYTES });
        return error.EntryTooBig;
    }
    const oid = try gitx.write_blob(c.alloc, c.io, repo, content);
    // Remember the oid before the push: a kill between push and re-read
    // would otherwise orphan a winning claim until the TTL.
    if (session) |s| {
        save_claim_state(c, repo, id, s, oid);
    }
    // Push by oid, then point the local ref at it: a failed push leaves no
    // local-only claim ref behind to pollute the next attempt's view.
    const ref_name = try std.fmt.allocPrint(c.alloc, "refs/collab/inbox/{s}/{s}", .{ resolved.principal, identity.entry_uuid(c.alloc, c.io) });
    const spec = try std.fmt.allocPrint(c.alloc, "{s}:{s}", .{ oid, ref_name });
    try gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = &.{spec} });
    try gitx.update_ref(c.alloc, c.io, repo, ref_name, oid);

    // Step 3: the convergence point — re-read; only the deterministic
    // winner may start work.
    try gitx.fetch_collab(c.alloc, c.io, repo, remote);
    const after = try claim_view_now(c, repo, id);
    if (!collab.card_claim_governs(after.status)) {
        std.debug.print("wgl: card {s} became {s} while the claim was publishing — not claimable; the claim stays in the log\n", .{ id, after.status });
        return error.NotClaimable;
    }
    if (after.view.winner) |w2| {
        if (std.mem.eql(u8, w2.oid, oid)) {
            print_claim(c, format, true, id, resolved.principal, oid, w2.expires_at(), false);
            return 0;
        }
        return stand_down(c, id, &w2, format);
    }
    // Not the winner and not stood down: ours never entered the verified
    // set, or expired between publish and re-read.
    for (after.view.claims) |cl| {
        if (std.mem.eql(u8, cl.oid, oid)) {
            std.debug.print("wgl: claim {s} expired between publish and re-read; retry with a larger --ttl\n", .{oid});
            return error.ClaimExpired;
        }
    }
    std.debug.print("wgl: claim {s} was published but is not a live verified claim — check that principal {s} is registered and --key matches its registered public key\n", .{ oid, resolved.principal });
    return error.ClaimNotVerified;
}

const ClaimNow = struct {
    view: collab.CardClaimView,
    status: []const u8,
};

fn claim_view_now(c: *Ctx, repo: []const u8, id: []const u8) !ClaimNow {
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    var group: util.List(EntryRef) = .init(c.alloc);
    for (loaded.entries) |e| {
        if (std.mem.eql(u8, e.entry.id, id)) try group.append(e);
    }
    if (group.items.len == 0) {
        std.debug.print("wgl: no card with id \"{s}\" in this checkout (is the id right, and are refs/collab/* fetched?)\n", .{id});
        return error.NoCard;
    }
    var has_card = false;
    for (group.items) |e| {
        if (!collab.non_card_kind(e.entry.kind)) has_card = true;
    }
    if (!has_card) {
        std.debug.print("wgl: thread \"{s}\" has no work-card entries — a CI run or bare claims are not claimable\n", .{id});
        return error.NotACard;
    }
    const ordered = try collab.thread(c.alloc, group.items);
    return .{
        .view = try collab.card_claim_view(c.alloc, group.items, loaded.principals, util.now_ts()),
        .status = collab.card_status(ordered),
    };
}

fn print_claim(c: *Ctx, format: []const u8, claimed: bool, id: []const u8, holder: []const u8, oid: []const u8, expires_at: i64, resumed: bool) void {
    if (std.mem.eql(u8, format, "json")) {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, c.alloc, "claimed", .{ .bool = claimed });
        jsonio.obj_put(&o, c.alloc, "id", .{ .string = id });
        jsonio.obj_put(&o, c.alloc, "owner", .{ .string = holder });
        jsonio.obj_put(&o, c.alloc, "claim", .{ .string = oid });
        jsonio.obj_put(&o, c.alloc, "expires_at", .{ .integer = expires_at });
        jsonio.obj_put(&o, c.alloc, "resumed", .{ .bool = resumed });
        c.stdout_line(jsonio.to_string(c.alloc, .{ .object = o }));
        return;
    }
    if (claimed) {
        c.stdout_line(std.fmt.allocPrint(c.alloc, "{s} {s} by {s} until {d} (claim {s})", .{ if (resumed) "already claimed" else "claimed", id, holder, expires_at, oid }) catch @panic("oom"));
    } else {
        c.stdout_line(std.fmt.allocPrint(c.alloc, "stand-down: card {s} is held by {s} until {d} (claim {s}); do not start work", .{ id, holder, expires_at, oid }) catch @panic("oom"));
    }
}

fn stand_down(c: *Ctx, id: []const u8, winner: *const collab.CardClaim, format: []const u8) u8 {
    print_claim(c, format, false, id, winner.actor, winner.oid, winner.expires_at(), false);
    if (std.mem.eql(u8, format, "json")) {
        // JSON mode's stdout is the machine channel; the human-readable
        // stand-down signal goes to stderr.
        std.debug.print("stand-down: card {s} is held by {s} until {d} (claim {s}); do not start work\n", .{ id, winner.actor, winner.expires_at(), winner.oid });
    }
    return CLAIM_STAND_DOWN_EXIT;
}

const ClaimState = struct { claim: []const u8, session: []const u8 };

/// `<git-dir>/walgit/claims/<sha256(id, session)>` — process-local,
/// disposable state (losing it costs a TTL wait, nothing else).
fn claim_state_path(c: *Ctx, repo: []const u8, id: []const u8, session: []const u8) ![]u8 {
    const dir = try identity.project_identity_dir(c.alloc, c.io, repo);
    var digest: [32]u8 = undefined;
    {
        var h = std.crypto.hash.sha2.Sha256.init(.{});
        h.update(id);
        h.update("\u{1f}");
        h.update(session);
        h.final(&digest);
    }
    const name = util.to_hex(c.alloc, &digest);
    return std.fmt.allocPrint(c.alloc, "{s}/claims/{s}", .{ dir, name });
}

fn read_claim_state(c: *Ctx, repo: []const u8, id: []const u8, session: []const u8) ?ClaimState {
    const path = claim_state_path(c, repo, id, session) catch return null;
    var d = std.Io.Dir.cwd();
    const raw = d.readFileAlloc(c.io, path, c.alloc, .limited(1 << 20)) catch return null;
    const v = std.json.parseFromSliceLeaky(Value, c.alloc, std.mem.trim(u8, raw, " \t\r\n"), .{}) catch return null;
    const o = switch (v) {
        .object => |o| o,
        else => return null,
    };
    const claim_oid = switch (o.get("claim") orelse return null) {
        .string => |s| s,
        else => return null,
    };
    const sess = switch (o.get("session") orelse return null) {
        .string => |s| s,
        else => return null,
    };
    return .{ .claim = claim_oid, .session = sess };
}

fn save_claim_state(c: *Ctx, repo: []const u8, id: []const u8, session: []const u8, claim_oid: []const u8) void {
    const path = claim_state_path(c, repo, id, session) catch |err| {
        std.debug.print("warning: could not record the claim state ({t}); a restart re-claims after the TTL\n", .{err});
        return;
    };
    var o = jsonio.new_object();
    jsonio.obj_put(&o, c.alloc, "claim", .{ .string = claim_oid });
    jsonio.obj_put(&o, c.alloc, "session", .{ .string = session });
    const doc = jsonio.to_string(c.alloc, .{ .object = o });
    identity.write_private_file(c.alloc, c.io, path, doc) catch |err| {
        std.debug.print("warning: could not record the claim state ({t}); a restart re-claims after the TTL\n", .{err});
    };
}

// ---- gc: fold the inbox into the signed snapshot (§9) --------------------------------------

pub fn gc(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const resolved = try identity.resolve_identity(c.alloc, c.io, repo, p.opt("actor"), p.opt("key"));
    try gc_inner(c, repo, resolved.principal, resolved.key_path, p.opt("push"), p.boolean("truncate"));
    return 0;
}

const GC_DELETE_CHUNK: usize = 500;

fn gc_inner(c: *Ctx, repo: []const u8, actor: []const u8, key_path: []const u8, push: ?[]const u8, truncate: bool) !void {
    try identity.ref_segment("gc.actor", actor);
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const format = try gitx.object_format(c.alloc, c.io, repo);
    const obj_format: entry_mod.ObjectFormat = switch (format) {
        .sha1 => .sha1,
        .sha256 => .sha256,
    };
    // The snapshot's own signature is only verifiable against a *registered*
    // key — folding as an unregistered principal would strand every reader.
    const principals = try reader.principals();
    const registered = principals.get(actor) orelse {
        std.debug.print(
            "wgl: gc.actor {s} has no registered collab key in the registry visible to this checkout (local ∪ host cache); if {s} is registered on the host, run `wgl collab principal-fetch` first; otherwise register the seed you already hold with `wgl collab principal-register --principal {s} --key <keyfile> --push origin` — without `--push origin` the registration stays local and the snapshot stays unverified on the remote\n",
            .{ actor, actor, actor },
        );
        return error.ActorUnregistered;
    };
    const key = identity.read_signing_key(c.alloc, c.io, key_path) catch return error.BadKey;
    {
        var buf: [64]u8 = undefined;
        const n = std.base64.standard.Decoder.calcSizeForSlice(std.mem.trim(u8, registered, " \t\r\n")) catch return error.BadKey;
        if (n != 32) return error.BadKey;
        std.base64.standard.Decoder.decode(buf[0..32], std.mem.trim(u8, registered, " \t\r\n")) catch return error.BadKey;
        if (!std.mem.eql(u8, buf[0..32], &key.public_key.bytes)) {
            std.debug.print(
                "wgl: gc.key does not match the public key registered for gc.actor {s} in the registry visible to this checkout (local ∪ host cache): this signature does not verify until {s} is rotated to this key, and the collab log is append-only, so the entry cannot be deleted or edited. Use the key registered for {s}, or rotate {s} on purpose with `wgl collab principal-register --principal {s} --key <keyfile> --push origin`; rotation is destructive, because every past entry signed by {s} turns unverified\n",
                .{ actor, actor, actor, actor, actor, actor },
            );
            return error.KeyMismatch;
        }
    }

    // The baseline the snapshot push leases against (§9.3): the snapshot
    // ref's value as this gc read it. None = an explicitly empty expect.
    var baseline: ?[]const u8 = null;
    var records: util.List(snapshot_mod.SnapshotRecord) = .init(c.alloc);
    var baseline_len: usize = 0;
    if (try reader.snapshot_blob()) |snap| {
        baseline_len = snap.bytes.len;
        baseline = snap.oid;
        const parsed = snapshot_mod.parse_snapshot(c.alloc, snap.bytes) catch |e| {
            std.debug.print("wgl: {s}: snapshot document rejected ({t})\n", .{ entry_mod.SNAPSHOT_REF, e });
            return error.BadSnapshot;
        };
        try records.append_slice(parsed.entries);
    }

    var seen = std.StringHashMap(usize).init(c.alloc);
    for (records.items, 0..) |rec, i| {
        if (!seen.contains(rec.oid)) try seen.put(rec.oid, i);
    }
    var folded: util.List([]const u8) = .init(c.alloc);
    var changed = false;
    var left: usize = 0;
    const inbox = try reader.inbox_refs();
    var in_oids: util.List([]const u8) = .init(c.alloc);
    for (inbox) |tip| try in_oids.append(tip.oid);
    const blobs = try reader.blobs_batch(in_oids.items, null);
    for (inbox) |tip| {
        const blob = blobs.get(tip.oid) orelse {
            left += 1;
            continue;
        };
        const e = entry_mod.parse_entry_arena(c.alloc, blob) catch {
            left += 1;
            continue;
        };
        const slash = std.mem.lastIndexOfScalar(u8, tip.name, '/') orelse {
            left += 1;
            continue;
        };
        const owner_start = std.mem.lastIndexOfScalar(u8, tip.name[0..slash], '/') orelse {
            left += 1;
            continue;
        };
        const principal = tip.name[owner_start + 1 .. slash];
        changed = (keep_fold_record(c.alloc, &records, &seen, .{
            .oid = tip.oid,
            .principal = principal,
            .json = blob,
        }, e.actor, obj_format) catch false) or changed;
        try folded.append(tip.name);
    }

    // Repair path: an over-cap snapshot with an empty inbox tail must still
    // be truncatable (§9.3).
    const over_cap = baseline_len > entry_mod.COLLAB_SNAPSHOT_MAX_BYTES;
    if (folded.items.len == 0 and !(over_cap and truncate)) {
        if (over_cap) {
            std.debug.print("wgl: collab gc: {s} is {d} MiB, over the {d} MiB cap; re-run with --truncate to drop the oldest records (marked complete:false)\n", .{ entry_mod.SNAPSHOT_REF, baseline_len / (1024 * 1024), entry_mod.COLLAB_SNAPSHOT_MAX_BYTES / (1024 * 1024) });
            return error.SnapshotOverCap;
        }
        const msg = std.fmt.allocPrint(c.alloc, "collab gc: nothing to fold ({d} unparseable inbox blob(s) left in place)", .{left}) catch @panic("oom");
        c.stdout_line(msg);
        return;
    }

    // Nothing new ⇒ prune-only fold: the snapshot is never rebuilt (a
    // rebuild would differ only in ts — pure ref churn).
    var snap_oid: ?[]const u8 = null;
    if (changed or over_cap) {
        const ts = util.now_ts();
        const built = try build_capped_snapshot(c.alloc, actor, ts, records.items, key, truncate, entry_mod.COLLAB_SNAPSHOT_MAX_BYTES);
        if (!built.snap.complete) {
            std.debug.print("wgl: collab gc: WARNING snapshot truncated: dropped {d} oldest record(s); the ledger is now marked complete=false\n", .{built.snap.dropped_entries});
        }
        snap_oid = try gitx.write_blob(c.alloc, c.io, repo, built.text);
    }

    if (push == null) {
        if (snap_oid) |oid| {
            try gitx.update_ref(c.alloc, c.io, repo, entry_mod.SNAPSHOT_REF, oid);
        }
        for (folded.items) |name| {
            try gitx.update_ref(c.alloc, c.io, repo, name, null);
        }
    } else {
        const remote = push.?;
        // Snapshot first — a CAS from the baseline this gc actually read;
        // a concurrent fold that moved the ref fails the lease and the gc
        // retries, never overwrites (§9.3).
        const snapshot_to_push = snap_oid orelse baseline orelse {
            std.debug.print("wgl: cannot prune a remote without a snapshot: fetch the remote and retry\n", .{});
            return error.NoSnapshotBaseline;
        };
        const lease = try std.fmt.allocPrint(c.alloc, "{s}:{s}", .{ entry_mod.SNAPSHOT_REF, baseline orelse "" });
        const spec = try std.fmt.allocPrint(c.alloc, "{s}:{s}", .{ snapshot_to_push, entry_mod.SNAPSHOT_REF });
        gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = &.{spec}, .leases = &.{lease} }) catch |err| {
            std.debug.print("wgl: push the snapshot (the fold baseline moved — a concurrent gc? fetch and retry): {t}\n", .{err});
            return err;
        };
        // Prune only refs the remote still advertises; a concurrent gc may
        // have pruned some already, and stock git refuses a delete whose
        // ref is not advertised — that refusal is convergence, not an error.
        const live = try gitx.ls_remote(c.alloc, c.io, repo, remote);
        var deletes: util.List(gitx.RefTip) = .init(c.alloc);
        for (folded.items) |name| {
            for (live) |l| {
                if (std.mem.eql(u8, l.name, name)) {
                    try deletes.append(.{ .name = name, .oid = l.oid });
                    break;
                }
            }
        }
        var i: usize = 0;
        while (i < deletes.items.len) {
            const end = @min(i + GC_DELETE_CHUNK, deletes.items.len);
            var specs: util.List([]const u8) = .init(c.alloc);
            var leases: util.List([]const u8) = .init(c.alloc);
            for (deletes.items[i..end]) |d| {
                const ds = try std.fmt.allocPrint(c.alloc, ":{s}", .{d.name});
                const dl = try std.fmt.allocPrint(c.alloc, "{s}:{s}", .{ d.name, d.oid });
                try specs.append(ds);
                try leases.append(dl);
            }
            gitx.push(c.alloc, c.io, repo, remote, .{ .refspecs = specs.items, .leases = leases.items }) catch |err| {
                std.debug.print("wgl: delete folded inbox refs; re-run `wgl collab gc` to converge (idempotent): {t}\n", .{err});
                return err;
            };
            i = end;
        }
        // Mirror the fold locally so this checkout aggregates the folded
        // shape immediately.
        if (snap_oid) |oid| {
            try gitx.update_ref(c.alloc, c.io, repo, entry_mod.SNAPSHOT_REF, oid);
        }
        for (folded.items) |name| {
            try gitx.update_ref(c.alloc, c.io, repo, name, null);
        }
    }
    const oid_text: []const u8 = snap_oid orelse (baseline orelse "new");
    const msg = std.fmt.allocPrint(c.alloc, "collab gc: folded {d} inbox ref(s) into {s} ({s}); {d} unparseable left in place", .{ folded.items.len, entry_mod.SNAPSHOT_REF, oid_text, left }) catch @panic("oom");
    c.stdout_line(msg);
}

/// Record one entry in a snapshot fold, preferring the copy whose inbox
/// principal is the entry's own actor (§7.1's rule). Returns whether the
/// records changed (a duplicate that adds no better copy is prune-only).
fn keep_fold_record(alloc: std.mem.Allocator, records: *util.List(snapshot_mod.SnapshotRecord), seen: *std.StringHashMap(usize), record: snapshot_mod.SnapshotRecord, entry_actor: []const u8, format: entry_mod.ObjectFormat) !bool {
    if (seen.get(record.oid)) |index| {
        const cur = &records.items[index];
        const cur_parses = cur.entry_ref(alloc, format) != null;
        const record_parses = record.entry_ref(alloc, format) != null;
        const replace = !cur_parses or !record_parses or
            (!std.mem.eql(u8, cur.principal, entry_actor) and std.mem.eql(u8, record.principal, entry_actor));
        if (replace) records.items[index] = record;
        return replace;
    }
    try seen.put(record.oid, records.items.len);
    try records.append(record);
    return true;
}

const BuiltSnapshot = struct { snap: snapshot_mod.Snapshot, text: []const u8 };

/// Build the fold under the snapshot byte cap: over the cap the fold is
/// refused (pointing at `--truncate`), unless truncation was requested —
/// then the oldest records (by entry ts, unparseable first) are dropped
/// until the document fits and the result is marked `complete: false`.
fn build_capped_snapshot(alloc: std.mem.Allocator, actor: []const u8, ts: i64, records_in: []snapshot_mod.SnapshotRecord, key: entry_mod.SigningKey, truncate: bool, max_bytes: usize) !BuiltSnapshot {
    var records = alloc.dupe(snapshot_mod.SnapshotRecord, records_in) catch return error.OutOfMemory;
    var snap = snapshot_mod.build_snapshot(alloc, actor, ts, records, true, 0, key);
    var text = snap.to_json(alloc);
    if (text.len <= max_bytes) {
        return .{ .snap = snap, .text = text };
    }
    if (!truncate) {
        std.debug.print("wgl: collab gc: folded snapshot would exceed {d} MiB; re-run with --truncate to drop the oldest records (marked complete:false), or raise the server cap\n", .{entry_mod.COLLAB_SNAPSHOT_MAX_BYTES / (1024 * 1024)});
        return error.SnapshotOverCap;
    }
    // Precompute each record's entry ts (unparseable → the oldest slot),
    // then sort indices — the comparator carries both slices as context.
    const ts_list = alloc.alloc(i64, records.len) catch return error.OutOfMemory;
    for (records, 0..) |rec, i| {
        ts_list[i] = blk: {
            var arena_state = std.heap.ArenaAllocator.init(alloc);
            defer arena_state.deinit();
            const e = entry_mod.parse_entry_arena(arena_state.allocator(), rec.json) catch break :blk std.math.minInt(i64);
            break :blk e.ts;
        };
    }
    const SortCtx = struct {
        ts_list: []const i64,
        records: []const snapshot_mod.SnapshotRecord,
        fn less(self: @This(), a: usize, b: usize) bool {
            if (self.ts_list[a] != self.ts_list[b]) return self.ts_list[a] < self.ts_list[b];
            return std.mem.order(u8, self.records[a].oid, self.records[b].oid) == .lt;
        }
    };
    const order = alloc.alloc(usize, records.len) catch return error.OutOfMemory;
    for (order, 0..) |*x, i| x.* = i;
    std.mem.sort(usize, order, SortCtx{ .ts_list = ts_list, .records = records }, SortCtx.less);
    const sorted_records = alloc.alloc(snapshot_mod.SnapshotRecord, records.len) catch return error.OutOfMemory;
    for (order, 0..) |src, i| sorted_records[i] = records[src];
    records = sorted_records;
    var dropped: u64 = 0;
    var drained: usize = 0;
    while (true) {
        snap = snapshot_mod.build_snapshot(alloc, actor, ts, records, false, dropped, key);
        text = snap.to_json(alloc);
        if (text.len <= max_bytes or records.len == 0) {
            return .{ .snap = snap, .text = text };
        }
        // Rendered size is monotone in the record set: drop the oldest
        // records until their serialized weight covers the overflow.
        const overflow = text.len - max_bytes;
        var freed: usize = 0;
        while (drained < records.len and freed < overflow) {
            const rec = records[drained];
            freed += rec.json.len + rec.oid.len + rec.principal.len + 64;
            drained += 1;
        }
        if (drained == 0) drained = 1; // defensive: always make progress
        const remaining = records.len - drained;
        std.mem.copyForwards(snapshot_mod.SnapshotRecord, records[0..remaining], records[drained..]);
        records = records[0..remaining];
        dropped += drained;
        drained = 0;
    }
}

// ---- watch (§11.1) -------------------------------------------------------------------------

pub fn watch(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const interval = p.u64_of("interval", 10) catch 10;
    const once = p.boolean("once");
    const exec = p.opt("exec");
    const state_file = blk: {
        if (p.opt("state")) |s| break :blk c.alloc.dupe(u8, s) catch @panic("oom");
        const dir = try gitx.absolute_git_dir(c.alloc, c.io, repo);
        break :blk std.fs.path.join(c.alloc, &.{ dir, "collab-watch.json" }) catch @panic("oom");
    };
    while (true) {
        try gitx.fetch_collab(c.alloc, c.io, repo, remote);
        const cur = try refs_map(c, repo);
        const prev = read_watch_state(c, state_file);
        const changed = try changed_refs(c.alloc, prev, &cur);
        for (changed) |ev_pair| {
            const ev = try describe_ref(c, repo, ev_pair.name, ev_pair.oid);
            if (exec) |cmd| {
                var child_env = std.process.Environ.Map.init(c.alloc);
                defer child_env.deinit();
                var it = c.env.iterator();
                while (it.next()) |kv| {
                    child_env.put(c.alloc.dupe(u8, kv.key_ptr.*) catch @panic("oom"), c.alloc.dupe(u8, kv.value_ptr.*) catch @panic("oom")) catch {};
                }
                child_env.put("WALGIT_COLLAB_REF", ev_pair.name) catch {};
                child_env.put("WALGIT_COLLAB_KIND", ev.kind) catch {};
                child_env.put("WALGIT_COLLAB_THREAD", ev.thread) catch {};
                child_env.put("WALGIT_COLLAB_ACTOR", ev.actor) catch {};
                child_env.put("WALGIT_COLLAB_VERIFIED", if (ev.verified) "true" else "false") catch {};
                exec_sh(c, cmd, ev.text, &child_env) catch |err| {
                    std.debug.print("wgl: --exec failed: {t}\n", .{err});
                    return error.ExecFailed;
                };
            }
            c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} {s}", .{ ev_pair.name, ev_pair.oid }));
        }
        write_watch_state(c, state_file, &cur) catch |err| {
            std.debug.print("wgl: write watch state: {t}\n", .{err});
        };
        if (once) return 0;
        c.io.sleep(std.Io.Duration.fromSeconds(@intCast(interval)), .real) catch {};
    }
}

fn refs_map(c: *Ctx, repo: []const u8) !std.StringHashMap([]const u8) {
    const tips = try gitx.for_each_ref(c.alloc, c.io, repo, &.{"refs/collab"});
    var map = std.StringHashMap([]const u8).init(c.alloc);
    for (tips) |tip| {
        try map.put(tip.name, tip.oid);
    }
    return map;
}

fn read_watch_state(c: *Ctx, path: []const u8) std.StringHashMap([]const u8) {
    var map = std.StringHashMap([]const u8).init(c.alloc);
    var d = std.Io.Dir.cwd();
    const raw = d.readFileAlloc(c.io, path, c.alloc, .limited(1 << 24)) catch return map;
    const v = std.json.parseFromSliceLeaky(Value, c.alloc, raw, .{}) catch return map;
    const o = switch (v) {
        .object => |o| o,
        else => return map,
    };
    var it = o.iterator();
    while (it.next()) |kv| {
        if (kv.value_ptr.* == .string) {
            map.put(kv.key_ptr.*, kv.value_ptr.*.string) catch {};
        }
    }
    return map;
}

fn write_watch_state(c: *Ctx, path: []const u8, map: *const std.StringHashMap([]const u8)) !void {
    var o = jsonio.new_object();
    var it = map.iterator();
    while (it.next()) |kv| {
        jsonio.obj_put(&o, c.alloc, kv.key_ptr.*, .{ .string = kv.value_ptr.* });
    }
    const doc = jsonio.to_string_pretty(c.alloc, .{ .object = o });
    var d = std.Io.Dir.cwd();
    try d.writeFile(c.io, .{ .sub_path = path, .data = doc });
}

const WatchPair = struct { name: []const u8, oid: []const u8 };

fn changed_refs(alloc: std.mem.Allocator, prev: std.StringHashMap([]const u8), cur: *const std.StringHashMap([]const u8)) ![]WatchPair {
    var out: util.List(WatchPair) = .init(alloc);
    var it = cur.iterator();
    while (it.next()) |kv| {
        if (prev.get(kv.key_ptr.*)) |old| {
            if (std.mem.eql(u8, old, kv.value_ptr.*)) continue;
        }
        try out.append(.{ .name = kv.key_ptr.*, .oid = kv.value_ptr.* });
    }
    std.mem.sort(WatchPair, out.items, {}, struct {
        fn f(_: void, a: WatchPair, b: WatchPair) bool {
            return std.mem.order(u8, a.name, b.name) == .lt;
        }
    }.f);
    return out.to_owned_slice();
}

const RefEvent = struct {
    kind: []const u8,
    actor: []const u8,
    thread: []const u8,
    verified: bool,
    text: []const u8,
};

/// One new/changed collab ref, described for the callback. The fields are
/// taken from the **parsed entry** — never re-parsed out of rendered text:
/// the blob is remote content, and a body containing a literal `\nkind=`
/// line must not be able to forge the signals an agent's `--exec` keys on.
fn describe_ref(c: *Ctx, repo: []const u8, name: []const u8, oid: []const u8) !RefEvent {
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const blob = try gitx.cat_blob(c.alloc, c.io, repo, oid);
    const principals = try reader.principals();
    const text = blob;
    if (std.mem.eql(u8, name, entry_mod.SNAPSHOT_REF)) {
        var actor: []const u8 = "";
        var verified = false;
        if (snapshot_mod.parse_snapshot(c.alloc, text) catch null) |snap| {
            actor = snap.actor;
            if (principals.get(snap.actor)) |k| {
                verified = if (snapshot_mod.verify_snapshot(c.alloc, &snap, k)) |_| true else |_| false;
            }
        }
        return .{ .kind = "snapshot", .actor = actor, .thread = "", .verified = verified, .text = text };
    }
    if (std.mem.startsWith(u8, name, entry_mod.PRINCIPALS_PREFIX)) {
        return .{ .kind = "principal", .actor = name[entry_mod.PRINCIPALS_PREFIX.len..], .thread = "", .verified = true, .text = text };
    }
    const e = entry_mod.parse_entry_arena(c.alloc, text) catch Entry{
        .version = 0,
        .kind = "unknown",
        .id = "",
        .actor = "",
        .ts = 0,
        .parent = "",
        .refs = null,
        .body = .null,
        .sig = "",
    };
    const principal: []const u8 = blk: {
        if (std.mem.startsWith(u8, name, entry_mod.INBOX_PREFIX)) {
            const tail = name[entry_mod.INBOX_PREFIX.len..];
            if (std.mem.lastIndexOfScalar(u8, tail, '/')) |slash| break :blk tail[0..slash];
        }
        break :blk "";
    };
    const er = EntryRef{ .oid = oid, .principal = principal, .entry = e };
    return .{
        .kind = e.kind,
        .actor = e.actor,
        .thread = e.id,
        .verified = er.is_verified(c.alloc, principals),
        .text = text,
    };
}

fn exec_sh(c: *Ctx, cmd: []const u8, stdin_text: []const u8, child_env: *std.process.Environ.Map) !void {
    const shell = if (@import("builtin").os.tag == .windows) &[_][]const u8{ "cmd", "/C", cmd } else &[_][]const u8{ "sh", "-c", cmd };
    var child = std.process.spawn(c.io, .{
        .argv = shell,
        .environ_map = child_env,
        .stdin = .pipe,
        .stdout = .inherit,
        .stderr = .inherit,
    }) catch return error.SpawnFailed;
    {
        var wbuf: [8192]u8 = undefined;
        var w = child.stdin.?.writer(c.io, &wbuf);
        w.interface.writeAll(stdin_text) catch {};
        w.interface.flush() catch {};
    }
    child.stdin.?.close(c.io);
    child.stdin = null;
    const term = try child.wait(c.io);
    switch (term) {
        .exited => |code| if (code != 0) return error.ExecFailed,
        else => return error.ExecFailed,
    }
}
