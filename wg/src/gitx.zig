//! The git transport layer: every read and write of `refs/collab/*` travels
//! through the `git` CLI exactly as the retired Rust commands did
//! (`docs/D1_PROTOCOL.md` §3.2 — read commands run against a local checkout;
//! writes are local refs pushed via receive-pack).

const std = @import("std");
const util = @import("util.zig");

pub const Io = std.Io;

/// The stderr of the last failed git invocation — the CLI's error printer
/// includes it (Rust's anyhow context chain carried the same text).
pub var last_stderr: []const u8 = "";

pub const Error = error{
    GitFailed,
    SpawnFailed,
    OutOfMemory,
    Timeout,
};

/// The bounded wait for a ref-only network operation: a `refs/collab/*`
/// fetch or push moves a handful of small objects, so two minutes without
/// finishing means a hung remote, not a slow one. The bound is what lets a
/// long-lived `wg mcp` answer with a tool error instead of stalling its
/// serialized stdio loop forever.
pub const NET_TIMEOUT: Io.Timeout = .{ .duration = .{ .raw = Io.Duration.fromSeconds(120), .clock = .awake } };

/// A push's bound, scaled by the bytes it will move. The 120 s ref bound is
/// right for an inbox entry but not for a 64 MiB gc snapshot or a 16 MiB CI
/// artifact on a slow uplink, so callers that know the payload pass it and
/// get 120 s plus one second per 64 KiB (a 512 kbit/s floor: 64 MiB ≈ 19 min,
/// 16 MiB ≈ 6 min). Unknown-size pushes keep the ref bound; a caller that
/// wants the old unbounded behaviour sets `PushOptions.timeout = .none`.
pub fn pushTimeoutSeconds(payload_bytes: u64) u64 {
    return 120 + payload_bytes / (64 * 1024);
}

fn pushTimeoutForSize(payload_bytes: u64) Io.Timeout {
    return .{ .duration = .{ .raw = Io.Duration.fromSeconds(@intCast(pushTimeoutSeconds(payload_bytes))), .clock = .awake } };
}

pub const Out = struct {
    stdout: []u8,
    stderr: []u8,
    exit_code: ?u8,

    pub fn ok(self: *const Out) bool {
        return self.exit_code != null and self.exit_code.? == 0;
    }
};

/// One git invocation, capturing stdout/stderr. A non-zero exit is **not** an
/// error here — callers decide (the CI runner's fetch failures are warnings).
pub fn run(alloc: std.mem.Allocator, io: Io, repo: ?[]const u8, argv: []const []const u8) Error!Out {
    return runWithTimeout(alloc, io, repo, argv, .none);
}

fn runWithTimeout(alloc: std.mem.Allocator, io: Io, repo: ?[]const u8, argv: []const []const u8, timeout: Io.Timeout) Error!Out {
    var full: util.List([]const u8) = .init(alloc);
    defer full.deinit();
    try full.append("git");
    if (repo) |r| {
        try full.append("-C");
        try full.append(r);
    }
    try full.append_slice(argv);
    last_stderr = "";
    // A duration is turned into an absolute deadline once: every read of the
    // child's output must share the one bound, not restart it.
    const res = std.process.run(alloc, io, .{ .argv = full.items, .timeout = timeout.toDeadline(io) }) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Timeout => return error.Timeout,
        else => return error.SpawnFailed,
    };
    const code: ?u8 = switch (res.term) {
        .exited => |c| c,
        else => null,
    };
    return .{ .stdout = res.stdout, .stderr = res.stderr, .exit_code = code };
}

/// Run a fully-formed argv (already starting with `git`) — for callers
/// that build the command themselves (`push`).
fn run_args(alloc: std.mem.Allocator, io: Io, argv: []const []const u8, timeout: Io.Timeout) Error!Out {
    last_stderr = "";
    const res = std.process.run(alloc, io, .{ .argv = argv, .timeout = timeout.toDeadline(io) }) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        error.Timeout => return error.Timeout,
        else => return error.SpawnFailed,
    };
    const code: ?u8 = switch (res.term) {
        .exited => |c| c,
        else => null,
    };
    return .{ .stdout = res.stdout, .stderr = res.stderr, .exit_code = code };
}

/// Run git and return stdout, or `error.GitFailed` with `last_stderr` set.
pub fn run_ok(alloc: std.mem.Allocator, io: Io, repo: ?[]const u8, argv: []const []const u8) Error![]u8 {
    return run_ok_timeout(alloc, io, repo, argv, .none);
}

fn run_ok_timeout(alloc: std.mem.Allocator, io: Io, repo: ?[]const u8, argv: []const []const u8, timeout: Io.Timeout) Error![]u8 {
    const out = try runWithTimeout(alloc, io, repo, argv, timeout);
    if (!out.ok()) {
        last_stderr = std.mem.trim(u8, out.stderr, " \t\r\n");
        return error.GitFailed;
    }
    return out.stdout;
}

/// Whether `git rev-parse --verify --quiet <name>` resolves.
pub fn ref_exists(alloc: std.mem.Allocator, io: Io, repo: []const u8, name: []const u8) Error!bool {
    const out = try run(alloc, io, repo, &.{ "rev-parse", "--verify", "--quiet", name });
    return out.ok() and std.mem.trim(u8, out.stdout, " \t\r\n").len != 0;
}

/// The checkout's git dir (absolute) — where per-checkout client state lives.
pub fn absolute_git_dir(alloc: std.mem.Allocator, io: Io, repo: []const u8) Error![]u8 {
    const out = try run_ok(alloc, io, repo, &.{ "rev-parse", "--absolute-git-dir" });
    return alloc.dupe(u8, std.mem.trim(u8, out, " \t\r\n")) catch error.OutOfMemory;
}

/// The repository's object format (§3 refnames carry 40- or 64-hex oids).
pub fn object_format(alloc: std.mem.Allocator, io: Io, repo: []const u8) Error!entry_format {
    const out = try run_ok(alloc, io, repo, &.{ "rev-parse", "--show-object-format" });
    const fmt = std.mem.trim(u8, out, " \t\r\n");
    if (std.mem.eql(u8, fmt, "sha1")) return .sha1;
    if (std.mem.eql(u8, fmt, "sha256")) return .sha256;
    return error.GitFailed;
}

pub const entry_format = enum { sha1, sha256 };

/// Write a blob via `git hash-object -w --stdin`; returns the oid. Binary
/// safe (CI artifact/log bytes are not text).
pub fn write_blob(alloc: std.mem.Allocator, io: Io, repo: []const u8, content: []const u8) Error![]u8 {
    var full: util.List([]const u8) = .init(alloc);
    defer full.deinit();
    try full.append_slice(&.{ "git", "-C", repo, "hash-object", "-w", "--stdin" });
    var child = std.process.spawn(io, .{
        .argv = full.items,
        .stdin = .pipe,
        .stdout = .pipe,
        .stderr = .pipe,
    }) catch return error.SpawnFailed;
    defer child.kill(io);

    // Feed the content, then drain the outputs (a pipe can fill while the
    // other side waits, so the drain loop interleaves via the reader buffer).
    {
        var wbuf: [4096]u8 = undefined;
        var w = child.stdin.?.writer(io, &wbuf);
        w.interface.writeAll(content) catch return error.GitFailed;
        w.interface.flush() catch return error.GitFailed;
    }
    child.stdin.?.close(io);
    child.stdin = null;

    var rbuf: [16384]u8 = undefined;
    var r = child.stdout.?.reader(io, &rbuf);
    var list: std.ArrayList(u8) = .empty;
    defer list.deinit(alloc);
    r.interface.appendRemaining(alloc, &list, .limited(1 << 30)) catch return error.GitFailed;
    // Drain stderr so the child cannot block on it.
    var ebuf: [16384]u8 = undefined;
    var er = child.stderr.?.reader(io, &ebuf);
    var elist: std.ArrayList(u8) = .empty;
    defer elist.deinit(alloc);
    er.interface.appendRemaining(alloc, &elist, .limited(1 << 20)) catch {};

    const term = child.wait(io) catch return error.SpawnFailed;
    const code: ?u8 = switch (term) {
        .exited => |c| c,
        else => null,
    };
    if (code == null or code.? != 0) {
        last_stderr = std.mem.trim(u8, elist.items, " \t\r\n");
        return error.GitFailed;
    }
    const oid = std.mem.trim(u8, list.items, " \t\r\n");
    return alloc.dupe(u8, oid) catch error.OutOfMemory;
}

/// `git update-ref <name> <oid>` (set) / `git update-ref -d <name>` (delete).
pub fn update_ref(alloc: std.mem.Allocator, io: Io, repo: []const u8, name: []const u8, oid: ?[]const u8) Error!void {
    if (oid) |o| {
        _ = try run_ok(alloc, io, repo, &.{ "update-ref", name, o });
    } else {
        _ = try run_ok(alloc, io, repo, &.{ "update-ref", "-d", name });
    }
}

pub const PushOptions = struct {
    refspecs: []const []const u8,
    /// `--force-with-lease=ref:expect` entries — a `+` refspec prefix would
    /// silently short-circuit the lease (§9.3), so force is never expressed
    /// through the refspec here.
    leases: []const []const u8 = &.{},
    atomic: bool = false,
    /// `null` derives the bound: `payload_bytes` scales it, otherwise the
    /// 120 s ref bound applies. `.none` means wait forever (the pre-D70
    /// behaviour for callers that still want it).
    timeout: ?Io.Timeout = null,
    /// Total bytes this push is expected to move, when the caller knows it
    /// (a gc snapshot, CI log/artifacts). Small ref pushes omit it.
    payload_bytes: ?u64 = null,
};

/// One push for several refspecs (a single receive-pack round trip).
pub fn push(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8, opts: PushOptions) Error!void {
    if (opts.refspecs.len == 0) return;
    var full: util.List([]const u8) = .init(alloc);
    defer full.deinit();
    try full.append_slice(&.{ "git", "-C", repo, "push" });
    // (full is the complete argv; run_args does not prepend)
    if (opts.atomic) try full.append("--atomic");
    for (opts.leases) |lease| {
        // The arena frees these wholesale; a per-iteration  here
        // would free each spec before git ever saw the argv.
        const spec = std.fmt.allocPrint(alloc, "--force-with-lease={s}", .{lease}) catch return error.OutOfMemory;
        try full.append(spec);
    }
    try full.append(remote);
    try full.append_slice(opts.refspecs);
    const timeout = opts.timeout orelse if (opts.payload_bytes) |bytes| pushTimeoutForSize(bytes) else NET_TIMEOUT;
    const out = try run_args(alloc, io, full.items, timeout);
    if (!out.ok()) {
        last_stderr = std.mem.trim(u8, out.stderr, " \t\r\n");
        return error.GitFailed;
    }
}

/// The watch/gc fetch: entries and meta only — deliberately NOT
/// `refs/collab/ci-artifacts/*` (those blobs are up to 16 MiB each and are
/// pulled on demand). `--prune` lets remote deletions (revoked registry
/// refs, folded inbox refs) vanish locally, so a revoked key stops
/// verifying in long-lived checkouts (§11.1).
pub fn fetch_collab(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8) Error!void {
    _ = try run_ok(alloc, io, repo, &.{
        "fetch",                                    "-q",
        "--prune",                                  remote,
        "+refs/collab/inbox/*:refs/collab/inbox/*", "+refs/collab/meta/*:refs/collab/meta/*",
    });
}

/// The MCP read lane's fetch: bounded, because a stdio server must answer
/// (fail closed) instead of stalling its serialized request loop. A first
/// fetch of a large inbox can exceed the bound; `--no-fetch` and `fresh:false`
/// are the offline escape hatch.
pub fn fetch_collab_bounded(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8) Error!void {
    _ = try run_ok_timeout(alloc, io, repo, &.{
        "fetch",                                    "-q",
        "--prune",                                  remote,
        "+refs/collab/inbox/*:refs/collab/inbox/*", "+refs/collab/meta/*:refs/collab/meta/*",
    }, NET_TIMEOUT);
}

/// On-demand pull of one CI artifact ref; `false` = the remote does not
/// advertise it (external / not published).
pub fn fetch_ci_artifact(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8, actor: []const u8, sha256: []const u8) Error!bool {
    const prefix = @import("d1/ci.zig").CI_ARTIFACT_REF_PREFIX;
    const name = try std.fmt.allocPrint(alloc, "{s}{s}/{s}", .{ prefix, actor, sha256 });
    defer alloc.free(name);
    const probe = try run(alloc, io, repo, &.{ "ls-remote", "--exit-code", "--refs", remote, name });
    if (probe.exit_code == @as(?u8, 2)) return false;
    if (!probe.ok()) {
        last_stderr = std.mem.trim(u8, probe.stderr, " \t\r\n");
        return error.GitFailed;
    }
    const spec = try std.fmt.allocPrint(alloc, "+{s}:{s}", .{ name, name });
    defer alloc.free(spec);
    _ = try run_ok(alloc, io, repo, &.{ "fetch", "-q", remote, spec });
    return true;
}

pub const RefTip = struct { name: []const u8, oid: []const u8 };

/// `git for-each-ref --format=%(refname) %(objectname) <patterns...>`.
pub fn for_each_ref(alloc: std.mem.Allocator, io: Io, repo: []const u8, patterns: []const []const u8) Error![]RefTip {
    var argv: util.List([]const u8) = .init(alloc);
    defer argv.deinit();
    try argv.append_slice(&.{ "for-each-ref", "--format=%(refname) %(objectname)" });
    try argv.append_slice(patterns);
    const out = try run_ok(alloc, io, repo, argv.items);
    var tips: util.List(RefTip) = .init(alloc);
    errdefer tips.deinit();
    var lines = std.mem.splitScalar(u8, out, '\n');
    while (lines.next()) |line| {
        const trimmed = std.mem.trim(u8, line, " \t\r");
        if (trimmed.len == 0) continue;
        const sp = std.mem.indexOfScalar(u8, trimmed, ' ') orelse continue;
        try tips.append(.{
            .name = alloc.dupe(u8, trimmed[0..sp]) catch return error.OutOfMemory,
            .oid = alloc.dupe(u8, trimmed[sp + 1 ..]) catch return error.OutOfMemory,
        });
    }
    return tips.to_owned_slice();
}

/// `git ls-remote <remote>` — the full advertisement as `refname -> oid`.
/// Peeled tag lines (`<oid>\trefs/tags/x^{}`) are skipped only by the
/// runner's tips call, not here (the gc reads the raw advertisement).
pub fn ls_remote(alloc: std.mem.Allocator, io: Io, repo: ?[]const u8, remote: []const u8) Error![]RefTip {
    const out = try run_ok(alloc, io, repo, &.{ "ls-remote", remote });
    var tips: util.List(RefTip) = .init(alloc);
    errdefer tips.deinit();
    var lines = std.mem.splitScalar(u8, out, '\n');
    while (lines.next()) |line| {
        const trimmed = std.mem.trim(u8, line, " \t\r");
        if (trimmed.len == 0) continue;
        const tab = std.mem.indexOfScalar(u8, trimmed, '\t') orelse continue;
        try tips.append(.{
            .name = alloc.dupe(u8, trimmed[tab + 1 ..]) catch return error.OutOfMemory,
            .oid = alloc.dupe(u8, trimmed[0..tab]) catch return error.OutOfMemory,
        });
    }
    return tips.to_owned_slice();
}

/// `git ls-remote --heads --tags` (the CI runner's trigger surface), tips
/// sorted, peeled lines skipped.
pub fn ls_tips(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8) Error![]RefTip {
    const out = try run_ok(alloc, io, repo, &.{ "ls-remote", "--heads", "--tags", remote });
    var tips: util.List(RefTip) = .init(alloc);
    errdefer tips.deinit();
    var lines = std.mem.splitScalar(u8, out, '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeAny(u8, line, " \t\r\n");
        const oid = it.next() orelse continue;
        const name = it.next() orelse continue;
        if (std.mem.endsWith(u8, name, "^{}")) continue;
        try tips.append(.{
            .name = alloc.dupe(u8, name) catch return error.OutOfMemory,
            .oid = alloc.dupe(u8, oid) catch return error.OutOfMemory,
        });
    }
    std.mem.sort(RefTip, tips.items, {}, struct {
        fn f(_: void, a: RefTip, b: RefTip) bool {
            return std.mem.order(u8, a.name, b.name) == .lt;
        }
    }.f);
    return tips.to_owned_slice();
}

/// One blob's bytes by oid (`git cat-file blob`). Missing objects are an
/// error — callers that tolerate absence check `ref_exists` / the batch
/// reader first.
pub fn cat_blob(alloc: std.mem.Allocator, io: Io, repo: []const u8, oid: []const u8) Error![]u8 {
    return run_ok(alloc, io, repo, &.{ "cat-file", "blob", oid });
}

/// One blob's size (`git cat-file -s`), or null when it cannot be read.
pub fn cat_size(alloc: std.mem.Allocator, io: Io, repo: []const u8, oid: []const u8) Error!?u64 {
    const out = try run(alloc, io, repo, &.{ "cat-file", "-s", oid });
    if (!out.ok()) return null;
    return std.fmt.parseInt(u64, std.mem.trim(u8, out.stdout, " \t\r\n"), 10) catch null;
}

/// Fetch one ref's tip and resolve it to a commit oid (the runner's
/// `fetch_commit`). `rev-list -1 --end-of-options` peels to a commit and
/// prints nothing for a non-commit.
pub fn fetch_commit(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8, ref_name: []const u8) Error![]u8 {
    _ = try run_ok(alloc, io, repo, &.{ "fetch", "-q", "--no-tags", remote, ref_name });
    const out = try run_ok(alloc, io, repo, &.{ "rev-list", "-1", "--end-of-options", "FETCH_HEAD" });
    const oid = std.mem.trim(u8, out, " \t\r\n");
    if (oid.len == 0) return error.GitFailed;
    return alloc.dupe(u8, oid) catch error.OutOfMemory;
}

/// `.walgit/ci.toml` from the tested commit's tree (§3): null if absent or
/// not a blob (a submodule is no declaration).
pub fn read_commit_file(alloc: std.mem.Allocator, io: Io, repo: []const u8, commit: []const u8, path: []const u8) Error!?[]u8 {
    const out = try run_ok(alloc, io, repo, &.{ "ls-tree", commit, "--", path });
    const line = std.mem.trim(u8, out, " \t\r\n");
    if (line.len == 0) return null;
    const meta_end = std.mem.indexOfScalar(u8, line, '\t') orelse return null;
    const meta = line[0..meta_end];
    if (std.mem.indexOf(u8, meta, " blob ") == null) return null;
    var it = std.mem.tokenizeScalar(u8, meta, ' ');
    _ = it.next(); // mode
    _ = it.next(); // "blob"
    const oid = it.next() orelse return null;
    const bytes = try cat_blob(alloc, io, repo, oid);
    return bytes;
}

/// `git remote get-url <remote>`.
pub fn remote_url(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote: []const u8) Error![]u8 {
    const out = try run_ok(alloc, io, repo, &.{ "remote", "get-url", remote });
    return alloc.dupe(u8, std.mem.trim(u8, out, " \t\r\n")) catch error.OutOfMemory;
}

/// `git config --get-urlmatch <key> <url>` — the URL-scoped TLS policy the
/// artifact preflight reuses (http.sslCAInfo / http.sslVerify).
pub fn config_urlmatch(alloc: std.mem.Allocator, io: Io, repo: []const u8, key: []const u8, url: []const u8) Error!?[]u8 {
    const out = try run(alloc, io, repo, &.{ "config", "--get-urlmatch", key, url });
    if (!out.ok()) return null;
    const v = std.mem.trim(u8, out.stdout, " \t\r\n");
    if (v.len == 0) return null;
    return alloc.dupe(u8, v) catch error.OutOfMemory;
}

pub const Credential = struct {
    username: []const u8 = "",
    password: []const u8 = "",
};

/// Ask Git for a credential scoped to this exact remote URL (`git credential
/// fill`). Never forward a process-global token; without a host-bound
/// credential the artifact preflight sends no Authorization header and a 401
/// is a hard error.
pub fn credential_fill(alloc: std.mem.Allocator, io: Io, repo: []const u8, url: []const u8) Error!?Credential {
    const Parsed = struct { scheme: []const u8, host: []const u8, path: []const u8 };
    const parsed: Parsed = blk: {
        const scheme_end = std.mem.indexOf(u8, url, "://") orelse return error.GitFailed;
        const rest = url[scheme_end + 3 ..];
        const slash = std.mem.indexOfScalar(u8, rest, '/') orelse rest.len;
        const host = rest[0..slash];
        const path = if (slash < rest.len) rest[slash..] else "";
        break :blk .{ .scheme = url[0..scheme_end], .host = host, .path = path };
    };
    var full: util.List([]const u8) = .init(alloc);
    defer full.deinit();
    try full.append_slice(&.{ "git", "-C", repo, "credential", "fill" });
    var child = std.process.spawn(io, .{
        .argv = full.items,
        .stdin = .pipe,
        .stdout = .pipe,
        .stderr = .ignore,
    }) catch return error.SpawnFailed;
    defer child.kill(io);
    {
        var wbuf: [4096]u8 = undefined;
        var w = child.stdin.?.writer(io, &wbuf);
        w.interface.print("protocol={s}\nhost={s}\npath={s}\n\n", .{ parsed.scheme, parsed.host, parsed.path }) catch {};
        w.interface.flush() catch {};
    }
    child.stdin.?.close(io);
    child.stdin = null;
    var rbuf: [8192]u8 = undefined;
    var r = child.stdout.?.reader(io, &rbuf);
    var list: std.ArrayList(u8) = .empty;
    defer list.deinit(alloc);
    r.interface.appendRemaining(alloc, &list, .limited(1 << 20)) catch {};
    const term = child.wait(io) catch return error.SpawnFailed;
    switch (term) {
        .exited => |c| if (c != 0) return null,
        else => return null,
    }
    var cred = Credential{};
    var lines = std.mem.splitScalar(u8, list.items, '\n');
    while (lines.next()) |line| {
        const t = std.mem.trim(u8, line, " \t\r\n");
        if (std.mem.startsWith(u8, t, "username=")) {
            cred.username = alloc.dupe(u8, t["username=".len..]) catch return error.OutOfMemory;
        } else if (std.mem.startsWith(u8, t, "password=")) {
            cred.password = alloc.dupe(u8, t["password=".len..]) catch return error.OutOfMemory;
        }
    }
    if (cred.password.len == 0) return null;
    return cred;
}

/// `https://host[:port]/owner/repo[.git]` → `https://host[:port]`.
pub fn host_root(alloc: std.mem.Allocator, remote: []const u8) Error![]u8 {
    const without_git = if (std.mem.endsWith(u8, remote, ".git")) remote[0 .. remote.len - 4] else remote;
    const scheme_end = std.mem.indexOf(u8, without_git, "://") orelse return error.GitFailed;
    const rest = without_git[scheme_end + 3 ..];
    const slash = std.mem.indexOfScalar(u8, rest, '/') orelse rest.len;
    return std.fmt.allocPrint(alloc, "{s}://{s}", .{ without_git[0..scheme_end], rest[0..slash] }) catch error.OutOfMemory;
}

test "push timeout scales with the payload" {
    try std.testing.expectEqual(@as(u64, 120), pushTimeoutSeconds(0));
    try std.testing.expectEqual(@as(u64, 120), pushTimeoutSeconds(64 * 1024 - 1));
    try std.testing.expectEqual(@as(u64, 121), pushTimeoutSeconds(64 * 1024));
    // 16 MiB and the 64 MiB snapshot cap sit well beyond the ref bound, and
    // the bound stays monotonic in size.
    try std.testing.expectEqual(@as(u64, 376), pushTimeoutSeconds(16 * 1024 * 1024));
    try std.testing.expectEqual(@as(u64, 1144), pushTimeoutSeconds(64 * 1024 * 1024));
    try std.testing.expect(pushTimeoutSeconds(64 * 1024 * 1024) > pushTimeoutSeconds(16 * 1024 * 1024));
}

test "host_root strips scheme and path" {
    const a = std.testing.allocator;
    const r = try host_root(a, "http://127.0.0.1:8081/gqf2008/walgit.git");
    defer a.free(r);
    try std.testing.expectEqualStrings("http://127.0.0.1:8081", r);
    const r2 = try host_root(a, "https://walgit.localhost:8080/owner/repo");
    defer a.free(r2);
    try std.testing.expectEqualStrings("https://walgit.localhost:8080", r2);
    try std.testing.expectError(error.GitFailed, host_root(a, "git@host:repo.git"));
}
