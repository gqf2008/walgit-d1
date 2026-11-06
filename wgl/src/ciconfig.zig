//! `.walgit/ci.toml` schema (D1-CI §3.1, V1–V11): parse + validate with
//! pipeline defaults applied — what the runner executes. Ported from
//! `ci_cmd.rs`; every violation is collected, never fail-silent.

const std = @import("std");
const toml = @import("toml.zig");
const util = @import("util.zig");
const ci = @import("d1/ci.zig");

// ---- schema limits (docs/D1_CI_PROTOCOL.md §3.1, normative bounds) -------------

/// V9: the file itself.
pub const FILE_MAX_BYTES: usize = 64 * 1024;
/// V4: the command string.
pub const COMMAND_MAX_BYTES: usize = 4096;
/// V3: tasks per file.
pub const TASKS_MAX: usize = 64;
/// V3: task name length.
pub const NAME_MAX: usize = 64;
/// V5: ref patterns per task, and per-pattern length.
pub const REFS_PER_TASK_MAX: usize = 64;
pub const REF_PATTERN_MAX: usize = 255;
/// V6: timeout / `claim_ttl` bounds, in seconds.
pub const TIMEOUT_MIN_SECS: u64 = 1;
pub const TIMEOUT_MAX_SECS: u64 = 24 * 3600;
/// V7: retry bound.
pub const ATTEMPTS_MAX: u32 = 10;
/// V8: `env_allow` entries.
pub const ENV_ALLOW_MAX: usize = 64;

/// Pipeline-level defaults (§3), applied where a task omits the field.
pub const DEFAULT_REFS = [_][]const u8{"refs/heads/*"};
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_CLAIM_TTL_SECS: u64 = 300;
pub const DEFAULT_ATTEMPTS: u32 = 1;

/// A validated task with pipeline defaults applied.
pub const CiResolvedTask = struct {
    name: []const u8,
    refs: []const []const u8,
    command: []const u8,
    timeout_secs: u64,
    max_attempts: u32,
    env_allow: []const []const u8,
    /// The validated cron expression (V10), when the task is also periodic.
    schedule: ?[]const u8 = null,
    /// The validated artifact paths (V11).
    artifacts: []const []const u8 = &.{},
};

/// A validated `.walgit/ci.toml`.
pub const CiResolved = struct {
    claim_ttl_secs: u64,
    tasks: []const CiResolvedTask,

    /// The tasks whose `refs` patterns match `ref_name`, in declaration order.
    pub fn matching(self: *const CiResolved, alloc: std.mem.Allocator, ref_name: []const u8) ![]*const CiResolvedTask {
        var out: util.List(*const CiResolvedTask) = .init(alloc);
        for (self.tasks) |*t| {
            for (t.refs) |p| {
                if (glob_match(p, ref_name)) {
                    try out.append(t);
                    break;
                }
            }
        }
        return out.to_owned_slice();
    }
};

/// Parse + validate (§3.1 V1–V9). `Err` carries every violation found —
/// the joined message is the CLI's stderr.
pub fn parse_and_validate(alloc: std.mem.Allocator, raw: []const u8) !CiResolved {
    var errs: util.List([]const u8) = .init(alloc);
    defer errs.deinit();
    if (raw.len > FILE_MAX_BYTES) {
        return error.V9FileTooBig;
    }
    const root = toml.parse(alloc, raw) catch |e| switch (e) {
        error.OutOfMemory => return error.OutOfMemory,
        else => {
            return error.V1Syntax;
        },
    };
    const CiKeys = struct {
        fn check_root(t: *const toml.Table) !void {
            for (t.keys.items) |k| {
                const known = std.mem.eql(u8, k, "version") or std.mem.eql(u8, k, "claim_ttl") or
                    std.mem.eql(u8, k, "timeout") or std.mem.eql(u8, k, "max_attempts") or
                    std.mem.eql(u8, k, "task");
                if (!known) {
                    return error.V1UnknownField;
                }
            }
        }
        fn check_task(t: *const toml.Table) !void {
            for (t.keys.items) |k| {
                const known = std.mem.eql(u8, k, "name") or std.mem.eql(u8, k, "refs") or
                    std.mem.eql(u8, k, "command") or std.mem.eql(u8, k, "timeout") or
                    std.mem.eql(u8, k, "max_attempts") or std.mem.eql(u8, k, "env_allow") or
                    std.mem.eql(u8, k, "schedule") or std.mem.eql(u8, k, "artifacts");
                if (!known) {
                    return error.V1UnknownField;
                }
            }
        }
    };
    try CiKeys.check_root(root);

    const version_v = root.get("version");
    const version: i64 = if (version_v) |v| switch (v) {
        .integer => |i| i,
        else => return error.V1Syntax,
    } else return error.V1Syntax;
    if (version != 1) {
        try appendf(alloc, &errs, "V2: version must be 1, got {d}", .{version});
    }

    const task_vals = root.tables("task");
    if (task_vals.len == 0) {
        try errs.append("V3: at least one [[task]] is required");
    }
    if (task_vals.len > TASKS_MAX) {
        try appendf(alloc, &errs, "V3: {d} tasks, limit {d}", .{ task_vals.len, TASKS_MAX });
    }

    var claim_ttl_secs: u64 = DEFAULT_CLAIM_TTL_SECS;
    if (root.get("claim_ttl")) |v| {
        if (v != .string) return error.V1Syntax;
        if (parse_duration_secs(v.string)) |secs| {
            if (TIMEOUT_MIN_SECS <= secs and secs <= TIMEOUT_MAX_SECS) {
                claim_ttl_secs = secs;
            } else {
                try appendf(alloc, &errs, "V6: claim_ttl \"{s}\" = {d}s outside [{d}, {d}]", .{ v.string, secs, TIMEOUT_MIN_SECS, TIMEOUT_MAX_SECS });
            }
        } else |_| {
            try appendf(alloc, &errs, "V6: claim_ttl \"{s}\": not a duration", .{v.string});
        }
    }
    var pipeline_timeout_secs: u64 = DEFAULT_TIMEOUT_SECS;
    if (root.get("timeout")) |v| {
        if (v != .string) return error.V1Syntax;
        if (parse_duration_secs(v.string)) |secs| {
            if (TIMEOUT_MIN_SECS <= secs and secs <= TIMEOUT_MAX_SECS) {
                pipeline_timeout_secs = secs;
            } else {
                try appendf(alloc, &errs, "V6: pipeline timeout \"{s}\" is not a valid duration in [{d}, {d}]", .{ v.string, TIMEOUT_MIN_SECS, TIMEOUT_MAX_SECS });
            }
        } else |_| {
            try appendf(alloc, &errs, "V6: pipeline timeout \"{s}\": not a duration", .{v.string});
        }
    }
    var pipeline_max_attempts: u32 = DEFAULT_ATTEMPTS;
    if (root.get("max_attempts")) |v| {
        const n = int_of(v) orelse return error.V1Syntax;
        if (n < 1 or n > ATTEMPTS_MAX) {
            try appendf(alloc, &errs, "V7: pipeline max_attempts {d} outside [1, {d}]", .{ n, ATTEMPTS_MAX });
        } else {
            pipeline_max_attempts = @intCast(n);
        }
    }

    var seen: util.List([]const u8) = .init(alloc);
    defer seen.deinit();
    var tasks: util.List(CiResolvedTask) = .init(alloc);

    for (task_vals) |tv| {
        const t = switch (tv) {
            .table => |t| t,
            else => return error.V1Syntax,
        };
        try CiKeys.check_task(t);
        const name_v = t.get("name") orelse {
            try errs.append("V3: a [[task]] has no name");
            continue;
        };
        if (name_v != .string) return error.V1Syntax;
        const name = name_v.string;
        const name_ok = name.len <= NAME_MAX and name.len > 0 and blk: {
            for (name) |c| {
                if (!(std.ascii.isAlphanumeric(c) or c == '.' or c == '_' or c == '-')) break :blk false;
            }
            break :blk true;
        };
        if (!name_ok) {
            try appendf(alloc, &errs, "V3: task name \"{s}\" must match [A-Za-z0-9._-]{{1,{d}}}", .{ name, NAME_MAX });
        }
        var dup = false;
        for (seen.items) |s| {
            if (std.mem.eql(u8, s, name)) dup = true;
        }
        if (dup) {
            try appendf(alloc, &errs, "V3: duplicate task name \"{s}\"", .{name});
        } else {
            try seen.append(name);
        }

        var command: []const u8 = "";
        if (t.get("command")) |v| {
            if (v != .string) return error.V1Syntax;
            command = v.string;
            if (command.len == 0 or command.len > COMMAND_MAX_BYTES) {
                try appendf(alloc, &errs, "V4: task \"{s}\" command must be 1..{d} bytes, got {d}", .{ name, COMMAND_MAX_BYTES, command.len });
            }
        } else {
            try errs.append("V4: a [[task]] has no command");
            command = "";
        }

        var refs: util.List([]const u8) = .init(alloc);
        if (t.get("refs")) |v| {
            for (v.array) |item| {
                if (item != .string) return error.V1Syntax;
                try refs.append(item.string);
            }
        } else {
            try refs.append_slice(&DEFAULT_REFS);
        }
        if (refs.items.len == 0 or refs.items.len > REFS_PER_TASK_MAX) {
            try appendf(alloc, &errs, "V5: task \"{s}\" refs must hold 1..{d} patterns", .{ name, REFS_PER_TASK_MAX });
        }
        for (refs.items) |r| {
            if (r.len == 0 or r.len > REF_PATTERN_MAX or !std.mem.startsWith(u8, r, "refs/")) {
                try appendf(alloc, &errs, "V5: task \"{s}\" ref pattern \"{s}\" must start with `refs/` and be ≤ {d} bytes", .{ name, r, REF_PATTERN_MAX });
            }
        }

        var timeout_secs = pipeline_timeout_secs;
        if (t.get("timeout")) |v| {
            if (v != .string) return error.V1Syntax;
            if (parse_duration_secs(v.string)) |secs| {
                if (TIMEOUT_MIN_SECS <= secs and secs <= TIMEOUT_MAX_SECS) {
                    timeout_secs = secs;
                } else {
                    try appendf(alloc, &errs, "V6: task \"{s}\" timeout \"{s}\" = {d}s outside [{d}, {d}]", .{ name, v.string, secs, TIMEOUT_MIN_SECS, TIMEOUT_MAX_SECS });
                }
            } else |_| {
                try appendf(alloc, &errs, "V6: task \"{s}\" timeout \"{s}\": not a duration", .{ name, v.string });
            }
        }

        var max_attempts = pipeline_max_attempts;
        if (t.get("max_attempts")) |v| {
            const n = int_of(v) orelse return error.V1Syntax;
            if (n < 1 or n > ATTEMPTS_MAX) {
                try appendf(alloc, &errs, "V7: task \"{s}\" max_attempts {d} outside [1, {d}]", .{ name, n, ATTEMPTS_MAX });
            } else {
                max_attempts = @intCast(n);
            }
        }

        var env_allow: util.List([]const u8) = .init(alloc);
        if (t.get("env_allow")) |v| {
            for (v.array) |item| {
                if (item != .string) return error.V1Syntax;
                try env_allow.append(item.string);
            }
        }
        if (env_allow.items.len > ENV_ALLOW_MAX) {
            try appendf(alloc, &errs, "V8: task \"{s}\" env_allow has {d} entries, limit {d}", .{ name, env_allow.items.len, ENV_ALLOW_MAX });
        }
        for (env_allow.items) |env_name| {
            if (!valid_env_name(env_name) or std.mem.startsWith(u8, env_name, "WALGIT_CI_")) {
                try appendf(alloc, &errs, "V8: task \"{s}\" env_allow \"{s}\" is not a valid env name (or reserves the WALGIT_CI_* namespace, which the runner injects)", .{ name, env_name });
            }
        }

        var schedule: ?[]const u8 = null;
        if (t.get("schedule")) |v| {
            if (v != .string) return error.V1Syntax;
            schedule = v.string;
            const cron = @import("cron.zig");
            var sched_arena = std.heap.ArenaAllocator.init(alloc);
            defer sched_arena.deinit();
            const sched_ok = if (v.string.len == 0 or v.string.len > 255) false else (cron.parse_schedule_a(sched_arena.allocator(), v.string) catch null) != null;
            if (!sched_ok) {
                try appendf(alloc, &errs, "V10: task \"{s}\" schedule \"{s}\" must be a 6/7-field UTC cron (sec min hour dom mon dow) or @hourly/@daily/@weekly", .{ name, v.string });
            }
        }

        var artifacts: util.List([]const u8) = .init(alloc);
        if (t.get("artifacts")) |v| {
            for (v.array) |item| {
                if (item != .string) return error.V1Syntax;
                try artifacts.append(item.string);
            }
        }
        if (artifacts.items.len > ci.CI_ARTIFACTS_PER_RESULT_MAX) {
            try appendf(alloc, &errs, "V11: task \"{s}\" artifacts has {d} entries, limit {d}", .{ name, artifacts.items.len, ci.CI_ARTIFACTS_PER_RESULT_MAX });
        }
        for (artifacts.items) |a| {
            const ok = a.len > 0 and a.len <= 255 and a[0] != '/' and
                std.mem.indexOfScalar(u8, a, '\\') == null and
                std.mem.indexOfScalar(u8, a, ':') == null and blk: {
                var it = std.mem.splitScalar(u8, a, '/');
                while (it.next()) |seg| {
                    if (seg.len == 0 or std.mem.eql(u8, seg, ".") or std.mem.eql(u8, seg, "..")) break :blk false;
                }
                break :blk true;
            };
            if (!ok) {
                try appendf(alloc, &errs, "V11: task \"{s}\" artifacts entry \"{s}\" must be a relative path (no leading /, no \\ or drive letter, no empty/./.. components)", .{ name, a });
            }
        }

        try tasks.append(.{
            .name = name,
            .refs = try refs.to_owned_slice(),
            .command = command,
            .timeout_secs = timeout_secs,
            .max_attempts = max_attempts,
            .env_allow = try env_allow.to_owned_slice(),
            .schedule = schedule,
            .artifacts = try artifacts.to_owned_slice(),
        });
    }

    if (errs.items.len > 0) {
        // One message, newline-joined — the caller prints it.
        var msg: util.List(u8) = .init(alloc);
        for (errs.items, 0..) |e, i| {
            if (i > 0) try msg.append('\n');
            try msg.append_slice(e);
        }
        const out = try alloc.alloc(u8, msg.items.len);
        @memcpy(out, msg.items);
        validation_msg = out;
        return error.Invalid;
    }
    return .{
        .claim_ttl_secs = claim_ttl_secs,
        .tasks = try tasks.to_owned_slice(),
    };
}

/// The collected violations of the last `parse_and_validate` failure (the
/// error set cannot carry the message; the CLI reads this instead).
pub var validation_msg: []const u8 = "";

fn appendf(alloc: std.mem.Allocator, errs: *util.List([]const u8), comptime fmt: []const u8, args: anytype) !void {
    try errs.append(std.fmt.allocPrint(alloc, fmt, args) catch return error.OutOfMemory);
}

fn int_of(v: toml.Value) ?i64 {
    return switch (v) {
        .integer => |i| i,
        else => null,
    };
}

/// `"30s"` / `"10m"` / `"1h"` / compound `"1h 30m"` → whole seconds.
pub fn parse_duration_secs(s: []const u8) !u64 {
    var total: u64 = 0;
    var any = false;
    var it = std.mem.tokenizeAny(u8, s, " \t");
    while (it.next()) |part| {
        var i: usize = 0;
        while (i < part.len and std.ascii.isDigit(part[i])) i += 1;
        if (i == 0) return error.NotADuration;
        const n = std.fmt.parseInt(u64, part[0..i], 10) catch return error.NotADuration;
        const unit = part[i..];
        const mult: u64 = blk: {
            if (std.mem.eql(u8, unit, "s")) break :blk 1;
            if (std.mem.eql(u8, unit, "m")) break :blk 60;
            if (std.mem.eql(u8, unit, "h")) break :blk 3600;
            if (std.mem.eql(u8, unit, "d")) break :blk 86400;
            if (std.mem.eql(u8, unit, "ms")) break :blk 0; // sub-second rounds to 0 here
            if (std.mem.eql(u8, unit, "us") or std.mem.eql(u8, unit, "ns")) break :blk 0;
            return error.NotADuration;
        };
        total = std.math.add(u64, total, std.math.mul(u64, n, mult) catch return error.NotADuration) catch return error.NotADuration;
        any = true;
    }
    if (!any) return error.NotADuration;
    return total;
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
pub fn valid_env_name(name: []const u8) bool {
    if (name.len == 0) return false;
    const first = name[0];
    if (!(std.ascii.isAlphabetic(first) or first == '_')) return false;
    for (name[1..]) |c| {
        if (!(std.ascii.isAlphanumeric(c) or c == '_')) return false;
    }
    return true;
}

/// Glob over ref names (§3.2): `*` matches any run of characters **including
/// `/`**, `?` one character, everything else literal; the whole name must
/// match. Iterative greedy backtracking, no allocations per candidate.
pub fn glob_match(pattern: []const u8, name: []const u8) bool {
    const p = pattern;
    const n = name;
    var pi: usize = 0;
    var ni: usize = 0;
    var star: usize = std.math.maxInt(usize);
    var mark: usize = 0;
    while (ni < n.len) {
        const pc: ?u8 = if (pi < p.len) p[pi] else null;
        if (pc != null and (pc.? == '?' or pc.? == n[ni])) {
            pi += 1;
            ni += 1;
        } else if (pc != null and pc.? == '*') {
            star = pi;
            mark = ni;
            pi += 1;
        } else if (star != std.math.maxInt(usize)) {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while (pi < p.len and p[pi] == '*') pi += 1;
    return pi == p.len;
}

// ---- tests -----------------------------------------------------------------------

const testing = std.testing;

test "glob matches the §3.2 semantics" {
    try testing.expect(glob_match("refs/heads/*", "refs/heads/main"));
    try testing.expect(glob_match("refs/heads/*", "refs/heads/a/b")); // `*` spans `/`
    try testing.expect(glob_match("refs/tags/v*", "refs/tags/v1.0"));
    try testing.expect(glob_match("refs/heads/feature-?", "refs/heads/feature-x"));
    try testing.expect(!glob_match("refs/heads/*", "refs/tags/v1"));
    try testing.expect(!glob_match("refs/heads/ma*", "refs/heads")); // must match whole name
    try testing.expect(glob_match("refs/heads/main", "refs/heads/main"));
}

test "durations parse" {
    try testing.expectEqual(@as(u64, 30), try parse_duration_secs("30s"));
    try testing.expectEqual(@as(u64, 600), try parse_duration_secs("10m"));
    try testing.expectEqual(@as(u64, 3600), try parse_duration_secs("1h"));
    try testing.expectEqual(@as(u64, 5400), try parse_duration_secs("1h 30m"));
    try testing.expectError(error.NotADuration, parse_duration_secs("abc"));
    try testing.expectError(error.NotADuration, parse_duration_secs(""));
}

test "ci.toml validates with defaults applied" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const doc =
        \\version = 1
        \\
        \\[[task]]
        \\name = "test"
        \\command = "just test"
        \\
        \\[[task]]
        \\name = "lint"
        \\command = "pnpm lint"
        \\refs = ["refs/tags/v*"]
        \\timeout = "1h"
        \\max_attempts = 2
        \\env_allow = ["CI"]
    ;
    const cfg = try parse_and_validate(arena, doc);
    try testing.expectEqual(@as(u64, 300), cfg.claim_ttl_secs); // default
    try testing.expectEqual(@as(usize, 2), cfg.tasks.len);
    try testing.expectEqual(@as(u64, 600), cfg.tasks[0].timeout_secs); // pipeline default
    try testing.expectEqual(@as(u32, 1), cfg.tasks[0].max_attempts);
    try testing.expectEqual(@as(u64, 3600), cfg.tasks[1].timeout_secs);
    try testing.expectEqual(@as(u32, 2), cfg.tasks[1].max_attempts);
    try testing.expectEqual(@as(usize, 1), cfg.tasks[0].refs.len);
    try testing.expectEqualStrings("refs/heads/*", cfg.tasks[0].refs[0]);
    const matched = try cfg.matching(arena, "refs/tags/v2.0");
    try testing.expectEqual(@as(usize, 1), matched.len);
    try testing.expectEqualStrings("lint", matched[0].name);

    // An unknown key aborts at the shape check (deny_unknown_fields).
    const unknown = "version = 1\nnoise = 1\n[[task]]\nname = \"x\"\ncommand = \"y\"\n";
    try testing.expectError(error.V1UnknownField, parse_and_validate(arena, unknown));
    // Collectible violations (V2/V3/V4) land in one newline-joined message.
    const bad = "version = 2\n[[task]]\nname = \"\"\ncommand = \"\"\n";
    const r = parse_and_validate(arena, bad);
    try testing.expectError(error.Invalid, r);
    try testing.expect(std.mem.indexOf(u8, validation_msg, "V2:") != null);
    try testing.expect(std.mem.indexOf(u8, validation_msg, "V3:") != null);
    try testing.expect(std.mem.indexOf(u8, validation_msg, "V4:") != null);
}
