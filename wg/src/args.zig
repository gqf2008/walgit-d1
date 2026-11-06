//! Argument parsing for wg — long flags only, mirroring the retired clap
//! surface: `--flag value`, `--flag=value`, boolean `--flag`, repeatable
//! flags collect in order, positional arguments are collected separately.

const std = @import("std");
const util = @import("util.zig");

pub const Spec = struct {
    name: []const u8,
    /// The flag takes a value (the default for every spec; set `boolean`
    /// for flags that never do).
    boolean: bool = false,
    repeat: bool = false,
};

pub const Parsed = struct {
    alloc: std.mem.Allocator,
    values: std.StringHashMap([]const u8),
    flags: std.StringHashMap(void),
    multis: std.StringHashMap(util.List([]const u8)),
    positionals: util.List([]const u8),

    pub fn deinit(self: *Parsed) void {
        var it = self.multis.iterator();
        while (it.next()) |e| e.value_ptr.deinit();
        self.values.deinit();
        self.flags.deinit();
        self.multis.deinit();
        self.positionals.deinit();
    }

    pub fn opt(self: *const Parsed, name: []const u8) ?[]const u8 {
        return self.values.get(name);
    }

    pub fn get(self: *const Parsed, name: []const u8, default: []const u8) []const u8 {
        return self.values.get(name) orelse default;
    }

    pub fn boolean(self: *const Parsed, name: []const u8) bool {
        return self.flags.contains(name);
    }

    pub fn many(self: *const Parsed, name: []const u8) []const []const u8 {
        if (self.multis.get(name)) |list| return list.items;
        return &.{};
    }

    pub fn positional(self: *const Parsed, index: usize) ?[]const u8 {
        if (index >= self.positionals.items.len) return null;
        return self.positionals.items[index];
    }

    /// A required value flag; a missing one names the flag on stderr.
    pub fn required(self: *const Parsed, name: []const u8) ![]const u8 {
        return self.values.get(name) orelse {
            std.debug.print("wg: missing required --{s}\n", .{name});
            return error.MissingFlag;
        };
    }

    /// A required positional (no `--name`); missing names the slot.
    pub fn required_positional(self: *const Parsed, name: []const u8, index: usize) ![]const u8 {
        return self.positional(index) orelse {
            std.debug.print("wg: missing required argument <{s}>\n", .{name});
            return error.MissingPositional;
        };
    }

    pub fn u64_of(self: *const Parsed, name: []const u8, default: u64) !u64 {
        const v = self.opt(name) orelse return default;
        return std.fmt.parseInt(u64, v, 10) catch {
            std.debug.print("wg: --{s} must be a number, got \"{s}\"\n", .{ name, v });
            return error.BadFlagValue;
        };
    }
};

pub fn parse(alloc: std.mem.Allocator, argv: []const []const u8, specs: []const Spec) !Parsed {
    var p = Parsed{
        .alloc = alloc,
        .values = std.StringHashMap([]const u8).init(alloc),
        .flags = std.StringHashMap(void).init(alloc),
        .multis = std.StringHashMap(util.List([]const u8)).init(alloc),
        .positionals = util.List([]const u8).init(alloc),
    };
    errdefer p.deinit();
    var i: usize = 0;
    while (i < argv.len) : (i += 1) {
        const arg = argv[i];
        if (std.mem.startsWith(u8, arg, "--")) {
            var name = arg[2..];
            var inline_value: ?[]const u8 = null;
            if (std.mem.indexOfScalar(u8, name, '=')) |eq| {
                inline_value = name[eq + 1 ..];
                name = name[0..eq];
            }
            const spec = blk: {
                for (specs) |s| {
                    if (std.mem.eql(u8, s.name, name)) break :blk s;
                }
                std.debug.print("wg: unknown flag --{s}\n", .{name});
                return error.UnknownFlag;
            };
            if (spec.boolean) {
                try p.flags.put(try alloc.dupe(u8, name), {});
                continue;
            }
            var value: []const u8 = undefined;
            if (inline_value) |v| {
                value = v;
            } else {
                i += 1;
                if (i >= argv.len) {
                    std.debug.print("wg: --{s} expects a value\n", .{name});
                    return error.MissingFlagValue;
                }
                value = argv[i];
            }
            const owned = try alloc.dupe(u8, value);
            if (spec.repeat) {
                const gop = try p.multis.getOrPut(try alloc.dupe(u8, name));
                if (!gop.found_existing) gop.value_ptr.* = util.List([]const u8).init(alloc);
                try gop.value_ptr.append(owned);
            } else {
                try p.values.put(try alloc.dupe(u8, name), owned);
            }
            continue;
        }
        try p.positionals.append(try alloc.dupe(u8, arg));
    }
    return p;
}

test "parse flags, inline values, repeats and positionals" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const a = arena_state.allocator();
    var p = try parse(a, &.{ "--kind", "issue", "--id=cc-ai-x", "--push", "origin", "--body", "{\"t\":1}", "extra-positional" }, &.{
        .{ .name = "kind" },
        .{ .name = "id" },
        .{ .name = "push" },
        .{ .name = "body" },
        .{ .name = "force", .boolean = true },
        .{ .name = "attach", .repeat = true },
    });
    defer p.deinit();
    try std.testing.expectEqualStrings("issue", p.get("kind", ""));
    try std.testing.expectEqualStrings("cc-ai-x", p.get("id", ""));
    try std.testing.expectEqualStrings("origin", p.opt("push").?);
    try std.testing.expectEqualStrings("{\"t\":1}", p.get("body", ""));
    try std.testing.expect(!p.boolean("force"));
    try std.testing.expectEqualStrings("extra-positional", p.positional(0).?);
    // unknown flags and missing values are errors
    try std.testing.expectError(error.UnknownFlag, parse(a, &.{"--nope"}, &.{}));
    try std.testing.expectError(error.MissingFlagValue, parse(a, &.{"--kind"}, &.{.{ .name = "kind" }}));
}
