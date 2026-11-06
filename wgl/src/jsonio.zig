//! JSON serialization for wgl, byte-compatible with serde_json's output.
//!
//! Two output shapes exist in the retired Rust implementation and both are
//! reproduced here (`docs/D1_PROTOCOL.md` §8.2: the wire order is part of the
//! contract):
//!
//! - **Sorted-map mode** — what `serde_json::json!{..}` produces (the default
//!   `Map` is a `BTreeMap`): every object's keys are emitted in ascending byte
//!   order at every depth. Used for ad-hoc documents (claim output, principal
//!   registry blobs, thread view objects) and for every `body` value, which
//!   Rust re-serialized through the same `BTreeMap`.
//! - **Struct mode** — a typed struct's field declaration order (entry fields,
//!   board cards, report rows). Callers hand-emit those fields and delegate
//!   inner values to the writers here.
//!
//! Whitespace follows `to_string` (none) and `to_string_pretty` (two spaces,
//! `"key": value`, empty containers as `{}` / `[]`).

const std = @import("std");
const jval = std.json;
const util = @import("util.zig");

pub const Value = jval.Value;

/// A growable byte buffer.
pub const Out = util.List(u8);

pub fn put_raw(out: *Out, bytes: []const u8) void {
    out.append_slice(bytes) catch @panic("oom");
}

pub fn putc(out: *Out, byte: u8) void {
    out.append(byte) catch @panic("oom");
}

pub fn newline_indent_pub(out: *Out, n: usize) void {
    putc(out, '\n');
    for (0..n) |_| put_raw(out, "  ");
}

/// Escape a string exactly like serde_json: only `"`, `\` and the control
/// characters expand (short forms for \b \t \n \f \r, `\u00XX` for the rest);
/// everything else stays raw UTF-8.
pub fn write_string(out: *Out, s: []const u8) void {
    putc(out, '"');
    for (s) |c| {
        switch (c) {
            '"' => put_raw(out, "\\\""),
            '\\' => put_raw(out, "\\\\"),
            0x08 => put_raw(out, "\\b"),
            0x09 => put_raw(out, "\\t"),
            0x0A => put_raw(out, "\\n"),
            0x0C => put_raw(out, "\\f"),
            0x0D => put_raw(out, "\\r"),
            else => {
                if (c < 0x20) {
                    put_raw(out, "\\u00");
                    const hexd = "0123456789abcdef";
                    putc(out, hexd[c >> 4]);
                    putc(out, hexd[c & 0xf]);
                } else {
                    putc(out, c);
                }
            },
        }
    }
    putc(out, '"');
}

fn write_int(out: *Out, i: i64) void {
    var buf: [32]u8 = undefined;
    put_raw(out, std.fmt.bufPrint(&buf, "{d}", .{i}) catch unreachable);
}

fn write_float(out: *Out, f: f64) void {
    var buf: [64]u8 = undefined;
    put_raw(out, std.fmt.bufPrint(&buf, "{d}", .{f}) catch unreachable);
}

fn key_less(_: void, a: []const u8, b: []const u8) bool {
    return std.mem.order(u8, a, b) == .lt;
}

/// Write one JSON value; objects are rendered with keys in ascending byte
/// order (serde_json's default `BTreeMap` behavior). `indent` = current
/// indentation width for pretty mode, `null` = compact.
pub fn write_value(out: *Out, v: Value, indent: ?usize) void {
    switch (v) {
        .null => put_raw(out, "null"),
        .bool => |b| put_raw(out, if (b) "true" else "false"),
        .integer => |i| write_int(out, i),
        .float => |f| write_float(out, f),
        .number_string => |s| put_raw(out, s),
        .string => |s| write_string(out, s),
        .array => |a| {
            if (a.items.len == 0) {
                put_raw(out, "[]");
                return;
            }
            putc(out, '[');
            for (a.items, 0..) |item, i| {
                if (i > 0) putc(out, ',');
                if (indent) |n| newline_indent_pub(out, n + 1);
                write_value(out, item, if (indent) |n| n + 1 else null);
            }
            if (indent) |n| newline_indent_pub(out, n);
            putc(out, ']');
        },
        .object => |o| {
            if (o.count() == 0) {
                put_raw(out, "{}");
                return;
            }
            putc(out, '{');
            var keys: util.List([]const u8) = .init(out.alloc);
            defer keys.deinit();
            var it = o.iterator();
            while (it.next()) |e| keys.append(e.key_ptr.*) catch @panic("oom");
            std.mem.sort([]const u8, keys.items, {}, key_less);
            for (keys.items, 0..) |k, i| {
                if (i > 0) putc(out, ',');
                if (indent) |n| newline_indent_pub(out, n + 1);
                write_string(out, k);
                putc(out, ':');
                if (indent != null) putc(out, ' ');
                write_value(out, o.get(k).?, if (indent) |n| n + 1 else null);
            }
            if (indent) |n| newline_indent_pub(out, n);
            putc(out, '}');
        },
    }
}

pub fn to_string(alloc: std.mem.Allocator, v: Value) []u8 {
    var out = Out.init(alloc);
    write_value(&out, v, null);
    return out.to_owned_slice() catch @panic("oom");
}

pub fn to_string_pretty(alloc: std.mem.Allocator, v: Value) []u8 {
    var out = Out.init(alloc);
    write_value(&out, v, 0);
    return out.to_owned_slice() catch @panic("oom");
}

// ---- object construction helpers ------------------------------------------------

/// Insert into an object value, copying `key` into `alloc`.
pub fn obj_put(o: *jval.ObjectMap, alloc: std.mem.Allocator, key: []const u8, v: Value) void {
    const k = alloc.dupe(u8, key) catch @panic("oom");
    o.put(alloc, k, v) catch @panic("oom");
}

/// A fresh, empty object (fields default; `deinit(alloc)` when done).
pub fn new_object() jval.ObjectMap {
    return .{};
}

pub fn str(alloc: std.mem.Allocator, s: []const u8) Value {
    return .{ .string = alloc.dupe(u8, s) catch @panic("oom") };
}

pub fn integer(i: i64) Value {
    return .{ .integer = i };
}

test "compact object keys are sorted like serde_json" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var o = new_object();
    defer o.deinit(alloc);
    obj_put(&o, alloc, "b", .{ .integer = 1 });
    obj_put(&o, alloc, "a", .{ .integer = 2 });
    const s = to_string(alloc, .{ .object = o });
    defer alloc.free(s);
    try std.testing.expectEqualStrings("{\"a\":2,\"b\":1}", s);
}

test "pretty uses two-space serde_json shape" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var inner = new_object();
    defer inner.deinit(alloc);
    obj_put(&inner, alloc, "t", .{ .string = "x" });
    var o = new_object();
    defer o.deinit(alloc);
    var arr = jval.Array.init(alloc);
    defer arr.deinit();
    arr.append(.{ .integer = 1 }) catch unreachable;
    arr.append(.{ .integer = 2 }) catch unreachable;
    obj_put(&o, alloc, "list", .{ .array = arr });
    obj_put(&o, alloc, "obj", .{ .object = inner });
    const s = to_string_pretty(alloc, .{ .object = o });
    defer alloc.free(s);
    try std.testing.expectEqualStrings(
        "{\n  \"list\": [\n    1,\n    2\n  ],\n  \"obj\": {\n    \"t\": \"x\"\n  }\n}",
        s,
    );
}

test "strings escape like serde_json" {
    var arena_state = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena_state.deinit();
    const alloc = arena_state.allocator();
    var o = new_object();
    defer o.deinit(alloc);
    obj_put(&o, alloc, "s", .{ .string = "a\"b\\c\nd\x01q" });
    const s = to_string(alloc, .{ .object = o });
    defer alloc.free(s);
    try std.testing.expectEqualStrings("{\"s\":\"a\\\"b\\\\c\\nd\\u0001q\"}", s);
}
