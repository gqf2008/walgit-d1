//! The project-local collaboration identity (`docs/D1_PROTOCOL.md` §4.1,
//! D58/D59/D62): one principal + key per **worktree**, materialized in that
//! worktree's git dir under `walgit/` — never tracked, never touched by
//! `git clean`, `0600` on every secret. Ported from `collab_cmd.rs`.

const std = @import("std");
const entry_mod = @import("d1/entry.zig");
const gitx = @import("gitx.zig");
const jsonio = @import("jsonio.zig");
const util = @import("util.zig");

pub const Io = std.Io;
pub const SigningKey = entry_mod.SigningKey;

pub const SegmentError = error{NotRefnameSafe};

/// The §3.1 segment rule with the CLI's error message.
pub fn ref_segment(label: []const u8, s: []const u8) SegmentError!void {
    if (entry_mod.ref_segment_ok(s)) return;
    std.debug.print("wgl: collab.{s}: \"{s}\" is not a refname-safe segment ([A-Za-z0-9._@-]+)\n", .{ label, s });
    return error.NotRefnameSafe;
}

/// Load a key file: 32 raw seed bytes as hex text (may carry a trailing
/// newline). `--key` is a file path, never the key material (§4.1: passing
/// content would echo the private key into the terminal on error).
pub fn read_signing_key(alloc: std.mem.Allocator, io: Io, path: []const u8) !SigningKey {
    var dir = std.Io.Dir.cwd();
    const raw = dir.readFileAlloc(io, path, alloc, .limited(1 << 20)) catch return error.KeyReadFailed;
    defer alloc.free(raw);
    const trimmed = std.mem.trim(u8, raw, " \t\r\n");
    const seed = util.from_hex(alloc, trimmed) catch return error.KeyNotHex;
    defer alloc.free(seed);
    if (seed.len != 32) return error.KeyNot32Bytes;
    return SigningKey.generateDeterministic(seed[0..32].*) catch error.BadSeed;
}

/// `<git-dir>/walgit` — the identity dir of THIS worktree (the main checkout
/// and each linked worktree are distinct collaborators).
pub fn project_identity_dir(alloc: std.mem.Allocator, io: Io, repo: []const u8) ![]u8 {
    const git_dir = try gitx.absolute_git_dir(alloc, io, repo);
    defer alloc.free(git_dir);
    return std.fs.path.join(alloc, &.{ git_dir, "walgit" }) catch error.OutOfMemory;
}

pub const Identity = struct {
    principal: []const u8,
    /// Key path, relative to the identity file's directory unless absolute.
    key: ?[]const u8 = null,
};

/// Parse `<dir>/identity` (JSON). Malformed identity files are an error — a
/// silently ignored identity would sign as the wrong principal.
pub fn read_identity_at(alloc: std.mem.Allocator, io: Io, dir: []const u8) !Identity {
    var arena_state = std.heap.ArenaAllocator.init(alloc);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const path = std.fs.path.join(arena, &.{ dir, "identity" }) catch return error.OutOfMemory;
    var d = std.Io.Dir.cwd();
    const raw = d.readFileAlloc(io, path, arena, .limited(1 << 20)) catch return error.IdentityReadFailed;
    const v = std.json.parseFromSliceLeaky(std.json.Value, arena, raw, .{}) catch return error.IdentityBad;
    const o = switch (v) {
        .object => |o| o,
        else => return error.IdentityBad,
    };
    const principal = switch (o.get("principal") orelse return error.IdentityBad) {
        .string => |s| s,
        else => return error.IdentityBad,
    };
    var key: ?[]const u8 = null;
    if (o.get("key")) |kv| {
        if (kv == .string) key = kv.string;
    }
    return .{
        .principal = alloc.dupe(u8, principal) catch return error.OutOfMemory,
        .key = if (key) |k| alloc.dupe(u8, k) catch return error.OutOfMemory else null,
    };
}

/// Resolve a relative key path against the identity dir; the conventional
/// default is `<dir>/keys/<principal>.ed25519`.
pub fn resolve_key_path(alloc: std.mem.Allocator, dir: []const u8, principal: []const u8, key: ?[]const u8) ![]u8 {
    const k = key orelse {
        return std.fmt.allocPrint(alloc, "{s}/keys/{s}.ed25519", .{ dir, principal }) catch error.OutOfMemory;
    };
    if (std.fs.path.isAbsolute(k)) {
        return alloc.dupe(u8, k) catch error.OutOfMemory;
    }
    return std.fs.path.join(alloc, &.{ dir, k }) catch error.OutOfMemory;
}

pub const Resolved = struct {
    principal: []const u8,
    key_path: []u8,
};

/// Resolve `--principal`/`--key` against the project identity. Explicit
/// values always win; otherwise the identity supplies the missing half, and
/// a bare principal falls back to the conventional project key path.
pub fn resolve_identity(alloc: std.mem.Allocator, io: Io, repo: []const u8, principal: ?[]const u8, key: ?[]const u8) !Resolved {
    if (principal != null and key != null) {
        return .{
            .principal = principal.?,
            .key_path = alloc.dupe(u8, key.?) catch return error.OutOfMemory,
        };
    }
    const dir = try project_identity_dir(alloc, io, repo);
    var identity: ?Identity = null;
    {
        const id_path = std.fs.path.join(alloc, &.{ dir, "identity" }) catch return error.OutOfMemory;
        defer alloc.free(id_path);
        var d = std.Io.Dir.cwd();
        if (d.access(io, id_path, .{}) catch error.FileNotFound) |_| {
            identity = read_identity_at(alloc, io, dir) catch null;
        } else |_| {}
    }
    const p: []const u8 = principal orelse blk: {
        if (identity) |id| break :blk id.principal;
        const id_path = std.fs.path.join(alloc, &.{ dir, "identity" }) catch return error.OutOfMemory;
        std.debug.print(
            "wgl: no --principal and no project identity at {s}; run `wgl join --principal <name>` first\n",
            .{id_path},
        );
        return error.NoIdentity;
    };
    const key_path = blk: {
        if (key) |k| break :blk alloc.dupe(u8, k) catch return error.OutOfMemory;
        if (identity) |id| {
            if (std.mem.eql(u8, id.principal, p)) {
                break :blk try resolve_key_path(alloc, dir, p, id.key);
            }
        }
        break :blk try resolve_key_path(alloc, dir, p, null);
    };
    return .{ .principal = p, .key_path = key_path };
}

/// Write `bytes` to `path` with `0600` (fsync before returning: a seed is
/// the only copy).
pub fn write_private_file(alloc: std.mem.Allocator, io: Io, path: []const u8, bytes: []const u8) !void {
    if (std.fs.path.dirname(path)) |parent| {
        make_private_dir(alloc, io, parent) catch {};
    }
    var d = std.Io.Dir.cwd();
    const f = d.createFile(io, path, .{ .truncate = true, .permissions = @fromBackingInt(@intCast(0o600)) }) catch return error.WriteFailed;
    defer f.close(io);
    f.writeStreamingAll(io, bytes) catch return error.WriteFailed;
    f.sync(io) catch {};
    // An existing file was created with its old mode; enforce 0600 via the
    // chmod lane when the platform exposes it on the directory handle's file.
    if (@hasDecl(std.Io.File, "chmod")) {
        f.chmod(io, 0o600) catch {};
    } else if (@import("builtin").os.tag != .windows) {
        // POSIX fallback: chmod(2) on the path.
        const c_path = alloc.allocSentinel(u8, path.len, 0) catch return;
        @memcpy(c_path, path);
        defer alloc.free(c_path);
        _ = std.c.chmod(c_path.ptr, 0o600);
    }
}

/// `0700` on the identity/keys directories.
pub fn make_private_dir(alloc: std.mem.Allocator, io: Io, dir: []const u8) !void {
    _ = alloc;
    var d = std.Io.Dir.cwd();
    if (d.createDirPathOpen(io, dir, .{})) |_| {} else |e| switch (e) {
        error.PathAlreadyExists => {},
        else => return error.WriteFailed,
    }
    // best-effort chmod; on failure the 0600 files still protect the seed
    var f = d.openDir(io, dir, .{}) catch return;
    defer f.close(io);
    if (@hasDecl(std.Io.Dir, "chmod")) {
        f.chmod(io, 0o700) catch {};
    }
}

/// 16 random bytes hex — the inbox `<entry-seg>` (§3.1: any unique
/// refname-safe segment is legal; this reproduces the CLI's shape).
pub fn entry_uuid(alloc: std.mem.Allocator, io: Io) []u8 {
    var b: [16]u8 = undefined;
    io.random(&b);
    return util.to_hex(alloc, &b);
}

/// The runtime session id (a coordination label, not a credential):
/// `KI_AGENT_ID` from the host, else `WALGIT_SESSION`.
pub fn collaboration_session(env: *std.process.Environ.Map, alloc: std.mem.Allocator) ?[]const u8 {
    for ([_][]const u8{ "KI_AGENT_ID", "WALGIT_SESSION" }) |name| {
        if (env.get(name)) |v| {
            const t = std.mem.trim(u8, v, " \t");
            if (t.len > 0 and t.len <= 128) {
                return alloc.dupe(u8, t) catch return null;
            }
        }
    }
    return null;
}
