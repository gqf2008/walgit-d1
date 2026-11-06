//! Reading collab state from a local git checkout (`docs/D1_PROTOCOL.md`
//! §3.2): the aggregation input is `refs/collab/meta/snapshot` ∪ the
//! unfolded inbox tail, deduped by oid (§7.1), plus the principals registry
//! (repo-local `refs/collab/meta/principals/*` overriding the host cache
//! under `refs/walgit/principals/*`, §4.4). Ported from `CollabReader`.

const std = @import("std");
const entry_mod = @import("d1/entry.zig");
const snapshot_mod = @import("d1/snapshot.zig");
const collab = @import("d1/collab.zig");
const gitx = @import("gitx.zig");
const boarddef = @import("boarddef.zig");
const util = @import("util.zig");

pub const Io = std.Io;
pub const EntryRef = entry_mod.EntryRef;
pub const Principals = entry_mod.Principals;

pub const Loaded = struct {
    entries: []EntryRef,
    principals: *Principals,
};

pub const SnapshotBlob = struct { oid: []const u8, bytes: []u8 };
pub const PrincipalRef = struct { principal: []const u8, oid: []const u8 };

pub const Reader = struct {
    alloc: std.mem.Allocator,
    io: Io,
    repo: []const u8,

    pub fn init(alloc: std.mem.Allocator, io: Io, repo: []const u8) Reader {
        return .{ .alloc = alloc, .io = io, .repo = repo };
    }

    pub fn git_ok(self: *const Reader, argv: []const []const u8) gitx.Error![]u8 {
        return gitx.run_ok(self.alloc, self.io, self.repo, argv);
    }

    /// Many oids in **one** `git cat-file --batch` — one process per object
    /// made a real repository's aggregation take minutes (the reason the
    /// batch reader exists). Requests go out in small chunks so neither pipe
    /// can fill while the other side waits; missing objects are absent from
    /// the map and objects over `max_bytes` are drained and skipped (the
    /// stream must stay aligned for the objects behind them).
    pub fn blobs_batch(self: *const Reader, oids: []const []const u8, max_bytes: ?usize) gitx.Error!std.StringHashMap([]const u8) {
        const CHUNK: usize = 512;
        var out = std.StringHashMap([]const u8).init(self.alloc);
        errdefer out.deinit();
        if (oids.len == 0) return out;

        var full: util.List([]const u8) = .init(self.alloc);
        defer full.deinit();
        try full.append_slice(&.{ "git", "-C", self.repo, "cat-file", "--batch" });
        var child = std.process.spawn(self.io, .{
            .argv = full.items,
            .stdin = .pipe,
            .stdout = .pipe,
            .stderr = .ignore,
        }) catch return error.SpawnFailed;
        // `kill` is a no-op once `wait` has reaped the child.
        defer child.kill(self.io);

        var wbuf: [64 * 1024]u8 = undefined;
        var w = child.stdin.?.writer(self.io, &wbuf);
        var rbuf: [256 * 1024]u8 = undefined;
        var r = child.stdout.?.reader(self.io, &rbuf);

        var i: usize = 0;
        while (i < oids.len) {
            const end = @min(i + CHUNK, oids.len);
            for (oids[i..end]) |oid| {
                const req = std.fmt.allocPrint(self.alloc, "{s}\n", .{oid}) catch return error.OutOfMemory;
                w.interface.writeAll(req) catch return error.GitFailed;
            }
            w.interface.flush() catch return error.GitFailed;
            for (oids[i..end]) |oid| {
                const header = r.interface.takeDelimiterInclusive('\n') catch return error.GitFailed;
                const h = std.mem.trim(u8, header, "\r\n");
                var it = std.mem.splitScalar(u8, h, ' ');
                const got = it.next() orelse return error.GitFailed;
                const kind = it.next() orelse "";
                const size_s = it.next() orelse "";
                if (std.mem.eql(u8, kind, "missing") or !std.mem.eql(u8, got, oid)) {
                    continue;
                }
                const size = std.fmt.parseInt(usize, size_s, 10) catch return error.GitFailed;
                if (max_bytes) |cap| {
                    if (size > cap) {
                        // Over the per-entry read cap: skip without
                        // materializing; the next header only comes after
                        // this body, so drain size + the trailing newline.
                        var drained: usize = 0;
                        const total = size + 1;
                        while (drained < total) : (drained += 1) {
                            _ = r.interface.takeByte() catch return error.GitFailed;
                        }
                        continue;
                    }
                }
                const buf = self.alloc.alloc(u8, size) catch return error.OutOfMemory;
                r.interface.readSliceAll(buf) catch {
                    self.alloc.free(buf);
                    return error.GitFailed;
                };
                // The trailing newline after the body.
                _ = r.interface.takeByte() catch {};
                try out.put(try self.alloc.dupe(u8, oid), buf);
            }
            i = end;
        }
        // Close stdin so the child exits, then reap it.
        child.stdin.?.close(self.io);
        child.stdin = null;
        const term = child.wait(self.io) catch return error.SpawnFailed;
        switch (term) {
            .exited => |c| if (c != 0) return error.GitFailed,
            else => return error.GitFailed,
        }
        return out;
    }

    /// `refs/collab/inbox/<principal>/<seg>` → (refname, oid), refname order.
    pub fn inbox_refs(self: *const Reader) gitx.Error![]gitx.RefTip {
        return gitx.for_each_ref(self.alloc, self.io, self.repo, &.{"refs/collab/inbox"});
    }

    /// Every local `refs/collab/{inbox,meta}` ref — the refs a peer needs
    /// for this checkout's aggregation. `refs/walgit/principals/*` stays
    /// out: it is a client-local cache, never part of the repository's
    /// protocol (§3).
    pub fn syncable_refs(self: *const Reader) gitx.Error![]gitx.RefTip {
        return gitx.for_each_ref(self.alloc, self.io, self.repo, &.{ "refs/collab/inbox", "refs/collab/meta" });
    }

    /// The principals registry: repo-local keys win (`or_insert` semantics —
    /// for-each-ref iterates `refs/collab/*` before `refs/walgit/*`, and the
    /// first sight fills the slot, §4.4).
    pub fn principals(self: *const Reader) gitx.Error!*Principals {
        const tips = try gitx.for_each_ref(self.alloc, self.io, self.repo, &.{
            "refs/collab/meta/principals",
            "refs/walgit/principals",
        });
        defer self.alloc.free(tips);
        var refs: util.List(PrincipalRef) = util.List(PrincipalRef).init(self.alloc);
        defer refs.deinit();
        for (tips) |tip| {
            const principal = blk: {
                if (std.mem.startsWith(u8, tip.name, entry_mod.PRINCIPALS_PREFIX)) {
                    break :blk tip.name[entry_mod.PRINCIPALS_PREFIX.len..];
                }
                if (std.mem.startsWith(u8, tip.name, entry_mod.HOST_PRINCIPALS_PREFIX)) {
                    break :blk tip.name[entry_mod.HOST_PRINCIPALS_PREFIX.len..];
                }
                continue;
            };
            try refs.append(.{ .principal = principal, .oid = tip.oid });
        }
        var oids: util.List([]const u8) = .init(self.alloc);
        defer oids.deinit();
        for (refs.items) |r| try oids.append(r.oid);
        const blobs = try self.blobs_batch(oids.items, null);

        const map = try self.alloc.create(Principals);
        map.* = Principals.init(self.alloc);
        for (refs.items) |r| {
            const blob = blobs.get(r.oid) orelse continue;
            var arena_state = std.heap.ArenaAllocator.init(self.alloc);
            defer arena_state.deinit();
            const v = std.json.parseFromSliceLeaky(std.json.Value, arena_state.allocator(), blob, .{}) catch continue;
            const o = switch (v) {
                .object => |o| o,
                else => continue,
            };
            const key = switch (o.get("public_key") orelse continue) {
                .string => |s| s,
                else => continue,
            };
            if (!map.contains(r.principal)) {
                try map.put(try self.alloc.dupe(u8, r.principal), try self.alloc.dupe(u8, key));
            }
        }
        return map;
    }

    /// The raw snapshot blob at `refs/collab/meta/snapshot` with its ref's
    /// current oid — the fold baseline the snapshot push leases against (§9).
    pub fn snapshot_blob(self: *const Reader) gitx.Error!?SnapshotBlob {
        const tips = try gitx.for_each_ref(self.alloc, self.io, self.repo, &.{entry_mod.SNAPSHOT_REF});
        defer self.alloc.free(tips);
        if (tips.len == 0) return null;
        const oid = tips[0].oid;
        const bytes = try gitx.cat_blob(self.alloc, self.io, self.repo, oid);
        return .{ .oid = oid, .bytes = bytes };
    }

    /// The aggregation input: snapshot ∪ inbox tail, deduped by oid (§9.2),
    /// plus the registry. Entries and their strings live in `arena`.
    pub fn load(self: *const Reader, arena: std.mem.Allocator) !Loaded {
        const registry = try self.principals();
        const format = try gitx.object_format(self.alloc, self.io, self.repo);
        const obj_format: entry_mod.ObjectFormat = switch (format) {
            .sha1 => .sha1,
            .sha256 => .sha256,
        };
        var set = entry_mod.EntrySet.init(arena);
        if (try self.snapshot_blob()) |snap| {
            if (snap.bytes.len > entry_mod.COLLAB_SNAPSHOT_MAX_BYTES) {
                std.debug.print(
                    "wgl: {s} is {d} MiB, over the {d} MiB read cap; run `wgl collab gc --truncate` in a clone that still holds it\n",
                    .{ entry_mod.SNAPSHOT_REF, snap.bytes.len / (1024 * 1024), entry_mod.COLLAB_SNAPSHOT_MAX_BYTES / (1024 * 1024) },
                );
                return error.SnapshotOverCap;
            }
            const parsed = snapshot_mod.parse_snapshot(arena, snap.bytes) catch |e| {
                std.debug.print("wgl: {s}: snapshot document rejected ({t}) — fail closed, not silently skipped\n", .{ entry_mod.SNAPSHOT_REF, e });
                return error.BadSnapshot;
            };
            for (parsed.entries) |*rec| {
                if (rec.entry_ref(arena, obj_format)) |er| {
                    set.insert(er);
                }
            }
        }
        // One `cat-file --batch` for the whole tail.
        const inbox = try self.inbox_refs();
        var oids: util.List([]const u8) = .init(arena);
        for (inbox) |tip| try oids.append(tip.oid);
        const blobs = try self.blobs_batch(oids.items, entry_mod.COLLAB_ENTRY_MAX_BYTES);
        for (inbox) |tip| {
            // The inbox path names the principal; a corrupt or oversized
            // entry must not take the whole report down (§7.1 read rules).
            const slash = std.mem.lastIndexOfScalar(u8, tip.name, '/') orelse continue;
            const owner_start = std.mem.lastIndexOfScalar(u8, tip.name[0..slash], '/') orelse continue;
            const principal = tip.name[owner_start + 1 .. slash];
            const blob = blobs.get(tip.oid) orelse continue;
            const e = entry_mod.parse_entry_arena(arena, blob) catch continue;
            set.insert(.{ .oid = tip.oid, .principal = principal, .entry = e });
        }
        return .{ .entries = set.slice(), .principals = registry };
    }
};

/// Merge rules for local aggregation: `--rules <file>` wins; otherwise read
/// `refs/collab/meta/rules` — the same source the server uses — defaulting
/// to nothing-protected when the ref is absent. A present but invalid
/// document is an error (fail closed, §7.3).
pub fn load_merge_rules(arena: std.mem.Allocator, io: Io, repo: []const u8, rules_path: ?[]const u8) !collab.MergeRules {
    if (rules_path) |p| {
        var d = std.Io.Dir.cwd();
        const bytes = d.readFileAlloc(io, p, arena, .limited(1 << 20)) catch return error.RulesReadFailed;
        return collab.parse_merge_rules(arena, bytes) catch error.RulesBad;
    }
    const exists = gitx.ref_exists(arena, io, repo, "refs/collab/meta/rules") catch false;
    if (!exists) return .{};
    const bytes = try gitx.cat_blob(arena, io, repo, "refs/collab/meta/rules");
    return collab.parse_merge_rules(arena, bytes) catch error.RulesBad;
}

/// The board definition from the same source the server endpoint reads —
/// `.walgit/board.toml` at HEAD. `--board` previews an uncommitted file;
/// absent file means the built-in default board; a present-but-invalid one
/// is an error (§8).
pub fn load_board_def(arena: std.mem.Allocator, io: Io, repo: []const u8, override_path: ?[]const u8) !collab.BoardDef {
    if (override_path) |p| {
        var d = std.Io.Dir.cwd();
        const bytes = d.readFileAlloc(io, p, arena, .limited(1 << 20)) catch return error.BoardReadFailed;
        return boarddef.parse_board_def(arena, bytes) catch error.BoardBad;
    }
    const bytes = gitx.cat_blob(arena, io, repo, "HEAD:" ++ entry_mod.BOARD_PATH) catch {
        return collab.default_board(arena) catch error.OutOfMemory;
    };
    return boarddef.parse_board_def(arena, bytes) catch error.BoardBad;
}

/// The write-side actor→key gate (§5.1): the signature must be the key
/// registered for the actor. A mismatched write mints an entry that never
/// verifies — and the log is append-only, so the write is refused instead.
pub fn ensure_actor_key_binding(
    arena: std.mem.Allocator,
    io: Io,
    repo: []const u8,
    actor: []const u8,
    key_path: []const u8,
    key: entry_mod.SigningKey,
) !void {
    const reader = Reader.init(arena, io, repo);
    const principals = reader.principals() catch return error.RegistryReadFailed;
    const registered = principals.get(actor);
    if (registered) |pk| {
        if (signing_key_matches_registered(key, pk)) return;
        const owner = key_owners(arena, io, repo, principals, key);
        std.debug.print(
            \\wgl: cannot sign as actor {s} with the key at {s}: {s}, and {s} is registered with a different public key — verification uses only the key registered for an entry's own actor, so this signature does not verify until {s} is rotated to this key; the log is append-only, so the entry cannot be deleted or edited. Use the key registered for {s}, sign as the actor this key belongs to, or rotate {s} to this key on purpose: `wgl collab principal-register --principal {s} --key <keyfile> --push origin`.
            \\
        , .{ actor, key_path, owner, actor, actor, actor, actor, actor });
        return error.ActorKeyMismatch;
    }
    const owner = key_owners(arena, io, repo, principals, key);
    std.debug.print(
        \\wgl: cannot sign as actor {s} with the key at {s}: {s}, and this repository has no collab key registered for {s}, so the signature cannot be checked here (this is a local view, not proof that {s} is unregistered elsewhere). If {s} is registered on the host, run `wgl collab principal-fetch` to cache the host registry under refs/walgit/principals/* — writes then verify against the host-registered key. Otherwise register the seed you already hold: `wgl join --principal {s} --key <registered seed> --push origin` — `--key` is required, because join without it generates a fresh random seed, and a repo-local registration overrides the host registry, silently shadowing {s}'s host identity (whose past entries then turn unverified). In a fresh clone, fetch the collab refs first (`git fetch <remote> '+refs/collab/*:refs/collab/*'`) so an existing registration is visible
        \\
    , .{ actor, key_path, owner, actor, actor, actor, actor, actor });
    return error.ActorUnregistered;
}

fn signing_key_matches_registered(key: entry_mod.SigningKey, registered_public_key: []const u8) bool {
    var buf: [64]u8 = undefined;
    const n = std.base64.standard.Decoder.calcSizeForSlice(std.mem.trim(u8, registered_public_key, " \t\r\n")) catch return false;
    if (n > buf.len) return false;
    std.base64.standard.Decoder.decode(buf[0..n], std.mem.trim(u8, registered_public_key, " \t\r\n")) catch return false;
    return std.mem.eql(u8, buf[0..n], &key.public_key.bytes);
}

/// Which principal(s) a signing key belongs to, for the refusal messages.
fn key_owners(arena: std.mem.Allocator, io: Io, repo: []const u8, principals: *const Principals, key: entry_mod.SigningKey) []const u8 {
    var owners: util.List([]const u8) = .init(arena);
    var it = principals.iterator();
    while (it.next()) |e| {
        if (signing_key_matches_registered(key, e.value_ptr.*)) {
            owners.append(e.key_ptr.*) catch {};
        }
    }
    std.mem.sort([]const u8, owners.items, {}, struct {
        fn f(_: void, a: []const u8, b: []const u8) bool {
            return std.mem.order(u8, a, b) == .lt;
        }
    }.f);
    if (identity_principal_for_key(arena, io, repo, key)) |identity| {
        var dup = false;
        for (owners.items) |o| {
            if (std.mem.eql(u8, o, identity)) dup = true;
        }
        if (!dup) {
            const tagged = std.fmt.allocPrint(arena, "{s} (this worktree's identity)", .{identity}) catch "";
            if (tagged.len > 0) owners.append(tagged) catch {};
        }
    }
    if (owners.items.len == 0) {
        return "the signing key is not registered under any principal here";
    }
    var out: util.List(u8) = .init(arena);
    out.append_slice("the signing key belongs to ") catch {};
    for (owners.items, 0..) |o, i| {
        if (i > 0) out.append_slice(", ") catch {};
        out.append_slice(o) catch {};
    }
    return out.items;
}

/// The principal whose key this worktree's identity file points at, when the
/// seeds match. Messages only: verification goes through the registry.
fn identity_principal_for_key(arena: std.mem.Allocator, io: Io, repo: []const u8, key: entry_mod.SigningKey) ?[]const u8 {
    const dir = @import("identity.zig").project_identity_dir(arena, io, repo) catch return null;
    const identity = @import("identity.zig").read_identity_at(arena, io, dir) catch return null;
    const path = @import("identity.zig").resolve_key_path(arena, dir, identity.principal, identity.key) catch return null;
    const bound = @import("identity.zig").read_signing_key(arena, io, path) catch return null;
    if (std.mem.eql(u8, &bound.public_key.bytes, &key.public_key.bytes)) return identity.principal;
    return null;
}
