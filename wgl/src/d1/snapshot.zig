//! The fold: a signed snapshot of the inbox (D45 / `docs/D1_PROTOCOL.md`
//! §9). One CAS-moved ref carries every folded entry verbatim; aggregation
//! over snapshot ∪ unfolded tail is byte-identical to the unfolded ledger.
//! Ported from `walgit-wal/src/collab.rs`.

const std = @import("std");
const jval = std.json;
const entry_mod = @import("entry.zig");

const jsonio = @import("../jsonio.zig");
const util = @import("../util.zig");

pub const Value = jval.Value;
pub const EntryRef = entry_mod.EntryRef;
pub const Entry = entry_mod.Entry;
pub const ObjectFormat = entry_mod.ObjectFormat;
pub const SNAPSHOT_REF = entry_mod.SNAPSHOT_REF;
pub const git_blob_oid = entry_mod.git_blob_oid;
pub const SigningKey = entry_mod.SigningKey;

pub const SnapshotRecord = struct {
    oid: []const u8,
    principal: []const u8,
    /// The raw entry bytes, verbatim — never re-serialized.
    json: []const u8,

    /// The record as an aggregation input, or `null` when the bytes do not
    /// hash to the declared oid or do not parse as an entry — skipped exactly
    /// like a corrupt inbox blob: visible degradation, never silent trust.
    pub fn entry_ref(self: *const SnapshotRecord, arena: std.mem.Allocator, format: ObjectFormat) ?EntryRef {
        const computed = git_blob_oid(arena, self.json, format);
        defer arena.free(computed);
        if (!std.mem.eql(u8, computed, self.oid)) return null;
        const e = entry_mod.parse_entry_arena(arena, self.json) catch return null;
        return .{ .oid = self.oid, .principal = self.principal, .entry = e };
    }
};

pub const Snapshot = struct {
    version: u32,
    kind: []const u8,
    actor: []const u8,
    ts: i64,
    entries: []SnapshotRecord,
    /// Whether this fold carries the whole ledger it was built from. Absent =
    /// true (pre-marker snapshots); `false` marks a deliberately truncated
    /// snapshot.
    complete: bool = true,
    dropped_entries: u64 = 0,
    sig: []const u8 = "",

    /// The wire bytes: struct field order; `complete` omitted when true,
    /// `dropped_entries` omitted when zero (the serde skip rules — old
    /// snapshots' signatures stay valid across the schema extension, §9.1).
    pub fn to_json(self: *const Snapshot, alloc: std.mem.Allocator) []u8 {
        var out = jsonio.Out.init(alloc);
        jsonio.put_raw(&out, "{\"version\":1,\"kind\":");
        jsonio.write_string(&out, self.kind);
        jsonio.put_raw(&out, ",\"actor\":");
        jsonio.write_string(&out, self.actor);
        var buf: [32]u8 = undefined;
        jsonio.put_raw(&out, ",\"ts\":");
        jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{self.ts}) catch unreachable);
        jsonio.put_raw(&out, ",\"entries\":[");
        for (self.entries, 0..) |r, i| {
            if (i > 0) jsonio.put_raw(&out, ",");
            jsonio.put_raw(&out, "{\"oid\":");
            jsonio.write_string(&out, r.oid);
            jsonio.put_raw(&out, ",\"principal\":");
            jsonio.write_string(&out, r.principal);
            jsonio.put_raw(&out, ",\"json\":");
            jsonio.write_string(&out, r.json);
            jsonio.put_raw(&out, "}");
        }
        jsonio.put_raw(&out, "]");
        if (!self.complete) {
            jsonio.put_raw(&out, ",\"complete\":false");
            jsonio.put_raw(&out, ",\"dropped_entries\":");
            jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{self.dropped_entries}) catch unreachable);
        }
        jsonio.put_raw(&out, ",\"sig\":");
        jsonio.write_string(&out, self.sig);
        jsonio.put_raw(&out, "}");
        return out.to_owned_slice() catch @panic("oom");
    }
};

pub const ParseError = error{
    NotJson,
    NotAnObject,
    MissingField,
    WrongType,
    BadVersion,
    BadKind,
    OutOfMemory,
};

fn get_str(o: jval.ObjectMap, key: []const u8) ?[]const u8 {
    const v = o.get(key) orelse return null;
    return switch (v) {
        .string => |s| s,
        else => null,
    };
}

/// Parse and validate a snapshot document. Fails closed on a wrong
/// version/kind — a corrupt snapshot document errors the read.
pub fn parse_snapshot(arena: std.mem.Allocator, bytes: []const u8) ParseError!Snapshot {
    const v = jval.parseFromSliceLeaky(Value, arena, bytes, .{}) catch return error.NotJson;
    const o = switch (v) {
        .object => |o| o,
        else => return error.NotAnObject,
    };
    const version = switch (o.get("version") orelse return error.MissingField) {
        .integer => |i| i,
        else => return error.WrongType,
    };
    if (version != 1) return error.BadVersion;
    const kind = get_str(o, "kind") orelse return error.MissingField;
    if (!std.mem.eql(u8, kind, "collab_snapshot")) return error.BadKind;
    const actor = get_str(o, "actor") orelse return error.MissingField;
    const ts = switch (o.get("ts") orelse return error.MissingField) {
        .integer => |i| i,
        else => return error.WrongType,
    };
    const sig: []const u8 = get_str(o, "sig") orelse "";
    const complete: bool = switch (o.get("complete") orelse Value{ .bool = true }) {
        .bool => |b| b,
        else => return error.WrongType,
    };
    const dropped: u64 = switch (o.get("dropped_entries") orelse Value{ .integer = 0 }) {
        .integer => |i| if (i >= 0) @intCast(i) else return error.WrongType,
        else => return error.WrongType,
    };
    const ev = o.get("entries") orelse return error.MissingField;
    const arr = switch (ev) {
        .array => |a| a,
        else => return error.WrongType,
    };
    var records: util.List(SnapshotRecord) = .init(arena);
    for (arr.items) |item| {
        const ro = switch (item) {
            .object => |ro| ro,
            else => return error.WrongType,
        };
        try records.append(.{
            .oid = get_str(ro, "oid") orelse return error.MissingField,
            .principal = get_str(ro, "principal") orelse return error.MissingField,
            .json = get_str(ro, "json") orelse return error.MissingField,
        });
    }
    return .{
        .version = @intCast(version),
        .kind = kind,
        .actor = actor,
        .ts = ts,
        .entries = try records.to_owned_slice(),
        .complete = complete,
        .dropped_entries = dropped,
        .sig = sig,
    };
}

/// The canonical bytes the snapshot's signature covers: the document without
/// `sig` (the key kept, empty), under the §5.3 canonical contract.
pub fn snapshot_canonical(alloc: std.mem.Allocator, snap: *const Snapshot) []u8 {
    var out = jsonio.Out.init(alloc);
    jsonio.put_raw(&out, "{\"actor\":");
    jsonio.write_string(&out, snap.actor);
    jsonio.put_raw(&out, ",\"complete\":");
    jsonio.put_raw(&out, if (snap.complete) "true" else "false");
    jsonio.put_raw(&out, ",\"dropped_entries\":");
    var buf: [32]u8 = undefined;
    jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{snap.dropped_entries}) catch unreachable);
    jsonio.put_raw(&out, ",\"entries\":[");
    for (snap.entries, 0..) |r, i| {
        if (i > 0) jsonio.put_raw(&out, ",");
        jsonio.put_raw(&out, "{\"json\":");
        jsonio.write_string(&out, r.json);
        jsonio.put_raw(&out, ",\"oid\":");
        jsonio.write_string(&out, r.oid);
        jsonio.put_raw(&out, ",\"principal\":");
        jsonio.write_string(&out, r.principal);
        jsonio.put_raw(&out, "}");
    }
    jsonio.put_raw(&out, "],\"kind\":");
    jsonio.write_string(&out, snap.kind);
    jsonio.put_raw(&out, ",\"sig\":\"\",\"ts\":");
    jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{snap.ts}) catch unreachable);
    jsonio.put_raw(&out, ",\"version\":");
    jsonio.put_raw(&out, std.fmt.bufPrint(&buf, "{d}", .{snap.version}) catch unreachable);
    jsonio.put_raw(&out, "}");
    return out.to_owned_slice() catch @panic("oom");
}

/// Sign a snapshot document; returns `ed25519:<base64>`.
pub fn sign_snapshot(alloc: std.mem.Allocator, snap: *Snapshot, key: SigningKey) void {
    const canonical = snapshot_canonical(alloc, snap);
    defer alloc.free(canonical);
    const sig = key.sign(canonical, null) catch @panic("ed25519 sign");
    const b64 = util.b64_encode(alloc, &sig.toBytes());
    snap.sig = std.fmt.allocPrint(alloc, "ed25519:{s}", .{b64}) catch @panic("oom");
    alloc.free(b64);
}

pub const VerifyError = entry_mod.VerifyError;

/// Verify a snapshot's own signature against a public key (base64) — fold
/// provenance for audit and the watch notification; never a trust input for
/// the contained entries.
pub fn verify_snapshot(alloc: std.mem.Allocator, snap: *const Snapshot, public_key_b64: []const u8) VerifyError!void {
    if (!std.mem.startsWith(u8, snap.sig, "ed25519:")) return error.SigPrefix;
    const sig_bytes = util.b64_decode(alloc, snap.sig["ed25519:".len..]) catch |err| switch (err) {
        error.OutOfMemory => return error.OutOfMemory,
        else => return error.BadSigBase64,
    };
    defer alloc.free(sig_bytes);
    if (sig_bytes.len != 64) return error.BadSigLength;
    const key_bytes = util.b64_decode(alloc, std.mem.trim(u8, public_key_b64, " \t\r\n")) catch |err| switch (err) {
        error.OutOfMemory => return error.OutOfMemory,
        else => return error.BadKeyBase64,
    };
    defer alloc.free(key_bytes);
    if (key_bytes.len != 32) return error.BadKeyLength;
    const canonical = snapshot_canonical(alloc, snap);
    defer alloc.free(canonical);
    const pk = std.crypto.sign.Ed25519.PublicKey.fromBytes(key_bytes[0..32].*) catch return error.BadKeyLength;
    const sig = std.crypto.sign.Ed25519.Signature.fromBytes(sig_bytes[0..64].*);
    var verifier = sig.verifier(pk) catch return error.VerificationFailed;
    verifier.update(canonical);
    verifier.verifyStrict() catch return error.VerificationFailed;
}

/// Build the fold: records deduped by oid and sorted oid-ascending (a pure
/// function of the set), signed by `actor`. Records are carried verbatim.
pub fn build_snapshot(alloc: std.mem.Allocator, actor: []const u8, ts: i64, entries: []const SnapshotRecord, complete: bool, dropped_entries: u64, key: SigningKey) Snapshot {
    const sorted = alloc.dupe(SnapshotRecord, entries) catch @panic("oom");
    std.mem.sort(SnapshotRecord, sorted, {}, struct {
        fn f(_: void, a: SnapshotRecord, b: SnapshotRecord) bool {
            return std.mem.order(u8, a.oid, b.oid) == .lt;
        }
    }.f);
    // dedup by oid (keep the first of each equal-oid run)
    var n: usize = 0;
    for (sorted, 0..) |r, i| {
        if (i == 0 or !std.mem.eql(u8, r.oid, sorted[n - 1].oid)) {
            sorted[n] = r;
            n += 1;
        }
    }
    var snap = Snapshot{
        .version = 1,
        .kind = "collab_snapshot",
        .actor = actor,
        .ts = ts,
        .entries = sorted[0..n],
        .complete = complete,
        .dropped_entries = dropped_entries,
        .sig = "",
    };
    sign_snapshot(alloc, &snap, key);
    return snap;
}

test "snapshot wire bytes skip the completeness defaults" {
    const alloc = std.testing.allocator;
    var arena_state = std.heap.ArenaAllocator.init(alloc);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const seed: [32]u8 = @splat(9);
    const key = SigningKey.generateDeterministic(seed) catch unreachable;

    var recs: [1]SnapshotRecord = .{.{ .oid = "aa", .principal = "alice", .json = "{}" }};
    const snap = build_snapshot(arena, "alice", 42, &recs, true, 0, key);
    const text = snap.to_json(arena);
    try std.testing.expect(std.mem.indexOf(u8, text, "\"complete\"") == null);
    try std.testing.expect(std.mem.indexOf(u8, text, "\"dropped_entries\"") == null);
    const parsed = try parse_snapshot(arena, text);
    try std.testing.expect(parsed.complete);
    try std.testing.expectEqual(@as(u64, 0), parsed.dropped_entries);
}

test "snapshot round trips and fails closed" {
    const alloc = std.testing.allocator;
    var arena_state = std.heap.ArenaAllocator.init(alloc);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const seed: [32]u8 = @splat(9);
    const key = SigningKey.generateDeterministic(seed) catch unreachable;
    const seed7: [32]u8 = @splat(7);
    const key7 = SigningKey.generateDeterministic(seed7) catch unreachable;

    const recs: [3]SnapshotRecord = .{
        .{ .oid = "bb", .principal = "bob", .json = "{\"id\":\"t2\"}" },
        .{ .oid = "aa", .principal = "alice", .json = "{\"id\":\"t1\"}" },
        .{ .oid = "aa", .principal = "alice", .json = "{\"id\":\"t1\"}" }, // duplicate
    };
    const snap = build_snapshot(arena, "alice", 42, &recs, true, 0, key);
    // Sorted by oid, deduped: "aa" once, then "bb".
    try std.testing.expectEqual(@as(usize, 2), snap.entries.len);
    try std.testing.expectEqualStrings("aa", snap.entries[0].oid);
    try std.testing.expectEqualStrings("bb", snap.entries[1].oid);
    // The fold is deterministic: same records, any input order.
    const shuffled: [3]SnapshotRecord = .{ recs[2], recs[1], recs[0] };
    const snap2 = build_snapshot(arena, "alice", 42, &shuffled, true, 0, key);
    try std.testing.expectEqualSlices(u8, snap.to_json(arena), snap2.to_json(arena));
    // The fold's signature verifies against the folder key and nobody else.
    const pk = entry_mod.public_key_b64(arena, key);
    try verify_snapshot(arena, &snap, pk);
    const pk7 = entry_mod.public_key_b64(arena, key7);
    try std.testing.expectError(error.VerificationFailed, verify_snapshot(arena, &snap, pk7));

    // Truncation marker: complete=false + dropped_entries carry the fact.
    const trunc = build_snapshot(arena, "alice", 43, recs[0..1], false, 7, key);
    const text = trunc.to_json(arena);
    try std.testing.expect(std.mem.indexOf(u8, text, "\"complete\":false") != null);
    try std.testing.expect(std.mem.indexOf(u8, text, "\"dropped_entries\":7") != null);
    const parsed = try parse_snapshot(arena, text);
    try std.testing.expect(!parsed.complete);
    try std.testing.expectEqual(@as(u64, 7), parsed.dropped_entries);
    try verify_snapshot(arena, &parsed, pk);

    // Fail closed on a wrong version / kind / garbage.
    try std.testing.expectError(error.BadVersion, parse_snapshot(arena, "{\"version\":2,\"kind\":\"collab_snapshot\",\"actor\":\"a\",\"ts\":1,\"entries\":[]}"));
    try std.testing.expectError(error.BadKind, parse_snapshot(arena, "{\"version\":1,\"kind\":\"other\",\"actor\":\"a\",\"ts\":1,\"entries\":[]}"));
    try std.testing.expectError(error.NotJson, parse_snapshot(arena, "{{{{"));
}

test "records with lying oids or unparseable bytes are skipped" {
    const alloc = std.testing.allocator;
    var arena_state = std.heap.ArenaAllocator.init(alloc);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const bytes = "{\"version\":1,\"id\":\"t\",\"kind\":\"issue\",\"actor\":\"alice\",\"ts\":1,\"parent\":\"\",\"body\":{},\"sig\":\"\"}";
    const oid = git_blob_oid(arena, bytes, .sha1);
    const good = SnapshotRecord{ .oid = oid, .principal = "alice", .json = bytes };
    try std.testing.expect(good.entry_ref(arena, .sha1) != null);
    const zero40 = "0000000000000000000000000000000000000000";
    const lying = SnapshotRecord{ .oid = zero40, .principal = "alice", .json = bytes };
    try std.testing.expect(lying.entry_ref(arena, .sha1) == null);
    const garbage = SnapshotRecord{ .oid = git_blob_oid(arena, "not json", .sha1), .principal = "alice", .json = "not json" };
    try std.testing.expect(garbage.entry_ref(arena, .sha1) == null);
    try std.testing.expect(garbage.entry_ref(arena, .sha1) == null);
}
