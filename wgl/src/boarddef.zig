//! The board definition document (`.walgit/board.toml`,
//! `docs/D1_PROTOCOL.md` §8.1): TOML in, the projection's `BoardDef` out,
//! fail-closed on unknown keys — a typo must not silently fold cards into
//! the wrong lane.

const std = @import("std");
const toml = @import("toml.zig");
const collab = @import("d1/collab.zig");
const util = @import("util.zig");

pub const Error = error{
    Syntax,
    UnknownField,
    MissingField,
    WrongType,
    BadVersion,
    BadMergeVerdict,
    EmptyName,
    DuplicateColumn,
    NoColumns,
    OutOfMemory,
};

const BoardDefKnown = struct {
    fn check_table(t: *const toml.Table) Error!void {
        for (t.keys.items) |k| {
            if (std.mem.eql(u8, k, "version") or std.mem.eql(u8, k, "sort") or
                std.mem.eql(u8, k, "column")) continue;
            return error.UnknownField;
        }
    }
    fn check_column(t: *const toml.Table) Error!void {
        for (t.keys.items) |k| {
            if (std.mem.eql(u8, k, "name") or std.mem.eql(u8, k, "kind") or
                std.mem.eql(u8, k, "status") or std.mem.eql(u8, k, "merge") or
                std.mem.eql(u8, k, "unverified")) continue;
            return error.UnknownField;
        }
    }
    fn check_sort(t: *const toml.Table) Error!void {
        for (t.keys.items) |k| {
            if (std.mem.eql(u8, k, "by") or std.mem.eql(u8, k, "direction")) continue;
            return error.UnknownField;
        }
    }
};

/// Parse + validate + map. `version != 1`, no columns, an empty/duplicate
/// column name or an unknown `merge` verdict is a broken board.
pub fn parse_board_def(arena: std.mem.Allocator, doc: []const u8) Error!collab.BoardDef {
    const root = toml.parse(arena, doc) catch |e| switch (e) {
        error.DuplicateKey => return error.Syntax,
        else => return error.Syntax,
    };
    try BoardDefKnown.check_table(root);
    // version is required (no default in the serde shape).
    const version_v = root.get("version") orelse return error.MissingField;
    if (version_v != .integer or version_v.integer != 1) return error.BadVersion;

    var sort = collab.BoardSort{};
    if (root.get("sort")) |sv| {
        const st = switch (sv) {
            .table => |t| t,
            else => return error.WrongType,
        };
        try BoardDefKnown.check_sort(st);
        if (st.get("by")) |bv| {
            if (bv != .string) return error.WrongType;
            if (std.mem.eql(u8, bv.string, "ts")) {
                sort.by = .ts;
            } else if (std.mem.eql(u8, bv.string, "id")) {
                sort.by = .id;
            } else return error.WrongType;
        }
        if (st.get("direction")) |dv| {
            if (dv != .string) return error.WrongType;
            if (std.mem.eql(u8, dv.string, "desc")) {
                sort.direction = .desc;
            } else if (std.mem.eql(u8, dv.string, "asc")) {
                sort.direction = .asc;
            } else return error.WrongType;
        }
    }

    const col_vals = root.tables("column");
    var columns: util.List(collab.BoardColumnDef) = util.List(collab.BoardColumnDef).init(arena);
    for (col_vals) |cv| {
        const ct = switch (cv) {
            .table => |t| t,
            else => return error.WrongType,
        };
        try BoardDefKnown.check_column(ct);
        const name_v = ct.get("name") orelse return error.MissingField;
        if (name_v != .string or name_v.string.len == 0) return error.EmptyName;
        var col = collab.BoardColumnDef{ .name = name_v.string };
        if (ct.get("kind")) |v| {
            if (v != .string) return error.WrongType;
            col.kind = v.string;
        }
        if (ct.get("status")) |v| {
            if (v != .string) return error.WrongType;
            col.status = v.string;
        }
        if (ct.get("merge")) |v| {
            if (v != .string) return error.WrongType;
            col.merge = v.string;
        }
        if (ct.get("unverified")) |v| {
            if (v != .boolean) return error.WrongType;
            col.unverified = v.boolean;
        }
        try columns.append(col);
    }
    const def = collab.BoardDef{
        .version = 1,
        .sort = sort,
        .columns = try columns.to_owned_slice(),
    };
    collab.validate_board_def(&def) catch |e| switch (e) {
        error.BoardVersion => return error.BadVersion,
        error.BoardNoColumns => return error.NoColumns,
        error.BoardColumnName => return error.EmptyName,
        error.BoardDuplicateColumn => return error.DuplicateColumn,
        error.BoardMergeVerdict => return error.BadMergeVerdict,
        else => return error.Syntax,
    };
    return def;
}

test "board.toml parses and validates" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const doc =
        \\version = 1
        \\
        \\[sort]
        \\by = "ts"
        \\direction = "desc"
        \\
        \\[[column]]
        \\name = "review"
        \\status = "needs-review"
        \\
        \\[[column]]
        \\name = "mergeable"
        \\kind = "patch"
        \\merge = "allowed"
        \\
        \\[[column]]
        \\name = "suspect"
        \\unverified = true
        \\
        \\[[column]]
        \\name = "everything else"
    ;
    const def = try parse_board_def(arena, doc);
    try std.testing.expectEqual(@as(usize, 4), def.columns.len);
    try std.testing.expectEqual(collab.BoardSortBy.ts, def.sort.by);
    try std.testing.expectEqual(collab.BoardSortDirection.desc, def.sort.direction);
    try std.testing.expectEqualStrings("patch", def.columns[1].kind);
    try std.testing.expect(def.columns[2].unverified);

    // Fail closed on every broken definition.
    try std.testing.expectError(error.BadVersion, parse_board_def(arena, "version = 2\n[[column]]\nname = \"x\"\n"));
    try std.testing.expectError(error.MissingField, parse_board_def(arena, "[sort]\n[[column]]\nname = \"x\"\n"));
    try std.testing.expectError(error.NoColumns, parse_board_def(arena, "version = 1\n"));
    try std.testing.expectError(error.EmptyName, parse_board_def(arena, "version = 1\n[[column]]\nname = \"\"\n"));
    try std.testing.expectError(error.DuplicateColumn, parse_board_def(arena, "version = 1\n[[column]]\nname = \"a\"\n[[column]]\nname = \"a\"\n"));
    try std.testing.expectError(error.BadMergeVerdict, parse_board_def(arena, "version = 1\n[[column]]\nname = \"a\"\nmerge = \"maybe\"\n"));
    // A typo must not silently no-op (deny_unknown_fields).
    try std.testing.expectError(error.UnknownField, parse_board_def(arena, "version = 1\n[[column]]\nname = \"a\"\nstatuz = \"open\"\n"));
    try std.testing.expectError(error.UnknownField, parse_board_def(arena, "version = 1\nsudt = 1\n[[column]]\nname = \"a\"\n"));
}
