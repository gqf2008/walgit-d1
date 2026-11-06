//! The wgl entrypoint: `wgl <collab|principal|ci> <action> [--flags]`.
//! One arena per run; every command is a leaf returning an exit code (the
//! card-claim stand-down is exit 2, §8.4).

const std = @import("std");
const args = @import("args.zig");
const cmd_collab = @import("cmd_collab.zig");
const cmd_principal = @import("cmd_principal.zig");
const cmd_ci = @import("cmd_ci.zig");
const util = @import("util.zig");

const usage =
    \\wgl — the standalone D1 collaboration client (docs/D1_PROTOCOL.md)
    \\
    \\usage:
    \\  wgl collab ls|thread|thread-heads|pr|report|board|entry|join|
    \\            principal-register|principal-revoke|principal-fetch|
    \\            claim|gc|watch
    \\  wgl principal register|rotate|list|revoke
    \\  wgl ci validate|run|status|log|artifacts
    \\
;

pub fn main(init: std.process.Init) !u8 {
    var arena_state = std.heap.ArenaAllocator.init(init.gpa);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();

    var argv: util.List([]const u8) = util.List([]const u8).init(alloc);
    var it = init.minimal.args.iterate();
    _ = it.next(); // argv[0]
    while (it.next()) |arg| try argv.append(arg);
    if (argv.items.len < 2) {
        std.debug.print("{s}", .{usage});
        return 2;
    }
    const group = argv.items[0];
    const action = argv.items[1];
    const rest = argv.items[2..];

    var ctx = cmd_collab.Ctx{
        .alloc = alloc,
        .io = init.io,
        .env = init.environ_map,
    };

    if (std.mem.eql(u8, group, "collab")) {
        return collab_dispatch(&ctx, action, rest);
    }
    if (std.mem.eql(u8, group, "principal")) {
        return principal_dispatch(&ctx, action, rest);
    }
    if (std.mem.eql(u8, group, "ci")) {
        return cmd_ci.dispatch(&ctx, action, rest);
    }
    std.debug.print("wgl: unknown group \"{s}\"\n{s}", .{ group, usage });
    return 2;
}

fn collab_dispatch(ctx: *cmd_collab.Ctx, action: []const u8, rest: []const []const u8) u8 {
    var parsed = args.parse(ctx.alloc, rest, specs_for(action)) catch |err| switch (err) {
        error.OutOfMemory => return 1,
        else => return 2, // usage errors
    };
    defer parsed.deinit();
    const runner: ?*const fn (*cmd_collab.Ctx, *args.Parsed) anyerror!u8 = blk: {
        if (eq(action, "ls")) break :blk &cmd_collab.ls;
        if (eq(action, "thread")) break :blk &cmd_collab.thread;
        if (eq(action, "thread-heads")) break :blk &cmd_collab.thread_heads;
        if (eq(action, "pr")) break :blk &cmd_collab.pr;
        if (eq(action, "report")) break :blk &cmd_collab.report;
        if (eq(action, "board")) break :blk &cmd_collab.board;
        if (eq(action, "entry")) break :blk &cmd_collab.entry;
        if (eq(action, "join")) break :blk &cmd_collab.join;
        if (eq(action, "principal-register")) break :blk &cmd_collab.principal_register;
        if (eq(action, "principal-revoke")) break :blk &cmd_collab.principal_revoke;
        if (eq(action, "principal-fetch")) break :blk &cmd_collab.principal_fetch;
        if (eq(action, "claim")) break :blk &cmd_collab.claim;
        if (eq(action, "gc")) break :blk &cmd_collab.gc;
        if (eq(action, "watch")) break :blk &cmd_collab.watch;
        break :blk null;
    };
    const run = runner orelse {
        std.debug.print("wgl: unknown collab action \"{s}\"\n", .{action});
        return 2;
    };
    return run(ctx, &parsed) catch |err| {
        report_failure(ctx, "collab", action, err);
        return 1;
    };
}

fn specs_for(action: []const u8) []const args.Spec {
    const S = args.Spec;
    const repo = S{ .name = "repo" };
    const key = S{ .name = "key" };
    const push = S{ .name = "push" };
    const actor = S{ .name = "actor" };
    const rules = S{ .name = "rules" };
    const format = S{ .name = "format" };
    const token = S{ .name = "token" };
    return switch (intern(action)) {
        .ls, .thread_heads => &.{repo},
        .thread, .pr => &.{ repo, rules },
        .board => &.{ repo, format, S{ .name = "board" }, rules },
        .report => &.{ repo, format, rules, S{ .name = "remote" } },
        .entry => &.{
            repo,                              push,
            S{ .name = "kind" },               S{ .name = "id" },
            actor,                             S{ .name = "parent" },
            S{ .name = "body" },               S{ .name = "base" },
            S{ .name = "head" },               key,
            S{ .name = "related", .repeat = true },
            S{ .name = "depends-on", .repeat = true },
            S{ .name = "attach", .repeat = true },
            S{ .name = "auto-fold", .boolean = true },
            S{ .name = "fold-threshold" },
        },
        .join => &.{ repo, S{ .name = "principal" }, key, push, S{ .name = "force", .boolean = true } },
        .principal_register => &.{ repo, actor, key, push },
        .principal_revoke => &.{ repo, S{ .name = "principal" }, push },
        .principal_fetch => &.{ repo, S{ .name = "remote" }, token },
        .claim => &.{
            repo,                          S{ .name = "id" },
            S{ .name = "push" },           S{ .name = "ttl" },
            actor,                         key,
            S{ .name = "session" },        S{ .name = "worktree" },
            S{ .name = "branch" },         S{ .name = "work" },
            format,
        },
        .gc => &.{ repo, actor, key, push, S{ .name = "truncate", .boolean = true } },
        .watch => &.{
            repo,                        S{ .name = "remote" },
            S{ .name = "interval" },     S{ .name = "once", .boolean = true },
            S{ .name = "exec" },         S{ .name = "state" },
        },
        else => &.{},
    };
}

const Action = enum {
    ls,
    thread,
    thread_heads,
    pr,
    report,
    board,
    entry,
    join,
    principal_register,
    principal_revoke,
    principal_fetch,
    claim,
    gc,
    watch,
    unknown,
};

fn intern(action: []const u8) Action {
    if (eq(action, "ls")) return .ls;
    if (eq(action, "thread")) return .thread;
    if (eq(action, "thread-heads")) return .thread_heads;
    if (eq(action, "pr")) return .pr;
    if (eq(action, "report")) return .report;
    if (eq(action, "board")) return .board;
    if (eq(action, "entry")) return .entry;
    if (eq(action, "join")) return .join;
    if (eq(action, "principal-register")) return .principal_register;
    if (eq(action, "principal-revoke")) return .principal_revoke;
    if (eq(action, "principal-fetch")) return .principal_fetch;
    if (eq(action, "claim")) return .claim;
    if (eq(action, "gc")) return .gc;
    if (eq(action, "watch")) return .watch;
    return .unknown;
}

fn eq(a: []const u8, b: []const u8) bool {
    return std.mem.eql(u8, a, b);
}

/// The failure printer: git errors carry their stderr (the last failed
/// invocation's), everything else prints the error name — the command
/// sites that explain themselves return before this fires.
fn report_failure(ctx: *cmd_collab.Ctx, group: []const u8, action: []const u8, err: anyerror) void {
    _ = ctx;
    const gitx = @import("gitx.zig");
    if (gitx.last_stderr.len > 0) {
        std.debug.print("wgl: {s} {s}: git: {s}\n", .{ group, action, gitx.last_stderr });
        gitx.last_stderr = "";
        return;
    }
    if (err == error.OutOfMemory) {
        std.debug.print("wgl: {s} {s}: out of memory\n", .{ group, action });
        return;
    }
    std.debug.print("wgl: {s} {s}: {t}\n", .{ group, action, err });
}

fn ci_fail(c: *cmd_collab.Ctx, action: []const u8, err: anyerror) u8 {
    report_failure(c, "ci", action, err);
    return 1;
}

fn principal_dispatch(ctx: *cmd_collab.Ctx, action: []const u8, rest: []const []const u8) u8 {
    const S = args.Spec;
    const specs: []const S = switch (intern_pr(action)) {
        .register, .rotate => &.{
            S{ .name = "url" },       S{ .name = "principal" },
            S{ .name = "key" },       S{ .name = "token" },
        },
        .list => &.{ S{ .name = "url" }, S{ .name = "token" } },
        .revoke => &.{ S{ .name = "url" }, S{ .name = "principal" }, S{ .name = "token" } },
        .unknown => &.{},
    };
    var parsed = args.parse(ctx.alloc, rest, specs) catch |err| switch (err) {
        error.OutOfMemory => return 1,
        else => return 2,
    };
    defer parsed.deinit();
    return cmd_principal.run(ctx, action, &parsed) catch |err| {
        report_failure(ctx, "principal", action, err);
        return 1;
    };
}

const PrincipalAction = enum { register, rotate, list, revoke, unknown };

fn intern_pr(action: []const u8) PrincipalAction {
    if (eq(action, "register")) return .register;
    if (eq(action, "rotate")) return .rotate;
    if (eq(action, "list")) return .list;
    if (eq(action, "revoke")) return .revoke;
    return .unknown;
}
