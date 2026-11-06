//! The D1 entry (`docs/D1_PROTOCOL.md` §5): schema, canonical form, Ed25519
//! signing/verification, the inbox model's verification definition (§4.5), the
//! deduplicated aggregation input (§7.1), and the git blob oid used by the
//! fold (§9). Ported from `walgit-wal/src/collab.rs`.

const std = @import("std");
const jval = std.json;
const jsonio = @import("../jsonio.zig");
const util = @import("../util.zig");

pub const Value = jval.Value;
pub const Principals = std.StringHashMap([]const u8);

// ---- refs layout constants (docs/D1_PROTOCOL.md §3) ---------------------------------------------

pub const SNAPSHOT_REF = "refs/collab/meta/snapshot";
pub const BOARD_PATH = ".walgit/board.toml";
pub const CARD_CLAIM_KIND = "card_claim";
pub const INBOX_PREFIX = "refs/collab/inbox/";
pub const PRINCIPALS_PREFIX = "refs/collab/meta/principals/";
pub const HOST_PRINCIPALS_PREFIX = "refs/walgit/principals/";

/// Upper bound for one collab entry blob read through aggregation (§13).
pub const COLLAB_ENTRY_MAX_BYTES: usize = 256 * 1024;
/// Upper bound for one repository's folded snapshot (§9.4).
pub const COLLAB_SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;

// ---- object format and the git blob oid ------------------------------------------------------------

pub const ObjectFormat = enum {
    sha1,
    sha256,

    pub fn hash_bytes(self: ObjectFormat, bytes: []const []const u8) [64]u8 {
        var out: [64]u8 = undefined;
        switch (self) {
            .sha1 => {
                var h = std.crypto.hash.Sha1.init(.{});
                for (bytes) |b| h.update(b);
                var d: [20]u8 = undefined;
                h.final(&d);
                @memcpy(out[0..20], &d);
            },
            .sha256 => {
                var h = std.crypto.hash.sha2.Sha256.init(.{});
                for (bytes) |b| h.update(b);
                var d: [32]u8 = undefined;
                h.final(&d);
                @memcpy(out[0..32], &d);
            },
        }
        return out;
    }

    pub fn oid_len(self: ObjectFormat) usize {
        return switch (self) {
            .sha1 => 40,
            .sha256 => 64,
        };
    }
};

/// The git blob id of raw bytes: `<hash>("blob <len>\0" + bytes)` — how a
/// fold record's declared oid is pinned to its bytes (§9.2).
pub fn git_blob_oid(alloc: std.mem.Allocator, bytes: []const u8, format: ObjectFormat) []u8 {
    var len_buf: [32]u8 = undefined;
    const header = std.fmt.bufPrint(&len_buf, "blob {d}\x00", .{bytes.len}) catch unreachable;
    const digest = format.hash_bytes(&.{ header, bytes });
    return util.to_hex(alloc, digest[0 .. format.oid_len() / 2]);
}

// ---- the entry (§5.1) -------------------------------------------------------------------------

pub const Refs = struct {
    base: ?[]const u8 = null,
    head: ?[]const u8 = null,
};

pub const Entry = struct {
    version: u32 = 1,
    kind: []const u8,
    id: []const u8,
    actor: []const u8,
    ts: i64,
    parent: []const u8,
    refs: ?Refs = null,
    body: Value,
    sig: []const u8 = "",
};

pub const ParseError = error{
    NotAnObject,
    MissingField,
    WrongType,
    OutOfMemory,
};

fn str_field(o: jval.ObjectMap, key: []const u8) ParseError![]const u8 {
    const v = o.get(key) orelse return error.MissingField;
    return switch (v) {
        .string => |s| s,
        else => error.WrongType,
    };
}

fn int_field(o: jval.ObjectMap, key: []const u8) ParseError!i64 {
    const v = o.get(key) orelse return error.MissingField;
    return switch (v) {
        .integer => |i| i,
        else => error.WrongType,
    };
}

/// Parse an entry from its JSON bytes. Mirrors the serde derive: the required
/// fields must be present and typed, `refs` may be absent or null, `sig`
/// defaults to `""`, unknown fields are ignored.
pub fn parse_entry(v: Value) ParseError!Entry {
    const o = switch (v) {
        .object => |o| o,
        else => return error.NotAnObject,
    };
    const version: i64 = int_field(o, "version") catch return error.MissingField;
    if (version < 0 or version > std.math.maxInt(u32)) return error.WrongType;
    var refs: ?Refs = null;
    if (o.get("refs")) |rv| switch (rv) {
        .null => {},
        .object => |ro| {
            var r = Refs{};
            if (ro.get("base")) |bv| switch (bv) {
                .null => {},
                .string => |s| r.base = s,
                else => return error.WrongType,
            };
            if (ro.get("head")) |hv| switch (hv) {
                .null => {},
                .string => |s| r.head = s,
                else => return error.WrongType,
            };
            refs = r;
        },
        else => return error.WrongType,
    };
    const body = o.get("body") orelse return error.MissingField;
    const sig: []const u8 = if (o.get("sig")) |sv| switch (sv) {
        .string => |s| s,
        else => return error.WrongType,
    } else "";
    return .{
        .version = @intCast(version),
        .kind = try str_field(o, "kind"),
        .id = try str_field(o, "id"),
        .actor = try str_field(o, "actor"),
        .ts = try int_field(o, "ts"),
        .parent = try str_field(o, "parent"),
        .refs = refs,
        .body = body,
        .sig = sig,
    };
}

/// Parse an entry from its JSON bytes into `arena` (all strings live there —
/// free the arena, not the fields). Mirrors the serde derive: the required
/// fields must be present and typed, `refs` may be absent or null, `sig`
/// defaults to `""`, unknown fields are ignored (and dropped, exactly like
/// the Rust struct re-serialization did).
pub fn parse_entry_arena(arena: std.mem.Allocator, bytes: []const u8) ParseError!Entry {
    const v = jval.parseFromSliceLeaky(jval.Value, arena, bytes, .{}) catch return error.WrongType;
    return parse_entry(v);
}

// ---- serialization (struct field order; body sorted like serde_json) ---------------------------

fn write_refs(out: *jsonio.Out, refs: Refs, indent: ?usize) void {
    // `{"base": …, "head": …}` — declaration order, absent fields omitted.
    const has_any = refs.base != null or refs.head != null;
    if (!has_any) {
        jsonio.put_raw(out, "{}");
        return;
    }
    jsonio.put_raw(out, "{");
    var first = true;
    if (refs.base) |b| {
        jsonio.write_string(out, "base");
        jsonio.put_raw(out, if (indent != null) ": " else ":");
        jsonio.write_string(out, b);
        first = false;
    }
    if (refs.head) |h| {
        if (!first) jsonio.put_raw(out, ",");
        if (indent) |n| jsonio.newline_indent_pub(out, n + 1);
        jsonio.write_string(out, "head");
        jsonio.put_raw(out, if (indent != null) ": " else ":");
        jsonio.write_string(out, h);
        first = false;
    }
    if (indent) |n| jsonio.newline_indent_pub(out, n);
    jsonio.put_raw(out, "}");
}

/// Write the entry in the struct's field declaration order — the exact bytes
/// `serde_json::to_string[_pretty](&entry)` produces (body objects sorted by
/// key at every depth).
pub fn write_entry(out: *jsonio.Out, e: *const Entry, indent: ?usize) void {
    jsonio.put_raw(out, "{");
    const child_indent: ?usize = if (indent) |n| n + 1 else null;
    const sep = struct {
        fn f(o: *jsonio.Out, first: *bool, ind: ?usize) void {
            if (!first.*) jsonio.put_raw(o, ",");
            first.* = false;
            if (ind) |n| jsonio.newline_indent_pub(o, n + 1);
        }
    }.f;
    var first = true;
    sep(out, &first, indent);
    jsonio.write_string(out, "version");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    var buf: [16]u8 = undefined;
    jsonio.put_raw(out, std.fmt.bufPrint(&buf, "{d}", .{e.version}) catch unreachable);

    sep(out, &first, indent);
    jsonio.write_string(out, "kind");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_string(out, e.kind);

    sep(out, &first, indent);
    jsonio.write_string(out, "id");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_string(out, e.id);

    sep(out, &first, indent);
    jsonio.write_string(out, "actor");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_string(out, e.actor);

    sep(out, &first, indent);
    jsonio.write_string(out, "ts");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    var tsbuf: [24]u8 = undefined;
    jsonio.put_raw(out, std.fmt.bufPrint(&tsbuf, "{d}", .{e.ts}) catch unreachable);

    sep(out, &first, indent);
    jsonio.write_string(out, "parent");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_string(out, e.parent);

    if (e.refs) |r| {
        sep(out, &first, indent);
        jsonio.write_string(out, "refs");
        jsonio.put_raw(out, if (indent != null) ": " else ":");
        write_refs(out, r, child_indent);
    }

    sep(out, &first, indent);
    jsonio.write_string(out, "body");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_value(out, e.body, child_indent);

    sep(out, &first, indent);
    jsonio.write_string(out, "sig");
    jsonio.put_raw(out, if (indent != null) ": " else ":");
    jsonio.write_string(out, e.sig);

    if (indent) |n| jsonio.newline_indent_pub(out, n);
    jsonio.put_raw(out, "}");
}

pub fn entry_json(alloc: std.mem.Allocator, e: *const Entry, pretty: bool) []u8 {
    var out = jsonio.Out.init(alloc);
    write_entry(&out, e, if (pretty) 0 else null);
    return out.to_owned_slice() catch @panic("oom");
}

// ---- canonical form (§5.3) -------------------------------------------------------------------

/// The canonical bytes an entry's signature covers: the entry object with
/// `sig` kept but set to `""`, `refs` omitted when absent, every object's keys
/// sorted, no whitespace.
pub fn entry_canonical(alloc: std.mem.Allocator, e: *const Entry) []u8 {
    var out = jsonio.Out.init(alloc);
    jsonio.put_raw(&out, "{");
    // Keys are emitted directly in sorted order (actor, body, id, kind,
    // parent, refs?, sig, ts, version).
    jsonio.write_string(&out, "actor");
    jsonio.put_raw(&out, ":");
    jsonio.write_string(&out, e.actor);

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "body");
    jsonio.put_raw(&out, ":");
    jsonio.write_value(&out, e.body, null);

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "id");
    jsonio.put_raw(&out, ":");
    jsonio.write_string(&out, e.id);

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "kind");
    jsonio.put_raw(&out, ":");
    jsonio.write_string(&out, e.kind);

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "parent");
    jsonio.put_raw(&out, ":");
    jsonio.write_string(&out, e.parent);

    if (e.refs) |r| {
        jsonio.put_raw(&out, ",");
        jsonio.write_string(&out, "refs");
        jsonio.put_raw(&out, ":");
        write_refs(&out, r, null);
    }

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "sig");
    jsonio.put_raw(&out, ":");
    jsonio.write_string(&out, ""); // the key is kept, the value cleared (§5.3)

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "ts");
    jsonio.put_raw(&out, ":");
    var buf: [24]u8 = undefined;
    jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{e.ts}) catch unreachable);

    jsonio.put_raw(&out, ",");
    jsonio.write_string(&out, "version");
    jsonio.put_raw(&out, ":");
    var vbuf: [16]u8 = undefined;
    jsonio.put_raw(&out, std.fmt.bufPrint(&vbuf, "{d}", .{e.version}) catch unreachable);

    jsonio.put_raw(&out, "}");
    return out.to_owned_slice() catch @panic("oom");
}

// ---- signing and verification (§5.4) ---------------------------------------------------------

pub const SigningKey = std.crypto.sign.Ed25519.KeyPair;
pub const Signature = std.crypto.sign.Ed25519.Signature;

/// The Ed25519 public key bytes of a signing key, base64 (standard alphabet) —
/// the registry's key format (§4.1). Key files (hex seed files) are loaded by
/// `identity.zig`, which owns the I/O side.
pub fn public_key_b64(alloc: std.mem.Allocator, key: SigningKey) []u8 {
    return util.b64_encode(alloc, &key.public_key.bytes);
}

/// Sign: `sig = "ed25519:" + base64(Ed25519_sign(canonical(entry with sig="")))`.
pub fn sign_entry(alloc: std.mem.Allocator, e: *Entry, key: SigningKey) void {
    const canonical = entry_canonical(alloc, e);
    defer alloc.free(canonical);
    const sig = key.sign(canonical, null) catch @panic("ed25519 sign");
    const b64 = util.b64_encode(alloc, &sig.toBytes());
    e.sig = std.fmt.allocPrint(alloc, "ed25519:{s}", .{b64}) catch @panic("oom");
    alloc.free(b64);
}

pub const VerifyError = error{
    SigPrefix,
    BadSigBase64,
    BadSigLength,
    BadKeyBase64,
    BadKeyLength,
    VerificationFailed,
    OutOfMemory,
};

/// Verify an entry against its actor's registered public key (base64). Strict
/// verification (weak public keys and malleable signatures are rejected) —
/// the Rust side's `verify_strict` (§5.4).
pub fn verify_entry(alloc: std.mem.Allocator, e: *const Entry, public_key_b64_str: []const u8) VerifyError!void {
    if (!std.mem.startsWith(u8, e.sig, "ed25519:")) return error.SigPrefix;
    const sig_bytes = util.b64_decode(alloc, e.sig["ed25519:".len..]) catch |err| switch (err) {
        error.OutOfMemory => return error.OutOfMemory,
        else => return error.BadSigBase64,
    };
    defer alloc.free(sig_bytes);
    if (sig_bytes.len != 64) return error.BadSigLength;
    const key_bytes = util.b64_decode(alloc, std.mem.trim(u8, public_key_b64_str, " \t\r\n")) catch |err| switch (err) {
        error.OutOfMemory => return error.OutOfMemory,
        else => return error.BadKeyBase64,
    };
    defer alloc.free(key_bytes);
    if (key_bytes.len != 32) return error.BadKeyLength;
    const canonical = entry_canonical(alloc, e);
    defer alloc.free(canonical);
    const pk = std.crypto.sign.Ed25519.PublicKey.fromBytes(key_bytes[0..32].*) catch return error.BadKeyLength;
    const sig = Signature.fromBytes(sig_bytes[0..64].*);
    var verifier = sig.verifier(pk) catch return error.VerificationFailed;
    verifier.update(canonical);
    verifier.verifyStrict() catch return error.VerificationFailed;
}

// ---- inbox model verification (§4.5) ----------------------------------------------------------

pub const EntryRef = struct {
    oid: []const u8,
    principal: []const u8,
    entry: Entry,

    /// `is_verified`: the signature checks against the actor's registered key
    /// **and** the inbox it was found in names the entry's own actor (§4.5).
    pub fn is_verified(self: *const EntryRef, alloc: std.mem.Allocator, principals: *const Principals) bool {
        if (!std.mem.eql(u8, self.principal, self.entry.actor)) return false;
        const key = principals.get(self.entry.actor) orelse return false;
        verify_entry(alloc, &self.entry, key) catch return false;
        return true;
    }
};

/// The aggregation input as a set keyed by oid (§7.1): the same blob under two
/// refs is one entry; when the copies disagree on the inbox, the occurrence
/// whose principal is the entry's own actor wins — a planted copy never
/// displaces the legitimate one.
pub const EntrySet = struct {
    entries: util.List(EntryRef),
    by_oid: std.StringHashMap(usize),
    alloc: std.mem.Allocator,

    pub fn init(alloc: std.mem.Allocator) EntrySet {
        return .{
            .entries = util.List(EntryRef).init(alloc),
            .by_oid = std.StringHashMap(usize).init(alloc),
            .alloc = alloc,
        };
    }

    pub fn deinit(self: *EntrySet) void {
        self.entries.deinit();
        self.by_oid.deinit();
    }

    pub fn insert(self: *EntrySet, er: EntryRef) void {
        if (self.by_oid.get(er.oid)) |i| {
            const cur = &self.entries.items[i];
            const replace = !std.mem.eql(u8, cur.principal, cur.entry.actor) and
                std.mem.eql(u8, er.principal, er.entry.actor);
            if (replace) cur.* = er;
            return;
        }
        self.entries.append(er) catch @panic("oom");
        self.by_oid.put(er.oid, self.entries.items.len - 1) catch @panic("oom");
    }

    pub fn slice(self: *EntrySet) []EntryRef {
        return self.entries.items;
    }
};

// ---- refname-safe segments (§3.1) -------------------------------------------------------------

/// The segment rule the CLI and the thin API share: non-empty, ≤ 255 bytes,
/// `[A-Za-z0-9._@-]`, first character alphanumeric, no `..`, not `.`/`..`,
/// not ending in `.lock` (case-insensitive).
pub fn ref_segment_ok(s: []const u8) bool {
    if (s.len == 0 or s.len > 255) return false;
    const first = s[0];
    if (!std.ascii.isAlphanumeric(first)) return false;
    if (std.mem.indexOf(u8, s, "..") != null) return false;
    if (std.mem.eql(u8, s, ".") or std.mem.eql(u8, s, "..")) return false;
    if (s.len >= 5 and std.ascii.eqlIgnoreCase(s[s.len - 5 ..], ".lock")) return false;
    for (s) |c| {
        if (!(std.ascii.isAlphanumeric(c) or c == '.' or c == '_' or c == '@' or c == '-')) return false;
    }
    return true;
}

// ---- tests --------------------------------------------------------------------------------------

const testing = std.testing;

fn golden_entry(alloc: std.mem.Allocator) Entry {
    var body = jsonio.new_object();
    jsonio.obj_put(&body, alloc, "title", .{ .string = "golden vector" });
    return .{
        .version = 1,
        .kind = "issue",
        .id = "golden",
        .actor = "alice",
        .ts = 1_786_500_000,
        .parent = "",
        .refs = null,
        .body = .{ .object = body },
        .sig = "",
    };
}

/// The cross-language golden vector (`docs/D1_PROTOCOL.md` §5.3.1): the same
/// constants `walgit-wal/src/collab.rs::golden_tests` and the SDK tests pin —
/// the third implementation must reproduce the exact signed bytes.
const GOLDEN_CANONICAL =
    "{\"actor\":\"alice\",\"body\":{\"title\":\"golden vector\"},\"id\":\"golden\",\"kind\":\"issue\",\"parent\":\"\",\"sig\":\"\",\"ts\":1786500000,\"version\":1}";
const GOLDEN_SIG_B64 = "VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ==";
const GOLDEN_PUB_B64 = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";

test "golden canonical bytes" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    const canonical = entry_canonical(alloc, &e);
    defer alloc.free(canonical);
    try testing.expectEqualStrings(GOLDEN_CANONICAL, canonical);
}

test "golden signature matches the fixed key" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    const seed: [32]u8 = @splat(7);
    const kp = try SigningKey.generateDeterministic(seed);
    const pub_b64 = public_key_b64(alloc, kp);
    defer alloc.free(pub_b64);
    try testing.expectEqualStrings(GOLDEN_PUB_B64, pub_b64);
    sign_entry(alloc, &e, kp);
    defer alloc.free(e.sig);
    const expected = "ed25519:" ++ GOLDEN_SIG_B64;
    try testing.expectEqualStrings(expected, e.sig);
    try verify_entry(alloc, &e, GOLDEN_PUB_B64);
}

test "sign verify roundtrip and tamper" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    const seed: [32]u8 = @splat(7);
    const kp = try SigningKey.generateDeterministic(seed);
    const pub_b64 = public_key_b64(alloc, kp);
    defer alloc.free(pub_b64);
    sign_entry(alloc, &e, kp);
    defer alloc.free(e.sig);
    try verify_entry(alloc, &e, pub_b64);
    // Tamper any covered field and the signature is dead.
    const old = e.kind;
    e.kind = "comment";
    try testing.expectError(error.VerificationFailed, verify_entry(alloc, &e, pub_b64));
    e.kind = old;
    // A mangled signature never verifies (restore the allocated one before
    // the scope's `defer alloc.free(e.sig)`).
    const real_sig = e.sig;
    e.sig = "ed25519:AAAA";
    try testing.expectError(error.BadSigLength, verify_entry(alloc, &e, pub_b64));
    e.sig = real_sig;
}

test "git blob oid known answers" {
    const alloc = testing.allocator;
    const h1 = git_blob_oid(alloc, "hello", .sha1);
    defer alloc.free(h1);
    try testing.expectEqualStrings("b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0", h1);
    const h2 = git_blob_oid(alloc, "", .sha1);
    defer alloc.free(h2);
    try testing.expectEqualStrings("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391", h2);
    const h3 = git_blob_oid(alloc, "{\"a\":1}", .sha1);
    defer alloc.free(h3);
    try testing.expectEqualStrings("daa5053ecf5f9a37b2de733d0751cc1ab53ac010", h3);
    const h4 = git_blob_oid(alloc, "hello", .sha256);
    defer alloc.free(h4);
    try testing.expectEqualStrings("8aec4e4876f854f688d0ebfc8f37598f38e5fd6903cccc850ca36591175aeb60", h4);
}

test "entry serializes in struct field order" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    const s = entry_json(alloc, &e, true);
    defer alloc.free(s);
    try testing.expectEqualStrings(
        "{\n  \"version\": 1,\n  \"kind\": \"issue\",\n  \"id\": \"golden\",\n  \"actor\": \"alice\",\n  \"ts\": 1786500000,\n  \"parent\": \"\",\n  \"body\": {\n    \"title\": \"golden vector\"\n  },\n  \"sig\": \"\"\n}",
        s,
    );
}

test "refs serialize with only the named fields" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    e.refs = .{ .base = "refs/heads/main" };
    const s = entry_json(alloc, &e, false);
    defer alloc.free(s);
    try testing.expectEqualStrings(
        "{\"version\":1,\"kind\":\"issue\",\"id\":\"golden\",\"actor\":\"alice\",\"ts\":1786500000,\"parent\":\"\",\"refs\":{\"base\":\"refs/heads/main\"},\"body\":{\"title\":\"golden vector\"},\"sig\":\"\"}",
        s,
    );
    // canonical keeps the refs key with sorted fields
    const c = entry_canonical(alloc, &e);
    defer alloc.free(c);
    try testing.expect(std.mem.indexOf(u8, c, "\"refs\":{\"base\":\"refs/heads/main\"}") != null);
}

test "ref segment rule" {
    try testing.expect(ref_segment_ok("alice"));
    try testing.expect(ref_segment_ok("svc-agent.1@x"));
    try testing.expect(!ref_segment_ok(""));
    try testing.expect(!ref_segment_ok(".hidden"));
    try testing.expect(!ref_segment_ok("a..b"));
    try testing.expect(!ref_segment_ok("a.lock"));
    try testing.expect(!ref_segment_ok("a/ b"));
}

test "entry set dedups by oid and prefers the legitimate inbox" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var e = golden_entry(alloc);
    defer e.body.object.deinit(alloc);
    const seed: [32]u8 = @splat(7);
    const kp = try SigningKey.generateDeterministic(seed);
    sign_entry(alloc, &e, kp);
    defer alloc.free(e.sig);
    const bytes = entry_json(alloc, &e, true);
    defer alloc.free(bytes);
    const oid = git_blob_oid(alloc, bytes, .sha1);
    defer alloc.free(oid);

    var principals = Principals.init(alloc);
    defer principals.deinit();
    try principals.put("alice", GOLDEN_PUB_B64);

    for ([2][]const u8{ "mallory", "alice" }) |first_inbox| {
        var set = EntrySet.init(alloc);
        defer set.deinit();
        // Insert the planted copy first, then the legitimate one.
        set.insert(.{ .oid = oid, .principal = first_inbox, .entry = e });
        set.insert(.{ .oid = oid, .principal = "alice", .entry = e });
        try testing.expectEqual(@as(usize, 1), set.entries.items.len);
        try testing.expect(set.entries.items[0].is_verified(alloc, &principals));
    }
}
