//! The decentralized CI protocol (`docs/D1_CI_PROTOCOL.md`): the
//! deterministic read side over signed `ci_claim` / `ci_result` collab
//! entries. Pure functions of `(entries, principals, now)`, like `collab.zig`.
//! Ported from `walgit-wal/src/ci.rs`.

const std = @import("std");
const entry_mod = @import("entry.zig");
const util = @import("../util.zig");

pub const EntryRef = entry_mod.EntryRef;
pub const Principals = entry_mod.Principals;
pub const Value = std.json.Value;

// ---- protocol constants (docs/D1_CI_PROTOCOL.md §6/§8, normative) --------------

pub const CI_CLAIM_KIND = "ci_claim";
pub const CI_RESULT_KIND = "ci_result";
/// §6.1: claim TTL bounds, seconds.
pub const CLAIM_TTL_MIN_SECS: u64 = 1;
pub const CLAIM_TTL_MAX_SECS: u64 = 86_400;
/// §7.4: attempts are bounded like ci.toml's `max_attempts` (V7).
pub const ATTEMPT_MAX: u32 = 10;
/// §11: an oversized entry body is **malformed** (visible red, never a
/// convergence input).
pub const CI_BODY_MAX_BYTES: usize = 256 * 1024;
/// §8.2 storage convention: `refs/collab/ci-artifacts/<actor>/<sha256>`.
pub const CI_ARTIFACT_REF_PREFIX = "refs/collab/ci-artifacts/";
/// §8.2: at most this many artifacts per result.
pub const CI_ARTIFACTS_PER_RESULT_MAX: usize = 32;
/// §8.2 storage convention: one artifact/log object's byte cap.
pub const CI_ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// 64-bit FNV-1a — the §5 run-id digest.
fn fnv1a64(bytes: []const u8) u64 {
    var h: u64 = 0xcbf2_9ce4_8422_2325;
    for (bytes) |b| {
        h ^= @as(u64, b);
        h = h *% 0x0000_0100_0000_01b3;
    }
    return h;
}

fn run_id_bytes_buf(alloc: std.mem.Allocator, task: []const u8, repo_ref: []const u8, commit: []const u8) ![]u8 {
    return std.mem.concat(alloc, u8, &.{ task, "\x1f", repo_ref, "\x1f", commit });
}

/// §5: `run_id = "ci-" + hex16(fnv1a64(task || 0x1f || ref || 0x1f || commit))`.
pub fn run_id(alloc: std.mem.Allocator, task: []const u8, repo_ref: []const u8, commit: []const u8) ![]u8 {
    const buf = try run_id_bytes_buf(alloc, task, repo_ref, commit);
    defer alloc.free(buf);
    return std.fmt.allocPrint(alloc, "ci-{x:0>16}", .{fnv1a64(buf)});
}

/// §5 scheduled variant (§4.3): a cron-triggered run is a **new** run — the
/// slot's fire epoch keeps the identity deterministic.
pub fn scheduled_run_id(alloc: std.mem.Allocator, task: []const u8, repo_ref: []const u8, commit: []const u8, slot_epoch: i64) ![]u8 {
    var buf = util.List(u8).init(alloc);
    defer buf.deinit();
    try buf.append_slice(task);
    try buf.append(0x1f);
    try buf.append_slice(repo_ref);
    try buf.append(0x1f);
    try buf.append_slice(commit);
    try buf.append(0x1f);
    const slot_text = std.fmt.allocPrint(alloc, "{d}", .{slot_epoch}) catch return error.OutOfMemory;
    try buf.append_slice(slot_text);
    return std.fmt.allocPrint(alloc, "ci-{x:0>16}", .{fnv1a64(buf.items)});
}

// ---- entry bodies (§6.1 claim / §8.2 result) ------------------------------------

/// The task's verdict (§8.2). `error` is a runner-side failure: it publishes
/// no task conclusion and voids the claim it references (§6.3).
pub const Conclusion = enum {
    success,
    failure,
    timeout,
    @"error",

    pub fn as_str(self: Conclusion) []const u8 {
        return switch (self) {
            .success => "success",
            .failure => "failure",
            .timeout => "timeout",
            .@"error" => "error",
        };
    }

    pub fn parse(s: []const u8) ?Conclusion {
        if (std.mem.eql(u8, s, "success")) return .success;
        if (std.mem.eql(u8, s, "failure")) return .failure;
        if (std.mem.eql(u8, s, "timeout")) return .timeout;
        if (std.mem.eql(u8, s, "error")) return .@"error";
        return null;
    }
};

pub const CiClaim = struct {
    oid: []const u8,
    actor: []const u8,
    ts: i64,
    task: []const u8,
    repo_ref: []const u8,
    commit: []const u8,
    ttl_secs: u64,
    attempt: u32,

    pub fn live(self: *const CiClaim, now: i64) bool {
        return now <= std.math.add(i64, self.ts, @intCast(self.ttl_secs)) catch std.math.maxInt(i64);
    }
};

pub const CiArtifact = struct {
    name: []const u8,
    path: []const u8,
    sha256: []const u8,
    bytes: u64,
    url: ?[]const u8,
};

pub const CiResult = struct {
    oid: []const u8,
    actor: []const u8,
    ts: i64,
    task: []const u8,
    repo_ref: []const u8,
    commit: []const u8,
    attempt: u32,
    claim: []const u8,
    conclusion: Conclusion,
    exit_code: ?i64,
    duration_ms: ?u64,
    log_summary: []const u8,
    log_sha256: []const u8,
    log_truncated: bool,
    artifacts: []CiArtifact,
};

fn body_str(body: Value, key: []const u8) ?[]const u8 {
    switch (body) {
        .object => |o| {
            const v = o.get(key) orelse return null;
            return switch (v) {
                .string => |s| s,
                else => null,
            };
        },
        else => return null,
    }
}

fn body_str_nonempty(body: Value, key: []const u8) ?[]const u8 {
    const s = body_str(body, key) orelse return null;
    if (s.len == 0) return null;
    return s;
}

fn body_u32(body: Value, key: []const u8, min: u32, max: u32) ?u32 {
    switch (body) {
        .object => |o| {
            const v = o.get(key) orelse return null;
            return switch (v) {
                .integer => |i| blk: {
                    if (i < 0 or i > std.math.maxInt(u32)) return null;
                    const n: u32 = @intCast(i);
                    if (n < min or n > max) return null;
                    break :blk n;
                },
                else => null,
            };
        },
        else => return null,
    }
}

/// §11 read side: is the body's compact JSON serialization within
/// `CI_BODY_MAX_BYTES`? A bounded walk, bailing the moment the budget is
/// blown; the count matches serde_json's compact form exactly.
fn body_within_cap(body: Value) bool {
    var remaining: isize = @intCast(CI_BODY_MAX_BYTES);
    return json_fits(body, &remaining);
}

fn json_fits(v: Value, remaining: *isize) bool {
    const cost: isize = switch (v) {
        .null => 4,
        .bool => |b| if (b) 4 else 5,
        .integer => |i| blk: {
            var buf: [32]u8 = undefined;
            break :blk @intCast((std.fmt.bufPrint(&buf, "{d}", .{i}) catch unreachable).len);
        },
        .float => |f| blk: {
            var buf: [64]u8 = undefined;
            break :blk @intCast((std.fmt.bufPrint(&buf, "{d}", .{f}) catch unreachable).len);
        },
        .number_string => |s| @intCast(s.len),
        .string => |s| blk: {
            var n: usize = 2;
            for (s) |c| {
                n += switch (c) {
                    '"', '\\', 0x08, 0x09, 0x0A, 0x0C, 0x0D => 2,
                    else => if (c < 0x20) 6 else std.unicode.utf8ByteSequenceLength(c) catch 1,
                };
            }
            break :blk @intCast(n);
        },
        .array => |a| @intCast(2 + a.items.len -| 1),
        .object => |o| @intCast(2 + o.count() -| 1 + o.count()),
    };
    remaining.* -= cost;
    if (remaining.* < 0) return false;
    switch (v) {
        .array => |a| {
            for (a.items) |x| {
                if (!json_fits(x, remaining)) return false;
            }
        },
        .object => |o| {
            var it = o.iterator();
            while (it.next()) |e| {
                if (!json_fits(e.value_ptr.*, remaining)) return false;
                // Each key serializes as a JSON string.
                var key_remaining: isize = remaining.*;
                var n: usize = 2;
                for (e.key_ptr.*) |c| {
                    n += switch (c) {
                        '"', '\\', 0x08, 0x09, 0x0A, 0x0C, 0x0D => 2,
                        else => if (c < 0x20) 6 else std.unicode.utf8ByteSequenceLength(c) catch 1,
                    };
                }
                key_remaining -= @intCast(n);
                if (key_remaining < 0) return false;
                remaining.* = key_remaining;
            }
        },
        else => {},
    }
    return true;
}

fn valid_sha256(s: []const u8) bool {
    if (s.len != 64) return false;
    for (s) |c| {
        if (!std.ascii.isHex(c)) return false;
    }
    return true;
}

/// Parse a `ci_claim` body (§6.1). Strict: wrong/missing fields → `null`.
pub fn parse_claim(r: *const EntryRef) ?CiClaim {
    if (!std.mem.eql(u8, r.entry.kind, CI_CLAIM_KIND)) return null;
    const body = r.entry.body;
    if (!body_within_cap(body)) return null; // §11: oversized body = malformed
    const ttl: u32 = body_u32(body, "ttl", @intCast(CLAIM_TTL_MIN_SECS), @intCast(CLAIM_TTL_MAX_SECS)) orelse return null;
    return .{
        .oid = r.oid,
        .actor = r.entry.actor,
        .ts = r.entry.ts,
        .task = body_str_nonempty(body, "task") orelse return null,
        .repo_ref = body_str_nonempty(body, "ref") orelse return null,
        .commit = body_str_nonempty(body, "commit") orelse return null,
        .ttl_secs = ttl,
        .attempt = body_u32(body, "attempt", 1, ATTEMPT_MAX) orelse return null,
    };
}

/// §8.2: `artifacts` is optional, but present-and-not-an-array makes the
/// whole entry malformed. Items are shape-checked one by one — a bad item
/// drops, it does not condemn the entry; the list is truncated to the cap.
fn parse_artifacts(alloc: std.mem.Allocator, body: Value) ?[]CiArtifact {
    switch (body) {
        .object => |o| {
            const val = o.get("artifacts") orelse return alloc.alloc(CiArtifact, 0) catch null;
            const arr = switch (val) {
                .array => |a| a,
                else => return null,
            };
            var out = util.List(CiArtifact).init(alloc);
            const take = @min(arr.items.len, CI_ARTIFACTS_PER_RESULT_MAX);
            for (arr.items[0..take]) |item| {
                const obj = switch (item) {
                    .object => |ob| ob,
                    else => continue,
                };
                const name = str_field(obj, "name") orelse continue;
                const path = str_field(obj, "path") orelse continue;
                const sha256 = str_field(obj, "sha256") orelse continue;
                const bytes_v = obj.get("bytes") orelse continue;
                const bytes: u64 = switch (bytes_v) {
                    .integer => |i| if (i >= 0) @intCast(i) else continue,
                    else => continue,
                };
                if (name.len == 0 or name.len > 128 or path.len == 0) continue;
                if (!valid_sha256(sha256)) continue;
                const url: ?[]const u8 = if (obj.get("url")) |uv| switch (uv) {
                    .string => |s| s,
                    else => null,
                } else null;
                out.append(.{
                    .name = name,
                    .path = path,
                    .sha256 = sha256,
                    .bytes = bytes,
                    .url = url,
                }) catch return null;
            }
            return out.to_owned_slice() catch null;
        },
        else => return null,
    }
}

fn str_field(o: std.json.ObjectMap, key: []const u8) ?[]const u8 {
    const v = o.get(key) orelse return null;
    return switch (v) {
        .string => |s| s,
        else => null,
    };
}

/// Parse a `ci_result` body (§8.2).
pub fn parse_result(alloc: std.mem.Allocator, r: *const EntryRef) ?CiResult {
    if (!std.mem.eql(u8, r.entry.kind, CI_RESULT_KIND)) return null;
    const body = r.entry.body;
    if (!body_within_cap(body)) return null;
    const conclusion = Conclusion.parse(body_str_nonempty(body, "conclusion") orelse return null) orelse return null;
    const artifacts = parse_artifacts(alloc, body) orelse return null;
    switch (body) {
        .object => |o| {
            return .{
                .oid = r.oid,
                .actor = r.entry.actor,
                .ts = r.entry.ts,
                .task = body_str_nonempty(body, "task") orelse return null,
                .repo_ref = body_str_nonempty(body, "ref") orelse return null,
                .commit = body_str_nonempty(body, "commit") orelse return null,
                .attempt = body_u32(body, "attempt", 1, ATTEMPT_MAX) orelse return null,
                .claim = body_str_nonempty(body, "claim") orelse return null,
                .conclusion = conclusion,
                .exit_code = if (o.get("exit_code")) |v| switch (v) {
                    .integer => |i| i,
                    else => null,
                } else null,
                .duration_ms = if (o.get("duration_ms")) |v| switch (v) {
                    .integer => |i| if (i >= 0) @as(u64, @intCast(i)) else null,
                    else => null,
                } else null,
                .log_summary = body_str(body, "log_summary") orelse "",
                .log_sha256 = body_str(body, "log_sha256") orelse "",
                .log_truncated = if (o.get("log_truncated")) |v| v == .bool and v.bool else false,
                .artifacts = artifacts,
            };
        },
        else => return null,
    }
}

// ---- §7 aggregation: one run's view ----------------------------------------------

pub const AttemptView = struct {
    attempt: u32,
    claims: []const CiClaim,
    /// min by `(ts, actor, oid)` over live claims — the runner that may execute.
    winner: ?CiClaim,
    results: []const CiResult,
    /// The one result that counts (§7.2).
    effective: ?CiResult,
    state: RunState,
};

pub const RunState = enum {
    /// No claims at all — anyone may claim.
    pending,
    /// A live winner holds the run.
    claimed,
    /// Claims exist but none is live (TTL passed, or voided by `error`).
    stale,
    /// An effective result exists.
    done,

    pub fn as_str(self: RunState) []const u8 {
        return @tagName(self);
    }
};

fn claim_min(a: CiClaim, b: CiClaim) CiClaim {
    if (less3(a.ts, a.actor, a.oid, b.ts, b.actor, b.oid)) return a;
    return b;
}

fn result_min(a: CiResult, b: CiResult) CiResult {
    if (less3(a.ts, a.actor, a.oid, b.ts, b.actor, b.oid)) return a;
    return b;
}

fn less3(ts_a: i64, actor_a: []const u8, oid_a: []const u8, ts_b: i64, actor_b: []const u8, oid_b: []const u8) bool {
    if (ts_a != ts_b) return ts_a < ts_b;
    const by_actor = std.mem.order(u8, actor_a, actor_b);
    if (by_actor != .eq) return by_actor == .lt;
    return std.mem.order(u8, oid_a, oid_b) == .lt;
}

/// §7.2, normative. `error` results void the claim they reference; the winner
/// is the earliest live claim; the effective result is the one referencing
/// the winner, falling back to the earliest result so a settled run never
/// reverts.
fn attempt_view(attempt: u32, claims: []const CiClaim, results: []const CiResult, now: i64) AttemptView {
    var winner: ?CiClaim = null;
    for (claims) |c| {
        if (!c.live(now)) continue;
        var voided = false;
        for (results) |r| {
            if (r.conclusion == .@"error" and std.mem.eql(u8, r.claim, c.oid)) {
                voided = true;
                break;
            }
        }
        if (voided) continue;
        winner = if (winner) |w| claim_min(w, c) else c;
    }
    var effective: ?CiResult = null;
    for (results) |r| {
        // Only results answering the winner count while one holds the run;
        // without a winner every result keeps the settled-run fallback.
        if (winner) |w| {
            if (!std.mem.eql(u8, r.claim, w.oid)) continue;
        }
        effective = if (effective) |e| result_min(e, r) else r;
    }
    const settled = effective != null and effective.?.conclusion != .@"error";
    const state: RunState = if (settled)
        .done
    else if (winner != null)
        .claimed
    else if (claims.len == 0)
        .pending
    else
        .stale;
    return .{
        .attempt = attempt,
        .claims = claims,
        .winner = winner,
        .results = results,
        .effective = effective,
        .state = state,
    };
}

pub const RunView = struct {
    id: []const u8,
    task: []const u8,
    repo_ref: []const u8,
    commit: []const u8,
    /// Sorted by attempt number.
    attempts: []AttemptView,
    latest_attempt: u32,
    state: RunState,
    conclusion: ?Conclusion,
    runner: ?[]const u8,
    last_ts: i64,
    /// Entries for this run that failed verification or parsing — visible
    /// red, never part of convergence (§7.1).
    unverified: usize,
};

/// The view of one run over the entries carrying its `id` (§7.1): only
/// verified entries drive the state; the rest count in `unverified`.
pub fn run_view(alloc: std.mem.Allocator, id: []const u8, entries: []const EntryRef, principals: *const Principals, now: i64) !?RunView {
    const Attempt = struct {
        n: u32,
        claims: util.List(CiClaim),
        results: util.List(CiResult),
    };
    // The attempt lists' buffers are arena allocations the returned views
    // borrow — only the outer slice is freed here. Freeing the inner lists
    // would rewind the arena and turn the claims/results the caller still
    // reads into garbage (the Rust original moved ownership instead).
    var attempts: util.List(Attempt) = util.List(Attempt).init(alloc);
    defer attempts.deinit();
    var unverified: usize = 0;
    var last_ts: i64 = 0;
    var seen = false;
    for (entries) |*r| {
        if (!std.mem.eql(u8, r.entry.id, id)) continue;
        seen = true;
        last_ts = @max(last_ts, r.entry.ts);
        const verified = r.is_verified(alloc, principals);
        var claim: ?CiClaim = null;
        var result: ?CiResult = null;
        if (verified) {
            claim = parse_claim(r);
            result = parse_result(alloc, r);
        }
        if (claim == null and result == null) {
            // Unverified, or a verified entry that does not parse: a visible
            // red, not a state input.
            const k = r.entry.kind;
            if (std.mem.eql(u8, k, CI_CLAIM_KIND) or std.mem.eql(u8, k, CI_RESULT_KIND)) {
                unverified += 1;
            }
            continue;
        }
        const attempt_n = if (claim) |c| c.attempt else if (result) |x| x.attempt else 1;
        var slot: ?*Attempt = null;
        for (attempts.items) |*a| {
            if (a.n == attempt_n) {
                slot = a;
                break;
            }
        }
        if (slot == null) {
            try attempts.append(.{
                .n = attempt_n,
                .claims = util.List(CiClaim).init(alloc),
                .results = util.List(CiResult).init(alloc),
            });
            slot = &attempts.items[attempts.items.len - 1];
        }
        if (claim) |c| try slot.?.claims.append(c);
        if (result) |x| try slot.?.results.append(x);
    }
    if (!seen) return null;

    std.mem.sort(Attempt, attempts.items, {}, struct {
        fn f(_: void, a: Attempt, b: Attempt) bool {
            return a.n < b.n;
        }
    }.f);

    var views = util.List(AttemptView).init(alloc);
    for (attempts.items) |*a| {
        try views.append(attempt_view(a.n, a.claims.items, a.results.items, now));
    }

    // Header fields come from the latest attempt only, deterministically:
    // effective result, else the winner claim, else the first parsed claim.
    var task: []const u8 = "";
    var repo_ref: []const u8 = "";
    var commit: []const u8 = "";
    var state: RunState = .pending;
    var conclusion: ?Conclusion = null;
    var runner: ?[]const u8 = null;
    if (views.items.len > 0) {
        const a = views.items[views.items.len - 1];
        state = a.state;
        if (a.effective) |e| {
            conclusion = e.conclusion;
            task = e.task;
            repo_ref = e.repo_ref;
            commit = e.commit;
        }
        if (a.winner) |w| {
            runner = w.actor;
            if (task.len == 0) {
                task = w.task;
                repo_ref = w.repo_ref;
                commit = w.commit;
            }
        }
        if (a.claims.len > 0 and task.len == 0) {
            task = a.claims[0].task;
            repo_ref = a.claims[0].repo_ref;
            commit = a.claims[0].commit;
        }
    }

    return .{
        .id = id,
        .task = task,
        .repo_ref = repo_ref,
        .commit = commit,
        .latest_attempt = if (views.items.len > 0) views.items[views.items.len - 1].attempt else 1,
        .attempts = try views.to_owned_slice(),
        .state = state,
        .conclusion = conclusion,
        .runner = runner,
        .last_ts = last_ts,
        .unverified = unverified,
    };
}

/// The CI-relevant slice of a log: every `ci_claim`/`ci_result` entry,
/// verified or not — the reds stay visible, everything else is not this
/// protocol's business.
pub fn ci_entries(alloc: std.mem.Allocator, entries: []const EntryRef) ![]EntryRef {
    var out = util.List(EntryRef).init(alloc);
    for (entries) |e| {
        if (std.mem.eql(u8, e.entry.kind, CI_CLAIM_KIND) or std.mem.eql(u8, e.entry.kind, CI_RESULT_KIND)) {
            try out.append(e);
        }
    }
    return try out.to_owned_slice();
}

pub const RunEntry = struct {
    id: []const u8,
    view: RunView,
};

/// Every run in the log, by id ascending (the BTreeMap order), as of `now`
/// (§7.1). The one aggregation behind `wgl ci status`, the collab report's CI
/// section and the SPA's thread display.
pub fn collect_runs(alloc: std.mem.Allocator, entries: []const EntryRef, principals: *const Principals, now: i64) ![]RunEntry {
    // distinct ids, sorted
    var ids = util.List([]const u8).init(alloc);
    defer ids.deinit();
    var seen = std.StringHashMap(void).init(alloc);
    defer seen.deinit();
    for (entries) |e| {
        if (!seen.contains(e.entry.id)) {
            try seen.put(e.entry.id, {});
            try ids.append(e.entry.id);
        }
    }
    std.mem.sort([]const u8, ids.items, {}, struct {
        fn f(_: void, a: []const u8, b: []const u8) bool {
            return std.mem.order(u8, a, b) == .lt;
        }
    }.f);
    var out = util.List(RunEntry).init(alloc);
    for (ids.items) |id| {
        var group = util.List(EntryRef).init(alloc);
        defer group.deinit();
        for (entries) |e| {
            if (std.mem.eql(u8, e.entry.id, id)) try group.append(e);
        }
        if (try run_view(alloc, id, group.items, principals, now)) |view| {
            try out.append(.{ .id = id, .view = view });
        }
    }
    return try out.to_owned_slice();
}

// ---- §7.3 decide: the runner's normative decision --------------------------------

pub const Decision = union(enum) {
    /// The run has an effective result on its latest attempt; nothing to do.
    settled: struct { conclusion: Conclusion, attempt: u32 },
    /// Publish a claim for this attempt.
    claim: struct { attempt: u32 },
    /// Our own unexpired claim already wins — execute now (crash recovery).
    @"resume": struct { claim_oid: []const u8, attempt: u32 },
    /// Another runner holds a live winning claim.
    stand_down: struct { holder: []const u8 },
};

/// §7.3, normative. `max_attempts` is the task's retry bound: a failed (or
/// timed-out) latest attempt with budget left becomes the next attempt's
/// claim; `error` conclusions void their claim, so the same attempt is
/// re-claimable at once (§6.3).
pub fn decide(run: *const RunView, actor: []const u8, max_attempts: u32) Decision {
    if (run.attempts.len == 0) return .{ .claim = .{ .attempt = 1 } };
    const latest = run.attempts[run.attempts.len - 1];
    switch (latest.state) {
        .done => {
            if (latest.effective) |eff| {
                const budget = @max(@min(max_attempts, ATTEMPT_MAX), 1);
                const retriable = (eff.conclusion == .failure or eff.conclusion == .timeout) and eff.attempt < budget;
                if (retriable) {
                    return .{ .claim = .{ .attempt = eff.attempt + 1 } };
                }
                return .{ .settled = .{ .conclusion = eff.conclusion, .attempt = eff.attempt } };
            }
            return .{ .claim = .{ .attempt = latest.attempt } };
        },
        .claimed => {
            if (latest.winner) |w| {
                if (std.mem.eql(u8, w.actor, actor)) {
                    return .{ .@"resume" = .{ .claim_oid = w.oid, .attempt = latest.attempt } };
                }
                return .{ .stand_down = .{ .holder = w.actor } };
            }
            return .{ .claim = .{ .attempt = 1 } };
        },
        .pending => return .{ .claim = .{ .attempt = 1 } },
        .stale => return .{ .claim = .{ .attempt = latest.attempt } },
    }
}

// ---- tests -----------------------------------------------------------------------

const testing = std.testing;

fn claim_body(alloc: std.mem.Allocator, task: []const u8, ref: []const u8, commit: []const u8, ttl: i64, attempt: i64) Value {
    const jsonio = @import("../jsonio.zig");
    var o = jsonio.new_object();
    jsonio.obj_put(&o, alloc, "task", .{ .string = task });
    jsonio.obj_put(&o, alloc, "ref", .{ .string = ref });
    jsonio.obj_put(&o, alloc, "commit", .{ .string = commit });
    jsonio.obj_put(&o, alloc, "ttl", .{ .integer = ttl });
    jsonio.obj_put(&o, alloc, "attempt", .{ .integer = attempt });
    return .{ .object = o };
}

fn oid_of(seed: []const u8) []const u8 {
    return seed;
}

test "run id is deterministic and discriminating" {
    const alloc = testing.allocator;
    const a = try run_id(alloc, "test", "refs/heads/main", "abc");
    defer alloc.free(a);
    const b = try run_id(alloc, "test", "refs/heads/main", "abd");
    defer alloc.free(b);
    const c = try run_id(alloc, "test", "refs/heads/main", "abc");
    defer alloc.free(c);
    try testing.expectEqualStrings(a, c);
    try testing.expect(!std.mem.eql(u8, a, b));
    const d = try run_id(alloc, "test", "refs/heads/mainx", "abc");
    defer alloc.free(d);
    try testing.expect(!std.mem.eql(u8, a, d));
    // FNV-1a known answer: fnv1a64("") = 0xcbf29ce484222325.
    try testing.expectEqual(@as(u64, 0xcbf2_9ce4_8422_2325), fnv1a64(""));
}

const NOW: i64 = 1_000;

test "attempt state machine: pending, claimed, stale, done" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();

    var principals = Principals.init(testing.allocator);
    defer principals.deinit();

    // No entries at all: no run (seen=false → None, like the Rust side).
    const ev0 = [_]EntryRef{};
    const v0 = try run_view(alloc, "r", &ev0, &principals, NOW);
    try testing.expect(v0 == null);

    // Build a fake claim/result pair directly through attempt_view.
    const claim = CiClaim{
        .oid = "c1",
        .actor = "ci-runner-a",
        .ts = NOW - 10,
        .task = "t",
        .repo_ref = "refs/heads/main",
        .commit = "abc",
        .ttl_secs = 300,
        .attempt = 1,
    };
    const claims1 = [_]CiClaim{claim};
    const av1 = attempt_view(1, &claims1, &.{}, NOW);
    try testing.expectEqual(RunState.claimed, av1.state);
    const av2 = attempt_view(1, &.{claim}, &.{}, NOW + 301);
    try testing.expectEqual(RunState.stale, av2.state);

    const result = CiResult{
        .oid = "r1",
        .actor = "ci-runner-a",
        .ts = NOW - 5,
        .task = "t",
        .repo_ref = "refs/heads/main",
        .commit = "abc",
        .attempt = 1,
        .claim = "c1",
        .conclusion = .success,
        .exit_code = 0,
        .duration_ms = 5,
        .log_summary = "ok",
        .log_sha256 = "",
        .log_truncated = false,
        .artifacts = &.{},
    };
    const av3 = attempt_view(1, &.{claim}, &.{result}, NOW);
    try testing.expectEqual(RunState.done, av3.state);
    try testing.expectEqual(Conclusion.success, av3.effective.?.conclusion);

    // An error result voids its claim: stale, re-claimable at once.
    var err_result = result;
    err_result.conclusion = .@"error";
    const av4 = attempt_view(1, &.{claim}, &.{err_result}, NOW + 301);
    try testing.expectEqual(RunState.stale, av4.state);
    _ = claim_body;
    _ = oid_of;
}

test "decide: settled, retriable, resume, stand down" {
    const mk = struct {
        fn run(state: RunState, conclusion: ?Conclusion, attempt: u32, winner_actor: ?[]const u8) RunView {
            var views: []AttemptView = &.{};
            if (state != .pending) {
                const views_buf = std.heap.page_allocator.alloc(AttemptView, 1) catch unreachable;
                views_buf[0] = .{
                    .attempt = attempt,
                    .claims = &.{},
                    .winner = if (winner_actor) |a| .{
                        .oid = "w",
                        .actor = a,
                        .ts = NOW,
                        .task = "t",
                        .repo_ref = "r",
                        .commit = "c",
                        .ttl_secs = 300,
                        .attempt = attempt,
                    } else null,
                    .results = &.{},
                    .effective = if (conclusion) |cc| .{
                        .oid = "e",
                        .actor = "x",
                        .ts = NOW,
                        .task = "t",
                        .repo_ref = "r",
                        .commit = "c",
                        .attempt = attempt,
                        .claim = "w",
                        .conclusion = cc,
                        .exit_code = 0,
                        .duration_ms = 1,
                        .log_summary = "",
                        .log_sha256 = "",
                        .log_truncated = false,
                        .artifacts = &.{},
                    } else null,
                    .state = state,
                };
                views = views_buf;
            }
            return .{
                .id = "r",
                .task = "t",
                .repo_ref = "r",
                .commit = "c",
                .attempts = views,
                .latest_attempt = attempt,
                .state = state,
                .conclusion = conclusion,
                .runner = winner_actor,
                .last_ts = NOW,
                .unverified = 0,
            };
        }
    };
    // settled, non-retriable
    const d1 = decide(&mk.run(.done, .success, 1, null), "ci-a", 3);
    try testing.expect(d1 == .settled);
    // failure with budget left → next attempt
    const d2 = decide(&mk.run(.done, .failure, 1, null), "ci-a", 3);
    try testing.expectEqual(@as(u32, 2), d2.claim.attempt);
    // failure without budget → settled
    const d3 = decide(&mk.run(.done, .timeout, 3, null), "ci-a", 3);
    try testing.expect(d3 == .settled);
    // claimed by another → stand down
    const d4 = decide(&mk.run(.claimed, null, 1, "ci-b"), "ci-a", 3);
    try testing.expect(d4 == .stand_down);
    // claimed by us → resume
    const d5 = decide(&mk.run(.claimed, null, 1, "ci-a"), "ci-a", 3);
    try testing.expect(d5 == .@"resume");
    // pending → claim attempt 1
    const d6 = decide(&mk.run(.pending, null, 0, null), "ci-a", 3);
    try testing.expectEqual(@as(u32, 1), d6.claim.attempt);
    // stale → re-claim same attempt
    const d7 = decide(&mk.run(.stale, null, 2, null), "ci-a", 3);
    try testing.expectEqual(@as(u32, 2), d7.claim.attempt);
}

test "body cap: a huge string body is malformed, a normal one is not" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const jsonio = @import("../jsonio.zig");

    var o = jsonio.new_object();
    const big = try arena.alloc(u8, 300 * 1024);
    @memset(big, 'x');
    jsonio.obj_put(&o, arena, "task", .{ .string = big });
    try testing.expect(!body_within_cap(.{ .object = o }));

    var small = jsonio.new_object();
    jsonio.obj_put(&small, arena, "task", .{ .string = "test" });
    try testing.expect(body_within_cap(.{ .object = small }));
}
