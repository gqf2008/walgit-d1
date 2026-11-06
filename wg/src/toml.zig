//! A minimal TOML subset parser — exactly what `.walgit/board.toml`
//! (`docs/D1_PROTOCOL.md` §8.1) and `.walgit/ci.toml` (D1-CI §3) use: bare
//! keys, `[table]`, `[[array-of-tables]]`, basic strings with escapes,
//! integers, booleans, and arrays of scalars. Comments and whitespace per
//! TOML. Everything else (inline tables, multi-line strings, dates, dotted
//! keys in values) is a parse error — both documents fail closed on a typo
//! (`deny_unknown_fields` is the consumer's job; `Table.get` + explicit key
//! checks give the same effect).

const std = @import("std");
const util = @import("util.zig");

pub const Value = union(enum) {
    string: []const u8,
    integer: i64,
    boolean: bool,
    array: []Value,
    table: *Table,
};

pub const Table = struct {
    keys: util.List([]const u8),
    vals: util.List(Value),
    arena: std.mem.Allocator,

    pub fn init(arena: std.mem.Allocator) Table {
        return .{
            .arena = arena,
            .keys = util.List([]const u8).init(arena),
            .vals = util.List(Value).init(arena),
        };
    }

    pub fn get(self: *const Table, key: []const u8) ?Value {
        for (self.keys.items, self.vals.items) |k, v| {
            if (std.mem.eql(u8, k, key)) return v;
        }
        return null;
    }

    fn put(self: *Table, key: []const u8, v: Value) void {
        // Re-assign of an existing key is only legal for [[array-of-tables]]
        // appends, which the parser handles before calling put.
        self.keys.append(self.arena.dupe(u8, key) catch @panic("oom")) catch @panic("oom");
        self.vals.append(v) catch @panic("oom");
    }

    fn table_at(self: *Table, key: []const u8) !*Table {
        if (self.get(key)) |v| switch (v) {
            .table => |t| return t,
            else => return error.DuplicateKey,
        };
        const t = try self.arena.create(Table);
        t.* = Table.init(self.arena);
        self.put(key, .{ .table = t });
        return t;
    }

    fn append_table_at(self: *Table, key: []const u8) !*Table {
        const t = try self.arena.create(Table);
        t.* = Table.init(self.arena);
        // [[x]] appends to the array; create it on first use.
        if (self.get(key)) |v| switch (v) {
            .array => |arr| {
                var list: util.List(Value) = util.List(Value).init(self.arena);
                try list.append_slice(arr);
                try list.append(.{ .table = t });
                // Replace in place.
                for (self.keys.items, 0..) |k, i| {
                    if (std.mem.eql(u8, k, key)) {
                        self.vals.items[i] = .{ .array = try list.to_owned_slice() };
                        return t;
                    }
                }
                return error.DuplicateKey;
            },
            else => return error.DuplicateKey,
        };
        const arr = try self.arena.alloc(Value, 1);
        arr[0] = .{ .table = t };
        self.put(key, .{ .array = arr });
        return t;
    }

    /// A table-valued array entry (`[[task]]` rows), or null when the key is
    /// absent / not an array of tables.
    pub fn tables(self: *const Table, key: []const u8) []Value {
        const v = self.get(key) orelse return &.{};
        return switch (v) {
            .array => |arr| arr,
            else => &.{},
        };
    }
};

pub const ParseError = error{
    Syntax,
    DuplicateKey,
    OutOfMemory,
};

/// Line-aware parser state for error messages.
pub fn parse(arena: std.mem.Allocator, text: []const u8) ParseError!*Table {
    const root = try arena.create(Table);
    root.* = Table.init(arena);
    var current: *Table = root;
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |raw_line| {
        const line = std.mem.trim(u8, raw_line, " \t\r");
        if (line.len == 0 or line[0] == '#') continue;
        if (line[0] == '[') {
            // [[array]] or [table]
            const array = std.mem.startsWith(u8, line, "[[");
            const close: []const u8 = if (array) "]]" else "]";
            const end = std.mem.indexOf(u8, line, close) orelse return error.Syntax;
            const name = std.mem.trim(u8, line[if (array) 2 else 1..end], " \t");
            if (name.len == 0 or std.mem.indexOfScalar(u8, name, '[') != null) return error.Syntax;
            current = root;
            var it = std.mem.splitScalar(u8, name, '.');
            var segs: [16][]const u8 = undefined;
            var n: usize = 0;
            while (it.next()) |seg| {
                if (n >= segs.len) return error.Syntax;
                segs[n] = std.mem.trim(u8, seg, " \t\"");
                if (segs[n].len == 0) return error.Syntax;
                n += 1;
            }
            for (segs[0 .. n - 1]) |seg| {
                current = current.table_at(seg) catch return error.DuplicateKey;
            }
            current = if (array)
                current.append_table_at(segs[n - 1]) catch return error.DuplicateKey
            else
                current.table_at(segs[n - 1]) catch return error.DuplicateKey;
            continue;
        }
        // key = value
        const eq = std.mem.indexOfScalar(u8, line, '=') orelse return error.Syntax;
        const key = std.mem.trim(u8, strip_comment_suffix_key(line[0..eq]), " \t\"");
        if (key.len == 0 or std.mem.indexOfAny(u8, key, ".=") != null) return error.Syntax;
        const rest = std.mem.trim(u8, line[eq + 1 ..], " \t");
        const value = try parse_value(arena, rest);
        if (current.get(key) != null) return error.DuplicateKey;
        current.put(key, value);
    }
    return root;
}

/// Keys never carry trailing comments; a `#` inside a quoted key would
/// already have been stripped with the quotes, so plain trimming suffices.
fn strip_comment_suffix_key(s: []const u8) []const u8 {
    return s;
}

fn parse_value(arena: std.mem.Allocator, rest: []const u8) ParseError!Value {
    if (rest.len == 0) return error.Syntax;
    switch (rest[0]) {
        '"' => {
            if (std.mem.startsWith(u8, rest, "\"\"\"")) return error.Syntax; // no multi-line
            var out: util.List(u8) = .init(arena);
            var i: usize = 1;
            while (i < rest.len) {
                const c = rest[i];
                if (c == '"') {
                    // Must be the end; anything after (except whitespace/comment) is a syntax error.
                    const tail = std.mem.trim(u8, rest[i + 1 ..], " \t");
                    if (tail.len != 0 and tail[0] != '#') return error.Syntax;
                    return .{ .string = try out.to_owned_slice() };
                }
                if (c == '\\') {
                    i += 1;
                    if (i >= rest.len) return error.Syntax;
                    const esc = rest[i];
                    switch (esc) {
                        'b' => try out.append(0x08),
                        't' => try out.append('\t'),
                        'n' => try out.append('\n'),
                        'f' => try out.append(0x0C),
                        'r' => try out.append('\r'),
                        '"' => try out.append('"'),
                        '\\' => try out.append('\\'),
                        'u' => {
                            if (i + 4 >= rest.len) return error.Syntax;
                            const cp = std.fmt.parseInt(u21, rest[i + 1 .. i + 5], 16) catch return error.Syntax;
                            var buf: [4]u8 = undefined;
                            const n = std.unicode.utf8Encode(cp, &buf) catch return error.Syntax;
                            try out.append_slice(buf[0..n]);
                            i += 4;
                        },
                        else => return error.Syntax,
                    }
                    i += 1;
                    continue;
                }
                if (c == '#') return error.Syntax; // unterminated string
                try out.append(c);
                i += 1;
            }
            return error.Syntax; // unterminated
        },
        '[', '{' => {
            if (rest[0] == '{') return error.Syntax; // no inline tables
            return parse_array(arena, rest);
        },
        else => {
            const token = blk: {
                // Value ends at a comment or end of line.
                const hash = std.mem.indexOfScalar(u8, rest, '#');
                const raw = if (hash) |h| rest[0..h] else rest;
                break :blk std.mem.trim(u8, raw, " \t");
            };
            if (std.mem.eql(u8, token, "true")) return .{ .boolean = true };
            if (std.mem.eql(u8, token, "false")) return .{ .boolean = false };
            // Integer (allow underscores per TOML, no signs beyond minus).
            var cleaned: util.List(u8) = util.List(u8).init(arena);
            defer cleaned.deinit();
            for (token) |c| {
                if (c == '_') continue;
                cleaned.append(c) catch return error.OutOfMemory;
            }
            const s = cleaned.items;
            if (s.len == 0) return error.Syntax;
            const negative = s[0] == '-';
            const digits = if (negative) s[1..] else s;
            if (digits.len == 0) return error.Syntax;
            for (digits) |c| {
                if (!std.ascii.isDigit(c)) return error.Syntax;
            }
            const n = std.fmt.parseInt(i64, s, 10) catch return error.Syntax;
            return .{ .integer = n };
        },
    }
}

fn parse_array(arena: std.mem.Allocator, rest: []const u8) ParseError!Value {
    // Arrays of scalars only; elements separated by commas, may span the
    // token (single-line in our documents).
    var items: util.List(Value) = util.List(Value).init(arena);
    var depth: usize = 0;
    var start: usize = 1; // past the opening bracket
    var in_str = false;
    var i: usize = 0;
    while (i < rest.len) : (i += 1) {
        const c = rest[i];
        if (in_str) {
            if (c == '\\') {
                i += 1;
                continue;
            }
            if (c == '"') in_str = false;
            continue;
        }
        switch (c) {
            '"' => in_str = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if (depth == 0) {
                    const inner = std.mem.trim(u8, rest[start..i], " \t");
                    if (inner.len != 0) try items.append(try parse_scalar_item(arena, inner));
                    return .{ .array = try items.to_owned_slice() };
                }
            },
            ',' => {
                if (depth == 1) {
                    const inner = std.mem.trim(u8, rest[start..i], " \t");
                    if (inner.len != 0) try items.append(try parse_scalar_item(arena, inner));
                    start = i + 1;
                }
            },
            else => {},
        }
    }
    return error.Syntax;
}

fn parse_scalar_item(arena: std.mem.Allocator, token: []const u8) ParseError!Value {
    const v = try parse_value(arena, token);
    return switch (v) {
        .string, .integer, .boolean => v,
        else => error.Syntax,
    };
}

// ---- tests -----------------------------------------------------------------------

const testing = std.testing;

fn parse_in_arena(text: []const u8) !*Table {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    arena_state.deinit(); // keep the allocator alive via leak-on-purpose for the test scope
    return parse(std.heap.page_allocator, text);
}

test "board document parses" {
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
        \\name = "suspect"
        \\unverified = true
    ;
    const root = try parse_in_arena(doc);
    try testing.expectEqual(@as(i64, 1), root.get("version").?.integer);
    const sort = root.get("sort").?.table;
    try testing.expectEqualStrings("ts", sort.get("by").?.string);
    try testing.expectEqualStrings("desc", sort.get("direction").?.string);
    const cols = root.tables("column");
    try testing.expectEqual(@as(usize, 2), cols.len);
    try testing.expectEqualStrings("review", cols[0].table.get("name").?.string);
    try testing.expectEqualStrings("needs-review", cols[0].table.get("status").?.string);
    try testing.expect(cols[1].table.get("unverified").?.boolean);
    try testing.expect(cols[0].table.get("merge") == null);
}

test "ci document parses with arrays and comments" {
    const doc =
        \\# the pipeline
        \\version = 1
        \\claim_ttl = "5m"
        \\timeout = "10m"
        \\
        \\[[task]]
        \\name = "test"
        \\command = "just test"
        \\refs = ["refs/heads/*", "refs/tags/v*"]
        \\env_allow = ["CI", "PATH"]
        \\max_attempts = 2
    ;
    const root = try parse_in_arena(doc);
    try testing.expectEqualStrings("5m", root.get("claim_ttl").?.string);
    const tasks = root.tables("task");
    try testing.expectEqual(@as(usize, 1), tasks.len);
    const t = tasks[0].table;
    try testing.expectEqualStrings("test", t.get("name").?.string);
    try testing.expectEqualStrings("just test", t.get("command").?.string);
    const refs = t.get("refs").?.array;
    try testing.expectEqual(@as(usize, 2), refs.len);
    try testing.expectEqualStrings("refs/tags/v*", refs[1].string);
    try testing.expectEqual(@as(i64, 2), t.get("max_attempts").?.integer);
}

test "escapes and duplicates fail closed" {
    const root = try parse_in_arena("a = \"line\\nbreak\\t\\\"q\\\"\"");
    try testing.expectEqualStrings("line\nbreak\t\"q\"", root.get("a").?.string);
    try testing.expectError(error.DuplicateKey, parse_in_arena("a = 1\na = 2\n"));
    try testing.expectError(error.Syntax, parse_in_arena("a = { x = 1 }\n"));
    try testing.expectError(error.Syntax, parse_in_arena("version = 1\n[s\n"));
    try testing.expectError(error.Syntax, parse_in_arena("a = \"unterminated\n"));
    try testing.expectError(error.Syntax, parse_in_arena("a = tru\n"));
}
