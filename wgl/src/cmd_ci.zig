//! `wgl ci` — the decentralized CI protocol runner (D1_CI_PROTOCOL §3–§9),
//! ported from the retired `walgit ci` family. walgit runs no CI: a runner
//! is a credentialed client that polls ref tips, claims a run with a signed
//! `ci_claim`, executes the tested commit's `.walgit/ci.toml` in a detached
//! worktree, and publishes a signed `ci_result` into its inbox.

const std = @import("std");
const args = @import("args.zig");
const ci = @import("d1/ci.zig");
const entry_mod = @import("d1/entry.zig");
const ciconfig = @import("ciconfig.zig");
const cron = @import("cron.zig");
const gitx = @import("gitx.zig");
const identity = @import("identity.zig");
const jsonio = @import("jsonio.zig");
const collabreader = @import("collabreader.zig");
const httpc = @import("httpc.zig");
const util = @import("util.zig");

const Io = std.Io;
const Entry = entry_mod.Entry;
const EntryRef = entry_mod.EntryRef;
const Ctx = cmd_collab_ctx();
fn cmd_collab_ctx() type {
    return @import("cmd_collab.zig").Ctx;
}

pub fn dispatch(c: *Ctx, action: []const u8, rest: []const []const u8) u8 {
    const S = args.Spec;
    var specs: []const S = &.{};
    if (eq(action, "validate")) {
        specs = &.{ S{ .name = "repo" }, S{ .name = "file" } };
    } else if (eq(action, "run")) {
        specs = &.{
            S{ .name = "repo" },                  S{ .name = "remote" },
            S{ .name = "actor" },                 S{ .name = "key" },
            S{ .name = "once", .boolean = true }, S{ .name = "interval" },
            S{ .name = "claim-ttl" },             S{ .name = "task" },
            S{ .name = "state" },
        };
    } else if (eq(action, "status")) {
        specs = &.{ S{ .name = "repo" }, S{ .name = "format" } };
    } else if (eq(action, "log") or eq(action, "artifacts")) {
        specs = &.{
            S{ .name = "repo" }, S{ .name = "remote" },
            S{ .name = "run" },  S{ .name = "out" },
        };
    }
    var p = args.parse(c.alloc, rest, specs) catch |err| switch (err) {
        error.OutOfMemory => return 1,
        else => return 2,
    };
    defer p.deinit();

    if (eq(action, "validate")) return ci_validate(c, &p) catch 1;
    if (eq(action, "run")) return ci_run(c, &p) catch 1;
    if (eq(action, "status")) return ci_status(c, &p) catch 1;
    if (eq(action, "log")) return ci_log(c, &p) catch 1;
    if (eq(action, "artifacts")) return ci_artifacts(c, &p) catch 1;
    std.debug.print("wgl: unknown ci action \"{s}\" (validate|run|status|log|artifacts)\n", .{action});
    return 2;
}

fn eq(a: []const u8, b: []const u8) bool {
    return std.mem.eql(u8, a, b);
}

fn ci_validate(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const path = p.opt("file") orelse try std.fs.path.join(c.alloc, &.{ repo, ".walgit", "ci.toml" });
    var d = std.Io.Dir.cwd();
    const raw = d.readFileAlloc(c.io, path, c.alloc, .limited(ciconfig.FILE_MAX_BYTES + 1)) catch {
        std.debug.print("wgl: read {s} failed\n", .{path});
        return error.ReadFailed;
    };
    const resolved = ciconfig.parse_and_validate(c.alloc, raw) catch |err| {
        if (err == error.Invalid) {
            std.debug.print("wgl: {s}: {s}\n", .{ path, ciconfig.validation_msg });
        } else {
            std.debug.print("wgl: {s}: {t}\n", .{ path, err });
        }
        return error.InvalidConfig;
    };
    c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s}: ok — claim_ttl {d}s, {d} task(s)", .{ path, resolved.claim_ttl_secs, resolved.tasks.len }));
    var out: util.List(u8) = .init(c.alloc);
    for (resolved.tasks) |*t| {
        var refs: util.List(u8) = .init(c.alloc);
        for (t.refs, 0..) |r, i| {
            if (i > 0) refs.append_slice(", ") catch @panic("oom");
            refs.append_slice(r) catch @panic("oom");
        }
        var env: util.List(u8) = .init(c.alloc);
        for (t.env_allow, 0..) |r, i| {
            if (i > 0) env.append_slice(", ") catch @panic("oom");
            env.append_slice(r) catch @panic("oom");
        }
        out.append_slice(try std.fmt.allocPrint(c.alloc, "  {s} refs [{s}] timeout {d}s attempts {d} env_allow [{s}]{s}\n    {s}\n", .{
            t.name,
            refs.items,
            t.timeout_secs,
            t.max_attempts,
            env.items,
            if (t.schedule) |s| std.fmt.allocPrint(c.alloc, " schedule \"{s}\"", .{s}) catch @panic("oom") else "",
            t.command,
        })) catch @panic("oom");
    }
    c.stdout(out.items);
    return 0;
}

fn ci_status(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const format = p.get("format", "text");
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    const now = util.now_ts();
    const runs = try ci.collect_runs(c.alloc, loaded.entries, loaded.principals, now);
    if (eq(format, "json")) {
        c.stdout_line(try runs_json(c, now, runs));
        return 0;
    }
    const render = @import("render.zig");
    var buf: util.List(u8) = .init(c.alloc);
    if (eq(format, "markdown")) {
        buf.append_slice(try std.fmt.allocPrint(c.alloc, "ci status: {d} run(s)\n\n", .{runs.len})) catch @panic("oom");
        buf.append_slice(render.ci_runs_markdown(c.alloc, runs)) catch @panic("oom");
    } else if (eq(format, "text")) {
        buf.append_slice(try std.fmt.allocPrint(c.alloc, "ci status: {d} run(s)\n", .{runs.len})) catch @panic("oom");
        buf.append_slice(render.ci_runs_text(c.alloc, runs)) catch @panic("oom");
    } else {
        std.debug.print("wgl: unknown ci status format {s} (text|markdown|json)\n", .{format});
        return error.BadFormat;
    }
    c.stdout(buf.items);
    return 0;
}

/// `{"now": …, "runs": [RunView…]}` — the json! wrapper sorts the top level;
/// each run keeps its struct field order (the wire order the SPA reads).
fn runs_json(c: *Ctx, now: i64, runs: []const ci.RunEntry) ![]u8 {
    var out: util.List(u8) = .init(c.alloc);
    out.append_slice(try std.fmt.allocPrint(c.alloc, "{{\n  \"now\": {d},\n  \"runs\": ", .{now})) catch @panic("oom");
    if (runs.len == 0) {
        out.append_slice("[]\n}") catch @panic("oom");
        return out.items;
    }
    out.append('[') catch @panic("oom");
    for (runs, 0..) |kv, i| {
        if (i > 0) out.append(',') catch @panic("oom");
        out.append('\n') catch @panic("oom");
        out.append_slice(try run_view_json(c, &kv.view)) catch @panic("oom");
    }
    out.append_slice("\n  ]\n}") catch @panic("oom");
    return out.items;
}

fn run_view_json(c: *Ctx, r: *const ci.RunView) ![]u8 {
    var out: util.List(u8) = .init(c.alloc);
    const w = struct {
        fn f(o: *util.List(u8), comptime fmt: []const u8, a: anytype) void {
            const s = std.fmt.allocPrint(o.alloc, fmt, a) catch @panic("oom");
            o.append_slice(s) catch @panic("oom");
        }
    }.f;
    w(&out, "    {{\"id\":", .{});
    jsonio.write_string(&out, r.id);
    w(&out, ",\"task\":", .{});
    jsonio.write_string(&out, r.task);
    w(&out, ",\"repo_ref\":", .{});
    jsonio.write_string(&out, r.repo_ref);
    w(&out, ",\"commit\":", .{});
    jsonio.write_string(&out, r.commit);
    w(&out, ",\"attempts\":", .{});
    if (r.attempts.len == 0) {
        out.append_slice("[]") catch @panic("oom");
    } else {
        out.append('[') catch @panic("oom");
        for (r.attempts, 0..) |a, i| {
            if (i > 0) out.append(',') catch @panic("oom");
            out.append_slice(try attempt_json(c, &a)) catch @panic("oom");
        }
        out.append(']') catch @panic("oom");
    }
    w(&out, ",\"latest_attempt\":{d},\"state\":\"{s}\",\"conclusion\":", .{ r.latest_attempt, r.state.as_str() });
    if (r.conclusion) |cc| {
        w(&out, "\"{s}\"", .{cc.as_str()});
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    w(&out, ",\"runner\":", .{});
    if (r.runner) |runner| {
        jsonio.write_string(&out, runner);
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    w(&out, ",\"last_ts\":{d},\"unverified\":{d}}}", .{ r.last_ts, r.unverified });
    return out.items;
}

fn attempt_json(c: *Ctx, a: *const ci.AttemptView) ![]u8 {
    var out: util.List(u8) = .init(c.alloc);
    const w = struct {
        fn f(o: *util.List(u8), comptime fmt: []const u8, arg: anytype) void {
            const s = std.fmt.allocPrint(o.alloc, fmt, arg) catch @panic("oom");
            o.append_slice(s) catch @panic("oom");
        }
    }.f;
    w(&out, "      {{\"attempt\":{d},\"claims\":", .{a.attempt});
    try claim_array(c, &out, a.claims);
    w(&out, ",\"winner\":", .{});
    if (a.winner) |winner| {
        out.append_slice(try claim_json(c, &winner)) catch @panic("oom");
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    w(&out, ",\"results\":", .{});
    if (a.results.len == 0) {
        out.append_slice("[]") catch @panic("oom");
    } else {
        out.append('[') catch @panic("oom");
        for (a.results, 0..) |*res, i| {
            if (i > 0) out.append(',') catch @panic("oom");
            out.append_slice(try result_json(c, res)) catch @panic("oom");
        }
        out.append(']') catch @panic("oom");
    }
    w(&out, ",\"effective\":", .{});
    if (a.effective) |*eff| {
        out.append_slice(try result_json(c, eff)) catch @panic("oom");
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    w(&out, ",\"state\":\"{s}\"}}", .{a.state.as_str()});
    return out.items;
}

fn claim_array(c: *Ctx, out: *util.List(u8), claims: []const ci.CiClaim) !void {
    if (claims.len == 0) {
        out.append_slice("[]") catch @panic("oom");
        return;
    }
    out.append('[') catch @panic("oom");
    for (claims, 0..) |*claim, i| {
        if (i > 0) out.append(',') catch @panic("oom");
        out.append_slice(try claim_json(c, claim)) catch @panic("oom");
    }
    out.append(']') catch @panic("oom");
}

fn claim_json(c: *Ctx, claim: *const ci.CiClaim) ![]u8 {
    return std.fmt.allocPrint(c.alloc, "{{\"oid\":{s},\"actor\":{s},\"ts\":{d},\"task\":{s},\"repo_ref\":{s},\"commit\":{s},\"ttl_secs\":{d},\"attempt\":{d}}}", .{
        quoted(c, claim.oid),      quoted(c, claim.actor),  claim.ts,       quoted(c, claim.task),
        quoted(c, claim.repo_ref), quoted(c, claim.commit), claim.ttl_secs, claim.attempt,
    });
}

fn result_json(c: *Ctx, r: *const ci.CiResult) ![]u8 {
    var out: util.List(u8) = .init(c.alloc);
    out.append_slice(try std.fmt.allocPrint(c.alloc, "{{\"oid\":{s},\"actor\":{s},\"ts\":{d},\"task\":{s},\"repo_ref\":{s},\"commit\":{s},\"attempt\":{d},\"claim\":{s},\"conclusion\":\"{s}\",\"exit_code\":", .{
        quoted(c, r.oid),      quoted(c, r.actor),
        r.ts,                  quoted(c, r.task),
        quoted(c, r.repo_ref), quoted(c, r.commit),
        r.attempt,             quoted(c, r.claim),
        r.conclusion.as_str(),
    })) catch @panic("oom");
    if (r.exit_code) |code| {
        out.append_slice(try std.fmt.allocPrint(c.alloc, "{d}", .{code})) catch @panic("oom");
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    out.append_slice(",\"duration_ms\":") catch @panic("oom");
    if (r.duration_ms) |d| {
        out.append_slice(try std.fmt.allocPrint(c.alloc, "{d}", .{d})) catch @panic("oom");
    } else {
        out.append_slice("null") catch @panic("oom");
    }
    out.append_slice(",\"log_summary\":") catch @panic("oom");
    out.append_slice(quoted(c, r.log_summary)) catch @panic("oom");
    out.append_slice(",\"log_sha256\":") catch @panic("oom");
    out.append_slice(quoted(c, r.log_sha256)) catch @panic("oom");
    out.append_slice(try std.fmt.allocPrint(c.alloc, ",\"log_truncated\":{}", .{r.log_truncated})) catch @panic("oom");
    out.append_slice(",\"artifacts\":") catch @panic("oom");
    if (r.artifacts.len == 0) {
        out.append_slice("[]") catch @panic("oom");
    } else {
        out.append('[') catch @panic("oom");
        for (r.artifacts, 0..) |*a, i| {
            if (i > 0) out.append(',') catch @panic("oom");
            out.append_slice(try std.fmt.allocPrint(c.alloc, "{{\"name\":{s},\"path\":{s},\"sha256\":{s},\"bytes\":{d},\"url\":{s}}}", .{
                quoted(c, a.name),                       quoted(c, a.path), quoted(c, a.sha256), a.bytes,
                if (a.url) |u| quoted(c, u) else "null",
            })) catch @panic("oom");
        }
        out.append(']') catch @panic("oom");
    }
    out.append('}') catch @panic("oom");
    return out.items;
}

fn quoted(c: *Ctx, s: []const u8) []u8 {
    var out: util.List(u8) = .init(c.alloc);
    jsonio.write_string(&out, s);
    return out.items;
}

// ---- log / artifacts (§8.2 on-demand reads) ------------------------------------------------

fn latest_effective_result(c: *Ctx, repo: []const u8, remote: []const u8, run_id: ?[]const u8) !struct { id: []const u8, result: ci.CiResult } {
    gitx.fetch_collab(c.alloc, c.io, repo, remote) catch |err| {
        std.debug.print("wgl: fetch collab refs failed ({t}); reading local state\n", .{err});
    };
    const reader = collabreader.Reader.init(c.alloc, c.io, repo);
    const loaded = try reader.load(c.alloc);
    const now = util.now_ts();
    const runs = try ci.collect_runs(c.alloc, loaded.entries, loaded.principals, now);
    var view: ?*const ci.RunView = null;
    if (run_id) |id| {
        for (runs) |*kv| {
            if (std.mem.eql(u8, kv.id, id)) view = &kv.view;
        }
        if (view == null) {
            std.debug.print("wgl: run {s} not found\n", .{id});
            return error.RunNotFound;
        }
    } else {
        var best: ?*const ci.RunView = null;
        for (runs) |*kv| {
            if (best == null or kv.view.last_ts > best.?.last_ts) best = &kv.view;
        }
        view = best;
        if (view == null) {
            std.debug.print("wgl: no runs yet\n", .{});
            return error.NoRuns;
        }
    }
    const v = view.?;
    if (v.attempts.len == 0 or v.attempts[v.attempts.len - 1].effective == null) {
        std.debug.print("wgl: run {s} has no effective result\n", .{v.id});
        return error.NoEffectiveResult;
    }
    return .{ .id = v.id, .result = v.attempts[v.attempts.len - 1].effective.? };
}

fn sha256_hex(alloc: std.mem.Allocator, bytes: []const u8) []u8 {
    var digest: [32]u8 = undefined;
    std.crypto.hash.sha2.Sha256.hash(bytes, &digest, .{});
    return util.to_hex(alloc, &digest);
}

/// The bytes of one artifact/log object: a git blob under
/// `refs/collab/ci-artifacts/*/<sha256>`, fetched on demand and sha256
/// verified before it is handed out. null = not published.
fn fetch_ci_object(c: *Ctx, repo: []const u8, remote: []const u8, actor: []const u8, sha: []const u8) !?[]u8 {
    if (try read_ci_object_local(c, repo, sha)) |bytes| return bytes;
    // The walgit endpoint answers 413 for oversized payloads; a pure Git
    // remote has no size metadata channel, so preflight fail closed.
    if (remote_artifact_fits_cap(c, repo, remote, actor, sha)) |fits| {
        if (!fits) return null;
    } else |_| {}
    if (!(try gitx.fetch_ci_artifact(c.alloc, c.io, repo, remote, actor, sha))) return null;
    return read_ci_object_local(c, repo, sha);
}

fn remote_artifact_fits_cap(c: *Ctx, repo: []const u8, remote: []const u8, actor: []const u8, sha: []const u8) !bool {
    const remote_url = try gitx.remote_url(c.alloc, c.io, repo, remote);
    if (std.mem.indexOf(u8, remote_url, "://") == null) {
        std.debug.print("wgl: remote \"{s}\" has no walgit artifact size endpoint; refused to fetch {s} before a size check\n", .{ remote_url, sha });
        return error.NoHttpRemote;
    }
    const status = httpc.artifact_head(c.alloc, c.io, repo, remote_url, actor, sha) catch |err| {
        std.debug.print("wgl: HEAD artifact size: {t}\n", .{err});
        return err;
    };
    return switch (status) {
        200 => true,
        404 => false,
        413 => blk: {
            std.debug.print("wgl: artifact {s}: object exceeds the {d}-byte cap; skipped before fetch\n", .{ sha, ci.CI_ARTIFACT_MAX_BYTES });
            break :blk false;
        },
        else => blk: {
            std.debug.print("wgl: HEAD artifact size: unexpected HTTP {d}\n", .{status});
            break :blk error.HttpFailed;
        },
    };
}

fn read_ci_object_local(c: *Ctx, repo: []const u8, sha: []const u8) !?[]u8 {
    const tips = try gitx.for_each_ref(c.alloc, c.io, repo, &.{ci.CI_ARTIFACT_REF_PREFIX});
    for (tips) |tip| {
        if (!std.mem.endsWith(u8, tip.name, sha)) continue;
        const size = (try gitx.cat_size(c.alloc, c.io, repo, tip.oid)) orelse continue;
        if (size > ci.CI_ARTIFACT_MAX_BYTES) {
            std.debug.print("wgl: artifact {s}: object {s} exceeds the {d}-byte cap; ignored\n", .{ sha, tip.oid, ci.CI_ARTIFACT_MAX_BYTES });
            continue;
        }
        const bytes = try gitx.cat_blob(c.alloc, c.io, repo, tip.oid);
        const actual = sha256_hex(c.alloc, bytes);
        if (!std.mem.eql(u8, actual, sha)) {
            std.debug.print("wgl: artifact {s}: object {s} hashes to {s}; ignored\n", .{ sha, tip.oid, actual });
            continue;
        }
        return bytes;
    }
    return null;
}

fn ci_log(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const picked = try latest_effective_result(c, repo, remote, p.opt("run"));
    const empty_sha = sha256_hex(c.alloc, "");
    const result = &picked.result;
    if (result.log_sha256.len == 0 or std.mem.eql(u8, result.log_sha256, empty_sha)) {
        // An empty capture was never uploaded; the summary is the log.
        c.stdout(result.log_summary);
        return 0;
    }
    if (result.log_truncated) {
        std.debug.print("wgl: run {s}: stored log exceeds the retention cap; showing the retained tail\n", .{picked.id});
    }
    if (try fetch_ci_object(c, repo, remote, result.actor, result.log_sha256)) |bytes| {
        c.stdout(bytes);
    } else {
        std.debug.print("wgl: run {s}: log object {s} not published; the stored summary follows\n", .{ picked.id, result.log_sha256 });
        c.stdout(result.log_summary);
    }
    return 0;
}

fn ci_artifacts(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const out_dir = p.get("out", ".");
    const picked = try latest_effective_result(c, repo, remote, p.opt("run"));
    const result = &picked.result;
    if (result.artifacts.len == 0) {
        c.stdout_line(try std.fmt.allocPrint(c.alloc, "wgl: run {s} declares no artifacts", .{picked.id}));
        return 0;
    }
    var d = std.Io.Dir.cwd();
    if (d.createDirPathOpen(c.io, out_dir, .{})) |_| {} else |err| {
        std.debug.print("wgl: create {s}: {t}\n", .{ out_dir, err });
        return error.CreateDirFailed;
    }
    var names: util.List([]const u8) = .init(c.alloc);
    for (result.artifacts) |*a| {
        // §8.2 name is a base name; a hostile writer could smuggle a path —
        // never write outside --out.
        const fname = std.fs.path.basename(a.name);
        if (fname.len == 0) {
            std.debug.print("wgl: {s}: unusable name, skipped\n", .{a.name});
            continue;
        }
        var dup = false;
        for (names.items) |n| {
            if (std.mem.eql(u8, n, fname)) dup = true;
        }
        if (dup) {
            std.debug.print("wgl: {s}: duplicate artifact name, skipped\n", .{fname});
            continue;
        }
        try names.append(fname);
        const bytes = (try fetch_ci_object(c, repo, remote, result.actor, a.sha256)) orelse {
            if (a.url) |u| {
                c.stdout_line(try std.fmt.allocPrint(c.alloc, "wgl: {s}: external artifact, see {s}", .{ fname, u }));
            } else {
                std.debug.print("wgl: {s}: object {s} not published, skipped\n", .{ fname, a.sha256 });
            }
            continue;
        };
        const dest = try std.fs.path.join(c.alloc, &.{ out_dir, fname });
        // create_new: refusing to overwrite or follow an existing path.
        const f = d.createFile(c.io, dest, .{ .exclusive = true }) catch {
            std.debug.print("wgl: create {s} failed (refusing to overwrite)\n", .{dest});
            continue;
        };
        f.writeStreamingAll(c.io, bytes) catch {};
        f.close(c.io);
        c.stdout_line(try std.fmt.allocPrint(c.alloc, "wgl: {s} ({d} bytes, sha256 verified) -> {s}", .{ fname, a.bytes, dest }));
    }
    return 0;
}

// ---- the runner (§4–§9) ---------------------------------------------------------------------

const BASE_ENV_ALLOW = [_][]const u8{
    "PATH",    "HOME",        "TMPDIR",      "TEMP",
    "TMP",     "SYSTEMROOT",  "SYSTEMDRIVE", "COMSPEC",
    "PATHEXT", "USERPROFILE",
};
const LOG_CAPTURE_MAX: usize = 16 * 1024 * 1024;
const LOG_SUMMARY_MAX: usize = 4096;
const ERROR_RECLAIMS_MAX: u32 = 3;

const CollectedArtifact = struct {
    name: []const u8,
    path: []const u8,
    sha256: []const u8,
    bytes: u64,
    oid: []const u8,
};

const ExecOutcome = struct {
    conclusion: ci.Conclusion,
    exit_code: ?i64,
    duration_ms: u64,
    log: []const u8,
    log_truncated: bool,
    artifacts: []CollectedArtifact = &.{},
};

const TaskOutcome = union(enum) {
    settled: ci.Conclusion,
    deferred,
};

const Runner = struct {
    c: *Ctx,
    repo: []const u8,
    remote: []const u8,
    actor: []const u8,
    key: entry_mod.SigningKey,
    state_file: []const u8,
    task_filter: ?[]const u8,

    fn ref_segment_ok(self: *const Runner) void {
        _ = self;
    }

    /// First-use self-registration (§2/§11): idempotent — re-registering is
    /// an explicit principal-register, never a side effect of `run`.
    fn register(self: *const Runner) !void {
        const ref_name = try std.fmt.allocPrint(self.c.alloc, "refs/collab/meta/principals/{s}", .{self.actor});
        gitx.fetch_collab(self.c.alloc, self.c.io, self.repo, self.remote) catch {};
        if (try gitx.ref_exists(self.c.alloc, self.c.io, self.repo, ref_name)) return;
        const public_key = entry_mod.public_key_b64(self.c.alloc, self.key);
        var o = jsonio.new_object();
        jsonio.obj_put(&o, self.c.alloc, "version", .{ .integer = 1 });
        jsonio.obj_put(&o, self.c.alloc, "principal", .{ .string = self.actor });
        jsonio.obj_put(&o, self.c.alloc, "public_key", .{ .string = public_key });
        jsonio.obj_put(&o, self.c.alloc, "registered_at", .{ .integer = util.now_ts() });
        const content = jsonio.to_string_pretty(self.c.alloc, .{ .object = o });
        const oid = try gitx.write_blob(self.c.alloc, self.c.io, self.repo, content);
        try gitx.update_ref(self.c.alloc, self.c.io, self.repo, ref_name, oid);
        try gitx.push(self.c.alloc, self.c.io, self.repo, self.remote, .{ .refspecs = &.{ref_name} });
    }

    /// One subscription pass (§4): diff the remote's tips against the state
    /// file, work every changed ref to a terminal state, then the cron
    /// sweep (§4.3). Returns the process exit code contribution.
    fn run_pass(self: *const Runner, ttl_override: ?u64) !i32 {
        gitx.fetch_collab(self.c.alloc, self.c.io, self.repo, self.remote) catch |err| {
            std.debug.print("wgl: fetching collab refs failed ({t}); continuing with local state\n", .{err});
        };
        var state = read_runner_state(self.c, self.state_file);
        var exit: i32 = 0;
        var dirty = false;
        const tips = try gitx.ls_tips(self.c.alloc, self.c.io, self.repo, self.remote);
        // Drop state for refs the remote no longer has (§4: prune).
        var stale: util.List([]const u8) = .init(self.c.alloc);
        {
            var it = state.processed.iterator();
            while (it.next()) |kv| {
                var live = false;
                for (tips) |tip| {
                    if (std.mem.eql(u8, tip.name, kv.key_ptr.*)) live = true;
                }
                if (!live) try stale.append(kv.key_ptr.*);
            }
            var it2 = state.schedules.iterator();
            while (it2.next()) |kv| {
                const sep = std.mem.indexOfScalar(u8, kv.key_ptr.*, 0x1f) orelse continue;
                const ref_name = kv.key_ptr.*[0..sep];
                var live = false;
                for (tips) |tip| {
                    if (std.mem.eql(u8, tip.name, ref_name)) live = true;
                }
                if (!live) try stale.append(ref_name);
            }
        }
        for (stale.items) |ref_name| {
            _ = state.processed.remove(ref_name);
            var it = state.schedules.iterator();
            while (it.next()) |kv| {
                if (std.mem.startsWith(u8, kv.key_ptr.*, ref_name) and kv.key_ptr.*.len > ref_name.len and kv.key_ptr.*[ref_name.len] == 0x1f) {
                    _ = state.schedules.remove(kv.key_ptr.*);
                }
            }
            var it2 = state.cron.iterator();
            while (it2.next()) |kv| {
                if (std.mem.startsWith(u8, kv.key_ptr.*, ref_name) and kv.key_ptr.*.len > ref_name.len and kv.key_ptr.*[ref_name.len] == 0x1f) {
                    _ = state.cron.remove(kv.key_ptr.*);
                }
            }
            dirty = true;
        }
        for (tips) |tip| {
            if (state.processed.get(tip.name)) |seen| {
                if (std.mem.eql(u8, seen, tip.oid) and !state.schedule_migration.contains(tip.name)) continue;
            }
            const outcome = self.process_ref(tip.name, ttl_override) catch |err| {
                std.debug.print("wgl: {s}: {t}\n", .{ tip.name, err });
                exit = 1;
                continue;
            };
            {
                // Persist the schedule snapshot for quiet cron passes.
                const preserve_cron = state.schedule_migration.contains(tip.name) and blk: {
                    if (state.processed.get(tip.name)) |seen| break :blk std.mem.eql(u8, seen, tip.oid);
                    break :blk false;
                };
                apply_schedule_snapshot(&state, tip.name, outcome.schedules, self.task_filter, preserve_cron);
                if (state.schedule_migration.remove(tip.name)) dirty = true;
                if (outcome.terminal) {
                    try state.processed.put(tip.name, tip.oid);
                    dirty = true;
                }
                if (outcome.failed) exit = 1;
            }
        }
        // §4.3: sweep every ref whose tip is fully processed — an unmoved
        // ref is exactly what a schedule fires on.
        const now = util.now_ts();
        var sched_keys: util.List([]const u8) = .init(self.c.alloc);
        {
            var it = state.schedules.iterator();
            while (it.next()) |kv| try sched_keys.append(kv.key_ptr.*);
        }
        std.mem.sort([]const u8, sched_keys.items, {}, struct {
            fn f(_: void, a: []const u8, b: []const u8) bool {
                return std.mem.order(u8, a, b) == .lt;
            }
        }.f);
        for (sched_keys.items) |key| {
            const sep = std.mem.indexOfScalar(u8, key, 0x1f) orelse continue;
            const ref_name = key[0..sep];
            const task_name = key[sep + 1 ..];
            if (self.task_filter) |f| {
                if (!std.mem.eql(u8, f, task_name)) continue;
            }
            var tip: ?[]const u8 = null;
            for (tips) |t| {
                if (std.mem.eql(u8, t.name, ref_name)) tip = t.oid;
            }
            const tip_oid = tip orelse continue;
            const seen = state.processed.get(ref_name) orelse continue;
            if (!std.mem.eql(u8, seen, tip_oid)) continue;
            const expr = state.schedules.get(key) orelse continue;
            const swept = self.cron_sweep_one(ref_name, tip_oid, task_name, expr, &state, ttl_override, now, &exit) catch |err| {
                std.debug.print("wgl: {s}: cron sweep: {t}\n", .{ ref_name, err });
                exit = 1;
                continue;
            };
            dirty = dirty or swept;
        }
        if (dirty) try write_runner_state(self.c, self.state_file, &state);
        return exit;
    }

    /// §4.3: evaluate one persisted scheduled task of an unmoved ref.
    fn cron_sweep_one(self: *const Runner, ref_name: []const u8, tip: []const u8, task_name: []const u8, expr: []const u8, state: *RunnerState, ttl_override: ?u64, now: i64, exit: *i32) !bool {
        const key = try schedule_key(self.c.alloc, ref_name, task_name);
        const last = state.cron.get(key) orelse {
            try state.cron.put(key, now);
            return true;
        };
        var sched_arena = std.heap.ArenaAllocator.init(self.c.alloc);
        defer sched_arena.deinit();
        const schedule = cron.parse_schedule_a(sched_arena.allocator(), expr) catch {
            _ = state.schedules.remove(key);
            _ = state.cron.remove(key);
            return true;
        };
        switch (cron.due_slot(&schedule, last, now)) {
            .not_due => return false,
            .skip => |slot| {
                try state.cron.put(key, slot);
                return true;
            },
            .run => |slot| {
                const commit = try gitx.fetch_commit(self.c.alloc, self.c.io, self.repo, self.remote, ref_name);
                const raw = (try gitx.read_commit_file(self.c.alloc, self.c.io, self.repo, commit, ".walgit/ci.toml")) orelse {
                    _ = state.schedules.remove(key);
                    _ = state.cron.remove(key);
                    return true;
                };
                const cfg = ciconfig.parse_and_validate(self.c.alloc, raw) catch {
                    _ = state.schedules.remove(key);
                    _ = state.cron.remove(key);
                    return true;
                };
                var found: ?*const ciconfig.CiResolvedTask = null;
                for (try cfg.matching(self.c.alloc, ref_name)) |t| {
                    if (std.mem.eql(u8, t.name, task_name)) {
                        if (t.schedule != null and std.mem.eql(u8, t.schedule.?, expr)) found = t;
                    }
                }
                const task = found orelse {
                    _ = state.schedules.remove(key);
                    _ = state.cron.remove(key);
                    return true;
                };
                const ttl = ttl_override orelse cfg.claim_ttl_secs;
                const id = try ci.scheduled_run_id(self.c.alloc, task_name, ref_name, tip, slot);
                const short = if (tip.len > 8) tip[0..8] else tip;
                std.debug.print("wgl: {s} @ {s} task {s}: schedule fired (slot {d}), run {s}\n", .{ ref_name, short, task_name, slot, id });
                const outcome = try self.process_task(task, ref_name, tip, ttl, id);
                switch (outcome) {
                    .settled => |conclusion| {
                        try state.cron.put(key, slot);
                        if (conclusion != .success) {
                            std.debug.print("wgl: {s} @ {s} task {s}: settled {s}\n", .{ ref_name, short, task_name, conclusion.as_str() });
                            exit.* = 1;
                        }
                        return true;
                    },
                    .deferred => return false,
                }
            },
        }
    }

    /// All tasks of one ref tip (§3.2/§4). The tip is recorded as processed
    /// only once **every** task of the tip reached a terminal state.
    fn process_ref(self: *const Runner, ref_name: []const u8, ttl_override: ?u64) !RefOutcome {
        const commit = try gitx.fetch_commit(self.c.alloc, self.c.io, self.repo, self.remote, ref_name);
        const short = if (commit.len > 8) commit[0..8] else commit;
        const raw = (try gitx.read_commit_file(self.c.alloc, self.c.io, self.repo, commit, ".walgit/ci.toml")) orelse {
            std.debug.print("wgl: {s} @ {s}: no .walgit/ci.toml\n", .{ ref_name, short });
            return .{ .terminal = true, .failed = false, .schedules = &.{} };
        };
        const cfg = ciconfig.parse_and_validate(self.c.alloc, raw) catch |err| {
            const msg = if (err == error.Invalid) ciconfig.validation_msg else "";
            std.debug.print("wgl: {s} @ {s}: invalid .walgit/ci.toml, skipping: {s}\n", .{ ref_name, short, msg });
            return .{ .terminal = true, .failed = true, .schedules = &.{} };
        };
        var schedules: util.List(ScheduleEntry) = util.List(ScheduleEntry).init(self.c.alloc);
        for (try cfg.matching(self.c.alloc, ref_name)) |t| {
            try schedules.append(.{ .name = t.name, .expr = t.schedule });
        }
        const ttl = ttl_override orelse cfg.claim_ttl_secs;
        var all_terminal = true;
        var any_failed = false;
        for (try cfg.matching(self.c.alloc, ref_name)) |t| {
            if (self.task_filter) |f| {
                if (!std.mem.eql(u8, f, t.name)) continue;
            }
            const id = try ci.run_id(self.c.alloc, t.name, ref_name, commit);
            const outcome = try self.process_task(t, ref_name, commit, ttl, id);
            switch (outcome) {
                .settled => |conclusion| {
                    if (conclusion != .success) {
                        std.debug.print("wgl: {s} @ {s} task {s}: settled {s}\n", .{ ref_name, short, t.name, conclusion.as_str() });
                        any_failed = true;
                    }
                },
                .deferred => all_terminal = false,
            }
        }
        return .{ .terminal = all_terminal, .failed = any_failed, .schedules = schedules.items };
    }

    /// One task of one ref tip: decide, claim, execute, publish — repeat
    /// until a terminal state or a hand-off (§6.2).
    fn process_task(self: *const Runner, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, ttl: u64, id: []const u8) !TaskOutcome {
        var error_reclaims: u32 = 0;
        while (true) {
            const decision = blk: {
                const view = (try self.run_view(id)) orelse break :blk ci.Decision{ .claim = .{ .attempt = 1 } };
                break :blk ci.decide(&view, self.actor, task.max_attempts);
            };
            switch (decision) {
                .settled => |s| return .{ .settled = s.conclusion },
                .stand_down => |s| {
                    std.debug.print("wgl: run {s} task {s}: held by {s}, standing down\n", .{ id, task.name, s.holder });
                    return .deferred;
                },
                .@"resume" => |r| {
                    if (!try self.run_once(task, ref_name, commit, id, r.attempt, r.claim_oid, &error_reclaims)) {
                        return .deferred;
                    }
                },
                .claim => |cl| {
                    const claim_oid = try self.publish_claim(id, task, ref_name, commit, ttl, cl.attempt);
                    // The convergence point (§6.2 step 3): re-read the log;
                    // only the deterministic winner executes.
                    const recheck = blk: {
                        const view = (try self.run_view(id)) orelse break :blk ci.Decision{ .claim = .{ .attempt = 1 } };
                        break :blk ci.decide(&view, self.actor, task.max_attempts);
                    };
                    switch (recheck) {
                        .@"resume" => |r| {
                            if (std.mem.eql(u8, r.claim_oid, claim_oid)) {
                                if (!try self.run_once(task, ref_name, commit, id, r.attempt, claim_oid, &error_reclaims)) {
                                    return .deferred;
                                }
                            } else {
                                return .deferred;
                            }
                        },
                        .stand_down => |s| {
                            std.debug.print("wgl: run {s} task {s}: lost the claim race, held by {s}\n", .{ id, task.name, s.holder });
                            return .deferred;
                        },
                        else => return .deferred,
                    }
                },
            }
        }
    }

    /// Execute once under our claim and publish the result. false = stop
    /// working the run this pass (error re-claim cap, §6.3).
    fn run_once(self: *const Runner, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, id: []const u8, attempt: u32, claim_oid: []const u8, error_reclaims: *u32) !bool {
        const outcome = self.execute(task, ref_name, commit, id, attempt);
        // §8.2: the log + artifact bytes travel through receive-pack BEFORE
        // the result references them — a published reference must never
        // dangle. An upload failure degrades to references-only.
        var artifacts: []std.json.Value = &.{};
        if (self.upload_run_objects(&outcome)) |a| {
            artifacts = a;
        } else |err| {
            std.debug.print("wgl: run {s}: object upload failed ({t}); publishing without them\n", .{ id, err });
        }
        try self.publish_result(id, task, ref_name, commit, attempt, claim_oid, &outcome, artifacts);
        if (outcome.conclusion != .@"error") return true;
        error_reclaims.* += 1;
        if (error_reclaims.* >= ERROR_RECLAIMS_MAX) {
            std.debug.print("wgl: run {s}: infrastructure keeps failing; deferring to the next pass\n", .{id});
            return false;
        }
        return true;
    }

    /// The captured log and every collected artifact become content-
    /// addressed git blobs under `refs/collab/ci-artifacts/<actor>/<sha256>`,
    /// pushed in ONE receive-pack round trip (§8.2).
    fn upload_run_objects(self: *const Runner, outcome: *const ExecOutcome) ![]std.json.Value {
        var refs = std.StringHashMap([]const u8).init(self.c.alloc);
        if (outcome.log.len != 0) {
            const sha = sha256_hex(self.c.alloc, outcome.log);
            const oid = try gitx.write_blob(self.c.alloc, self.c.io, self.repo, outcome.log);
            const name = try std.fmt.allocPrint(self.c.alloc, "{s}{s}/{s}", .{ ci.CI_ARTIFACT_REF_PREFIX, self.actor, sha });
            try refs.put(name, oid);
        }
        for (outcome.artifacts) |*a| {
            const name = try std.fmt.allocPrint(self.c.alloc, "{s}{s}/{s}", .{ ci.CI_ARTIFACT_REF_PREFIX, self.actor, a.sha256 });
            try refs.put(name, a.oid);
        }
        var names: util.List([]const u8) = .init(self.c.alloc);
        var it = refs.iterator();
        while (it.next()) |kv| {
            try gitx.update_ref(self.c.alloc, self.c.io, self.repo, kv.key_ptr.*, kv.value_ptr.*);
            try names.append(kv.key_ptr.*);
        }
        try gitx.push(self.c.alloc, self.c.io, self.repo, self.remote, .{ .refspecs = names.items });
        var out: util.List(std.json.Value) = .init(self.c.alloc);
        for (outcome.artifacts) |*a| {
            var o = jsonio.new_object();
            jsonio.obj_put(&o, self.c.alloc, "name", .{ .string = a.name });
            jsonio.obj_put(&o, self.c.alloc, "path", .{ .string = a.path });
            jsonio.obj_put(&o, self.c.alloc, "sha256", .{ .string = a.sha256 });
            jsonio.obj_put(&o, self.c.alloc, "bytes", .{ .integer = @intCast(a.bytes) });
            try out.append(.{ .object = o });
        }
        return out.items;
    }

    fn run_view(self: *const Runner, id: []const u8) !?ci.RunView {
        gitx.fetch_collab(self.c.alloc, self.c.io, self.repo, self.remote) catch |err| {
            std.debug.print("wgl: fetch collab refs failed ({t}); reading local state\n", .{err});
        };
        const reader = collabreader.Reader.init(self.c.alloc, self.c.io, self.repo);
        const loaded = try reader.load(self.c.alloc);
        var group: util.List(EntryRef) = .init(self.c.alloc);
        for (loaded.entries) |e| {
            if (std.mem.eql(u8, e.entry.id, id)) try group.append(e);
        }
        return ci.run_view(self.c.alloc, id, group.items, loaded.principals, util.now_ts());
    }

    /// Sign an entry into the runner's inbox and push it (§5 write path).
    fn publish(self: *const Runner, kind: []const u8, id: []const u8, parent: []const u8, body: std.json.Value) ![]const u8 {
        var e = Entry{
            .version = 1,
            .kind = kind,
            .id = id,
            .actor = self.actor,
            .ts = util.now_ts(),
            .parent = parent,
            .refs = null,
            .body = body,
            .sig = "",
        };
        entry_mod.sign_entry(self.c.alloc, &e, self.key);
        const content = entry_mod.entry_json(self.c.alloc, &e, true);
        const oid = try gitx.write_blob(self.c.alloc, self.c.io, self.repo, content);
        const ref_name = try std.fmt.allocPrint(self.c.alloc, "refs/collab/inbox/{s}/{s}", .{ self.actor, identity.entry_uuid(self.c.alloc, self.c.io) });
        try gitx.update_ref(self.c.alloc, self.c.io, self.repo, ref_name, oid);
        try gitx.push(self.c.alloc, self.c.io, self.repo, self.remote, .{ .refspecs = &.{ref_name} });
        return oid;
    }

    fn publish_claim(self: *const Runner, id: []const u8, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, ttl: u64, attempt: u32) ![]const u8 {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, self.c.alloc, "task", .{ .string = task.name });
        jsonio.obj_put(&o, self.c.alloc, "ref", .{ .string = ref_name });
        jsonio.obj_put(&o, self.c.alloc, "commit", .{ .string = commit });
        jsonio.obj_put(&o, self.c.alloc, "ttl", .{ .integer = @intCast(ttl) });
        jsonio.obj_put(&o, self.c.alloc, "attempt", .{ .integer = attempt });
        jsonio.obj_put(&o, self.c.alloc, "runner", .{ .string = "wgl" });
        const oid = try self.publish(ci.CI_CLAIM_KIND, id, "", .{ .object = o });
        std.debug.print("wgl: run {s} task {s} attempt {d}: claimed (ttl {d}s)\n", .{ id, task.name, attempt, ttl });
        return oid;
    }

    fn publish_result(self: *const Runner, id: []const u8, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, attempt: u32, claim_oid: []const u8, outcome: *const ExecOutcome, artifacts: []std.json.Value) !void {
        var o = jsonio.new_object();
        jsonio.obj_put(&o, self.c.alloc, "task", .{ .string = task.name });
        jsonio.obj_put(&o, self.c.alloc, "ref", .{ .string = ref_name });
        jsonio.obj_put(&o, self.c.alloc, "commit", .{ .string = commit });
        jsonio.obj_put(&o, self.c.alloc, "attempt", .{ .integer = attempt });
        jsonio.obj_put(&o, self.c.alloc, "claim", .{ .string = claim_oid });
        jsonio.obj_put(&o, self.c.alloc, "conclusion", .{ .string = outcome.conclusion.as_str() });
        jsonio.obj_put(&o, self.c.alloc, "exit_code", if (outcome.exit_code) |code| std.json.Value{ .integer = code } else .null);
        jsonio.obj_put(&o, self.c.alloc, "duration_ms", .{ .integer = @intCast(outcome.duration_ms) });
        jsonio.obj_put(&o, self.c.alloc, "log_summary", .{ .string = tail_string(outcome.log, LOG_SUMMARY_MAX) });
        jsonio.obj_put(&o, self.c.alloc, "log_sha256", .{ .string = sha256_hex(self.c.alloc, outcome.log) });
        if (artifacts.len > 0) {
            o.put(self.c.alloc, "artifacts", .{ .array = blk: {
                var arr = std.json.Array.init(self.c.alloc);
                for (artifacts) |a| try arr.append(a);
                break :blk arr;
            } }) catch @panic("oom");
        }
        if (outcome.log_truncated) {
            jsonio.obj_put(&o, self.c.alloc, "log_truncated", .{ .bool = true });
        }
        const oid = try self.publish(ci.CI_RESULT_KIND, id, claim_oid, .{ .object = o });
        std.debug.print("wgl: run {s} task {s} attempt {d}: {s} (exit {s}, {d} ms) as {s}\n", .{
            id,
            task.name,
            attempt,
            outcome.conclusion.as_str(),
            if (outcome.exit_code) |code| std.fmt.allocPrint(self.c.alloc, "{d}", .{code}) catch @panic("oom") else "signal",
            outcome.duration_ms,
            oid,
        });
    }

    /// §8.1: a detached worktree of the tested commit, the command inside
    /// it, cleanup whatever happens. Infrastructure failures map to
    /// `Conclusion::Error` (§8.2): no task verdict, the claim it references
    /// voids, the run re-claimable at once (§6.3).
    fn execute(self: *const Runner, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, id: []const u8, attempt: u32) ExecOutcome {
        const started = millis();
        const fail = struct {
            fn f(ms: u64, msg: []const u8) ExecOutcome {
                return .{ .conclusion = .@"error", .exit_code = null, .duration_ms = ms, .log = msg, .log_truncated = false };
            }
        }.f;
        // A unique temp dir for the detached worktree.
        var rand: [8]u8 = undefined;
        self.c.io.random(&rand);
        const tmp_base = self.c.env_or("TMPDIR", "/tmp");
        const dir_path = std.fmt.allocPrint(self.c.alloc, "{s}/wgl-ci-{s}", .{ tmp_base, util.to_hex(self.c.alloc, &rand) }) catch return fail(millis() - started, "create temp dir: oom");
        {
            var d = std.Io.Dir.cwd();
            if (d.createDirPathOpen(self.c.io, dir_path, .{})) |_| {} else |_| {
                return fail(millis() - started, "create temp dir failed");
            }
        }
        const add_argv = [_][]const u8{ "worktree", "add", "--detach", dir_path, commit };
        const added = gitx.run(self.c.alloc, self.c.io, self.repo, &add_argv) catch return fail(millis() - started, "git worktree add: spawn failed");
        if (!added.ok()) {
            const msg = std.fmt.allocPrint(self.c.alloc, "git worktree add {s}: {s}", .{ commit, std.mem.trim(u8, added.stderr, " \t\r\n") }) catch "git worktree add failed";
            return fail(millis() - started, msg);
        }
        var outcome = self.run_command(dir_path, task, ref_name, commit, id, attempt);
        // §8.2: collect declared artifacts before the worktree goes away.
        if (outcome.conclusion != .@"error" and task.artifacts.len > 0) {
            outcome.artifacts = collect_artifacts(self.c, dir_path, task);
        }
        // Best-effort cleanup: a failed removal never blocks the result.
        {
            const rm = [_][]const u8{ "worktree", "remove", "--force", dir_path };
            _ = gitx.run(self.c.alloc, self.c.io, self.repo, &rm) catch {};
            const prune = [_][]const u8{"worktree"};
            _ = gitx.run(self.c.alloc, self.c.io, self.repo, &prune) catch {};
            var d = std.Io.Dir.cwd();
            d.deleteDir(self.c.io, dir_path) catch {};
        }
        return outcome;
    }

    /// §8.1: `sh -c` (POSIX) / `cmd /C` (Windows), cwd = worktree, cleared
    /// environment with the platform basics + `env_allow` + `WALGIT_CI_*`,
    /// capped capture of the merged output, hard timeout — the child runs
    /// in its own process group, so the timeout kills its whole tree.
    fn run_command(self: *const Runner, dir: []const u8, task: *const ciconfig.CiResolvedTask, ref_name: []const u8, commit: []const u8, id: []const u8, attempt: u32) ExecOutcome {
        const started = millis();
        const fail = struct {
            fn f(ms: u64, msg: []const u8) ExecOutcome {
                return .{ .conclusion = .@"error", .exit_code = null, .duration_ms = ms, .log = msg, .log_truncated = false };
            }
        }.f;
        const shell: []const []const u8 = if (@import("builtin").os.tag == .windows)
            &.{ "cmd", "/C", task.command }
        else
            &.{ "sh", "-c", task.command };
        // Cleared environment: platform basics + env_allow + WALGIT_CI_*.
        var env_map = std.process.Environ.Map.init(self.c.alloc);
        for (BASE_ENV_ALLOW) |name| {
            if (self.c.env.get(name)) |v| {
                env_map.put(name, v) catch {};
            }
        }
        for (task.env_allow) |name| {
            if (self.c.env.get(name)) |v| {
                env_map.put(name, v) catch {};
            }
        }
        env_map.put("WALGIT_CI_TASK", task.name) catch {};
        env_map.put("WALGIT_CI_REF", ref_name) catch {};
        env_map.put("WALGIT_CI_COMMIT", commit) catch {};
        env_map.put("WALGIT_CI_RUN_ID", id) catch {};
        env_map.put("WALGIT_CI_ATTEMPT", std.fmt.allocPrint(self.c.alloc, "{d}", .{attempt}) catch "1") catch {};
        env_map.put("WALGIT_CI_ACTOR", self.actor) catch {};

        var child = std.process.spawn(self.c.io, .{
            .argv = shell,
            .cwd = .{ .path = dir },
            .environ_map = &env_map,
            .stdin = .ignore,
            .stdout = .pipe,
            .stderr = .pipe,
            .pgid = 0, // own process group: the timeout kills the whole tree
        }) catch return fail(millis() - started, "spawn failed");

        // Drain both pipes into one capped tail while the child runs.
        const Drain = struct {
            fn go(r: *std.Io.File.Reader, buf: *Capture) void {
                var tmp: [8192]u8 = undefined;
                while (true) {
                    const n = r.interface.readSliceShort(&tmp) catch return;
                    if (n == 0) return;
                    buf.append(tmp[0..n]);
                }
            }
        };
        var capture = Capture.init(self.c.alloc);
        var rbuf1: [64 * 1024]u8 = undefined;
        var rbuf2: [64 * 1024]u8 = undefined;
        var r1 = child.stdout.?.reader(self.c.io, &rbuf1);
        var r2 = child.stderr.?.reader(self.c.io, &rbuf2);
        const drain1 = struct {
            fn go(r: *std.Io.File.Reader, cap: *Capture) void {
                Drain.go(r, cap);
            }
        }.go;
        _ = drain1;
        const t1 = std.Thread.spawn(.{}, Drain.go, .{ &r1, &capture }) catch {
            return fail(millis() - started, "thread spawn failed");
        };
        const t2 = std.Thread.spawn(.{}, Drain.go, .{ &r2, &capture }) catch {
            return fail(millis() - started, "thread spawn failed");
        };

        const deadline_ms = started + task.timeout_secs * 1000;
        while (true) {
            const term = child.wait(self.c.io) catch null;
            if (term) |t| {
                t1.join();
                t2.join();
                const code: ?i64 = switch (t) {
                    .exited => |code| code,
                    else => null,
                };
                const captured = capture.take();
                return .{
                    .conclusion = if (code != null and code.? == 0) .success else .failure,
                    .exit_code = code,
                    .duration_ms = millis() - started,
                    .log = captured.log,
                    .log_truncated = captured.truncated,
                };
            }
            if (millis() >= deadline_ms) break;
            self.c.io.sleep(std.Io.Duration.fromNanoseconds(25 * std.time.ns_per_ms), .real) catch {};
        }
        // Hard timeout: kill the whole process group.
        if (child.id) |pid| {
            if (@import("builtin").os.tag != .windows) {
                std.posix.kill(-@as(i32, @intCast(pid)), std.posix.SIG.KILL) catch {};
            }
        }
        child.kill(self.c.io);
        t1.join();
        t2.join();
        const captured = capture.take();
        return .{
            .conclusion = .timeout,
            .exit_code = null,
            .duration_ms = millis() - started,
            .log = captured.log,
            .log_truncated = captured.truncated,
        };
    }
};

/// One stream's capped tail. The two pipes drain into separate buffers and
/// the merged log is stdout ++ stderr — the retired Rust implementation
/// interleaved them through one ring, but the interleaving there was
/// nondeterministic anyway, so the deterministic order costs nothing.
const Capture = struct {
    alloc: std.mem.Allocator,
    bytes: util.List(u8),
    truncated: bool = false,
    done: bool = false,

    fn init(alloc: std.mem.Allocator) Capture {
        return .{ .alloc = alloc, .bytes = util.List(u8).init(alloc) };
    }

    fn take(self: *Capture) struct { log: []const u8, truncated: bool } {
        return .{ .log = self.bytes.items, .truncated = self.truncated };
    }

    fn append(self: *Capture, chunk: []const u8) void {
        if (chunk.len >= LOG_CAPTURE_MAX) {
            self.bytes.items.len = 0;
            self.bytes.append_slice(chunk[chunk.len - LOG_CAPTURE_MAX ..]) catch {};
            self.truncated = true;
            return;
        }
        if (self.bytes.items.len + chunk.len > LOG_CAPTURE_MAX) {
            const drop = self.bytes.items.len + chunk.len - LOG_CAPTURE_MAX;
            const remaining = self.bytes.items[drop..];
            std.mem.copyForwards(u8, self.bytes.items[0..remaining.len], remaining);
            self.bytes.items.len = remaining.len;
            self.truncated = true;
        }
        self.bytes.append_slice(chunk) catch {};
    }
};

pub const ScheduleEntry = struct { name: []const u8, expr: ?[]const u8 };

const RefOutcome = struct {
    terminal: bool,
    failed: bool,
    schedules: []ScheduleEntry,
};

fn schedule_key(alloc: std.mem.Allocator, ref_name: []const u8, task: []const u8) ![]u8 {
    return std.fmt.allocPrint(alloc, "{s}\u{1f}{s}", .{ ref_name, task });
}

fn apply_schedule_snapshot(state: *RunnerState, ref_name: []const u8, schedules: []ScheduleEntry, task_filter: ?[]const u8, preserve_cron: bool) void {
    if (task_filter) |task_name| {
        const key = std.fmt.allocPrint(std.heap.page_allocator, "{s}\u{1f}{s}", .{ ref_name, task_name }) catch return;
        defer std.heap.page_allocator.free(key);
        _ = state.schedules.remove(key);
        if (!preserve_cron) _ = state.cron.remove(key);
        for (schedules) |s| {
            if (std.mem.eql(u8, s.name, task_name)) {
                if (s.expr) |expr| {
                    const k = std.fmt.allocPrint(std.heap.page_allocator, "{s}\u{1f}{s}", .{ ref_name, task_name }) catch return;
                    state.schedules.put(k, expr) catch {};
                }
            }
        }
        return;
    }
    // Clear this ref's schedule rows, then repopulate from the snapshot.
    var doomed: util.List([]const u8) = .init(std.heap.page_allocator);
    defer doomed.deinit();
    var it = state.schedules.iterator();
    while (it.next()) |kv| {
        if (std.mem.startsWith(u8, kv.key_ptr.*, ref_name) and kv.key_ptr.*.len > ref_name.len and kv.key_ptr.*[ref_name.len] == 0x1f) {
            doomed.append(kv.key_ptr.*) catch {};
        }
    }
    for (doomed.items) |k| _ = state.schedules.remove(k);
    if (!preserve_cron) {
        var doomed2: util.List([]const u8) = .init(std.heap.page_allocator);
        defer doomed2.deinit();
        var it2 = state.cron.iterator();
        while (it2.next()) |kv| {
            if (std.mem.startsWith(u8, kv.key_ptr.*, ref_name) and kv.key_ptr.*.len > ref_name.len and kv.key_ptr.*[ref_name.len] == 0x1f) {
                doomed2.append(kv.key_ptr.*) catch {};
            }
        }
        for (doomed2.items) |k| _ = state.cron.remove(k);
    }
    for (schedules) |s| {
        const expr = s.expr orelse continue;
        const k = std.fmt.allocPrint(std.heap.page_allocator, "{s}\u{1f}{s}", .{ ref_name, s.name }) catch continue;
        state.schedules.put(k, expr) catch {};
    }
}

fn collect_artifacts(c: *Ctx, worktree: []const u8, task: *const ciconfig.CiResolvedTask) []CollectedArtifact {
    var out: util.List(CollectedArtifact) = .init(c.alloc);
    for (task.artifacts) |rel| {
        const path = std.fs.path.join(c.alloc, &.{ worktree, rel }) catch continue;
        var d = std.Io.Dir.cwd();
        const stat = d.statFile(c.io, path, .{}) catch {
            std.debug.print("wgl: artifact \"{s}\": not a regular file, skipped\n", .{rel});
            continue;
        };
        if (stat.kind != .file) {
            std.debug.print("wgl: artifact \"{s}\": not a regular file, skipped\n", .{rel});
            continue;
        }
        if (stat.size > ci.CI_ARTIFACT_MAX_BYTES) {
            std.debug.print("wgl: artifact \"{s}\": {d} bytes over the {d}-byte cap, skipped\n", .{ rel, stat.size, ci.CI_ARTIFACT_MAX_BYTES });
            continue;
        }
        const base = std.fs.path.basename(rel);
        if (base.len == 0 or base.len > 128) {
            std.debug.print("wgl: artifact \"{s}\": base name missing or over 128 bytes, skipped\n", .{rel});
            continue;
        }
        const hash_argv = [_][]const u8{ "hash-object", "-w", "--", rel };
        const hashed = gitx.run(c.alloc, c.io, worktree, &hash_argv) catch continue;
        if (!hashed.ok()) continue;
        const oid = std.mem.trim(u8, hashed.stdout, " \t\r\n");
        const f = d.openFile(c.io, path, .{}) catch continue;
        defer f.close(c.io);
        var h = std.crypto.hash.sha2.Sha256.init(.{});
        var buf: [64 * 1024]u8 = undefined;
        var rf = f.reader(c.io, &buf);
        while (true) {
            const n = rf.interface.readSliceShort(&buf) catch break;
            if (n == 0) break;
            h.update(buf[0..n]);
        }
        var digest: [32]u8 = undefined;
        h.final(&digest);
        out.append(.{
            .name = base,
            .path = rel,
            .sha256 = util.to_hex(c.alloc, &digest),
            .bytes = stat.size,
            .oid = c.alloc.dupe(u8, oid) catch continue,
        }) catch {};
    }
    return out.items;
}

/// The captured tail snapped forward to a UTF-8 char boundary (§8.2
/// `log_summary`).
fn tail_string(bytes: []const u8, max: usize) []const u8 {
    var start = bytes.len -| max;
    while (start < bytes.len and (bytes[start] & 0xC0) == 0x80) start += 1;
    return bytes[start..];
}

fn millis() u64 {
    // Monotonic deltas only (the §8.2 duration_ms); the base is irrelevant.
    const static = struct {
        var base: ?i96 = null;
    };
    const ts = std.Io.Timestamp.now(self_io(), .awake);
    if (static.base == null) static.base = ts.nanoseconds;
    return @intCast(@divTrunc(ts.nanoseconds - static.base.?, std.time.ns_per_ms));
}

fn self_io() std.Io {
    // The runner passes its Io into every call; millis is only used under
    // `execute`/`run_command`, which carry `*const Runner` — but the clock
    // is process-global, so the test binary synthesizes a Threaded driver.
    io_single.init();
    return io_single.io();
}

const io_single = struct {
    var threaded: ?std.Io.Threaded = null;
    fn init() void {
        if (threaded == null) {
            threaded = std.Io.Threaded.init(std.heap.page_allocator, .{});
        }
    }
    fn io() std.Io {
        return threaded.?.io();
    }
};

const RunnerState = struct {
    processed: std.StringHashMap([]const u8),
    cron: std.StringHashMap(i64),
    schedules: std.StringHashMap([]const u8),
    schedule_migration: std.StringHashMap(void),
    alloc: std.mem.Allocator,

    fn init(alloc: std.mem.Allocator) RunnerState {
        return .{
            .processed = std.StringHashMap([]const u8).init(alloc),
            .cron = std.StringHashMap(i64).init(alloc),
            .schedules = std.StringHashMap([]const u8).init(alloc),
            .schedule_migration = std.StringHashMap(void).init(alloc),
            .alloc = alloc,
        };
    }
};

fn read_runner_state(c: *Ctx, path: []const u8) RunnerState {
    var state = RunnerState.init(c.alloc);
    var d = std.Io.Dir.cwd();
    const raw = d.readFileAlloc(c.io, path, c.alloc, .limited(1 << 24)) catch return state;
    const v = std.json.parseFromSliceLeaky(std.json.Value, c.alloc, raw, .{}) catch return state;
    const o = switch (v) {
        .object => |o| o,
        else => return state,
    };
    if (o.get("processed")) |pv| {
        if (pv == .object) {
            var it = pv.object.iterator();
            while (it.next()) |kv| {
                if (kv.value_ptr.* == .string) {
                    state.processed.put(kv.key_ptr.*, kv.value_ptr.*.string) catch {};
                }
            }
        }
    }
    if (o.get("cron")) |cv| {
        if (cv == .object) {
            var it = cv.object.iterator();
            while (it.next()) |kv| {
                if (kv.value_ptr.* == .integer) {
                    state.cron.put(kv.key_ptr.*, kv.value_ptr.*.integer) catch {};
                }
            }
        }
    }
    const schedules_present = if (o.get("schedules")) |sv| sv == .object else false;
    if (o.get("schedules")) |sv| {
        if (sv == .object) {
            var it = sv.object.iterator();
            while (it.next()) |kv| {
                if (kv.value_ptr.* == .string) {
                    state.schedules.put(kv.key_ptr.*, kv.value_ptr.*.string) catch {};
                }
            }
        }
    }
    if (!schedules_present) {
        // One-shot migration for pre-`schedules` state files.
        var it = state.cron.iterator();
        while (it.next()) |kv| {
            if (std.mem.indexOfScalar(u8, kv.key_ptr.*, 0x1f)) |sep| {
                state.schedule_migration.put(kv.key_ptr.*[0..sep], {}) catch {};
            }
        }
    }
    return state;
}

fn write_runner_state(c: *Ctx, path: []const u8, state: *RunnerState) !void {
    var o = jsonio.new_object();
    {
        var po = jsonio.new_object();
        var it = state.processed.iterator();
        while (it.next()) |kv| {
            jsonio.obj_put(&po, c.alloc, kv.key_ptr.*, .{ .string = kv.value_ptr.* });
        }
        jsonio.obj_put(&o, c.alloc, "processed", .{ .object = po });
    }
    {
        var co = jsonio.new_object();
        var it = state.cron.iterator();
        while (it.next()) |kv| {
            jsonio.obj_put(&co, c.alloc, kv.key_ptr.*, .{ .integer = kv.value_ptr.* });
        }
        jsonio.obj_put(&o, c.alloc, "cron", .{ .object = co });
    }
    {
        var so = jsonio.new_object();
        var it = state.schedules.iterator();
        while (it.next()) |kv| {
            jsonio.obj_put(&so, c.alloc, kv.key_ptr.*, .{ .string = kv.value_ptr.* });
        }
        jsonio.obj_put(&o, c.alloc, "schedules", .{ .object = so });
    }
    const doc = jsonio.to_string_pretty(c.alloc, .{ .object = o });
    var d = std.Io.Dir.cwd();
    try d.writeFile(c.io, .{ .sub_path = path, .data = doc });
}

fn ci_run(c: *Ctx, p: *args.Parsed) !u8 {
    const repo = p.get("repo", ".");
    const remote = p.get("remote", "origin");
    const actor = try p.required("actor");
    try identity.ref_segment("ci.actor", actor);
    const key_path = try p.required("key");
    const key = identity.read_signing_key(c.alloc, c.io, key_path) catch |err| {
        std.debug.print("wgl: key {s}: {t}\n", .{ key_path, err });
        return error.BadKey;
    };
    const state_file = blk: {
        if (p.opt("state")) |s| break :blk c.alloc.dupe(u8, s) catch @panic("oom");
        const dir = try gitx.absolute_git_dir(c.alloc, c.io, repo);
        break :blk std.fs.path.join(c.alloc, &.{ dir, "ci-run.json" }) catch @panic("oom");
    };
    const runner = Runner{
        .c = c,
        .repo = repo,
        .remote = remote,
        .actor = actor,
        .key = key,
        .state_file = state_file,
        .task_filter = p.opt("task"),
    };
    try runner.register();
    // An actor already bound to another key would make every entry this
    // runner writes unverifiable: refuse that pairing up front.
    try collabreader.ensure_actor_key_binding(c.alloc, c.io, repo, actor, key_path, key);
    const interval = p.u64_of("interval", 15) catch 15;
    const ttl_override: ?u64 = if (p.opt("claim-ttl")) |t| std.fmt.parseInt(u64, t, 10) catch null else null;
    std.debug.print("wgl: ci: watching {s} via {s} as {s} (interval {d}s, state {s})\n", .{ repo, remote, actor, interval, state_file });
    if (p.boolean("once")) {
        const code = try runner.run_pass(ttl_override);
        if (code != 0) return error.TaskFailed;
        return 0;
    }
    // Pull only: every pass re-reads the remote's tips, so the poll
    // interval is the worst-case notification latency.
    while (true) {
        _ = runner.run_pass(ttl_override) catch |err| {
            std.debug.print("wgl: ci pass failed: {t}\n", .{err});
        };
        c.io.sleep(std.Io.Duration.fromSeconds(@intCast(interval)), .real) catch {};
    }
}
