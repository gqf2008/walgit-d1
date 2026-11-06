//! Small shared helpers: hex/base64, wall clock, HTML/markdown escaping,
//! process exits.

const std = @import("std");

/// The client version — keep in step with `build.zig.zon`.
pub const version = "0.1.0";

/// A managed growable list — the allocator-carried shape the 0.17 std
/// `ArrayList` dropped. One wrapper keeps the whole port readable. `items`
/// is always the active prefix of `buffer`.
pub fn List(comptime T: type) type {
    return struct {
        const Self = @This();
        items: []T = &.{},
        buffer: []T = &.{},
        alloc: std.mem.Allocator,

        pub fn init(alloc: std.mem.Allocator) Self {
            return .{ .alloc = alloc };
        }

        pub fn deinit(self: *Self) void {
            if (self.buffer.len != 0) self.alloc.free(self.buffer);
            self.* = .{ .alloc = self.alloc };
        }

        pub fn append(self: *Self, item: T) !void {
            if (self.items.len == self.buffer.len) try self.grow_to(self.items.len + 8);
            self.buffer[self.items.len] = item;
            self.items = self.buffer[0 .. self.items.len + 1];
        }

        pub fn append_slice(self: *Self, slice: []const T) !void {
            if (self.items.len + slice.len > self.buffer.len) {
                try self.grow_to(self.items.len + slice.len);
            }
            @memcpy(self.buffer[self.items.len..][0..slice.len], slice);
            self.items = self.buffer[0 .. self.items.len + slice.len];
        }

        fn grow_to(self: *Self, new_len: usize) !void {
            var new_cap = self.buffer.len;
            if (new_cap == 0) new_cap = 8;
            while (new_cap < new_len) new_cap += new_cap / 2 + 8;
            const new_mem = try self.alloc.alloc(T, new_cap);
            @memcpy(new_mem[0..self.items.len], self.items);
            if (self.buffer.len != 0) self.alloc.free(self.buffer);
            self.buffer = new_mem;
            self.items = new_mem[0..self.items.len];
        }

        pub fn to_owned_slice(self: *Self) ![]T {
            const out = try self.alloc.alloc(T, self.items.len);
            @memcpy(out, self.items);
            self.deinit();
            return out;
        }
    };
}

pub fn to_hex(alloc: std.mem.Allocator, bytes: []const u8) []u8 {
    const out = alloc.alloc(u8, bytes.len * 2) catch @panic("oom");
    const hexd = "0123456789abcdef";
    for (bytes, 0..) |b, i| {
        out[i * 2] = hexd[b >> 4];
        out[i * 2 + 1] = hexd[b & 0xf];
    }
    return out;
}

pub fn from_hex(alloc: std.mem.Allocator, s: []const u8) ![]u8 {
    if (s.len % 2 != 0) return error.OddLength;
    const out = try alloc.alloc(u8, s.len / 2);
    errdefer alloc.free(out);
    for (s, 0..) |c, i| {
        const v: u8 = switch (c) {
            '0'...'9' => c - '0',
            'a'...'f' => c - 'a' + 10,
            'A'...'F' => c - 'A' + 10,
            else => return error.InvalidHex,
        };
        if (i % 2 == 0) {
            out[i / 2] = v << 4;
        } else {
            out[i / 2] |= v;
        }
    }
    return out;
}

pub const B64 = std.base64.standard.Encoder;
pub const B64D = std.base64.standard.Decoder;

pub fn b64_encode(alloc: std.mem.Allocator, bytes: []const u8) []u8 {
    const out = alloc.alloc(u8, B64.calcSize(bytes.len)) catch @panic("oom");
    _ = B64.encode(out, bytes);
    return out;
}

pub fn b64_decode(alloc: std.mem.Allocator, s: []const u8) ![]u8 {
    const n = B64D.calcSizeForSlice(s) catch return error.InvalidBase64;
    const out = try alloc.alloc(u8, n);
    B64D.decode(out, s) catch {
        alloc.free(out);
        return error.InvalidBase64;
    };
    return out;
}

/// Unix seconds — the entry `ts` clock. The wall clock is process-global;
/// the Threaded driver here backs `std.time.timestamp`'s replacement.
var clock_threaded: ?std.Io.Threaded = null;
pub fn now_ts() i64 {
    if (clock_threaded == null) {
        clock_threaded = std.Io.Threaded.init(std.heap.page_allocator, .{});
    }
    const ts = std.Io.Timestamp.now(clock_threaded.?.io(), .real);
    return @intCast(@divTrunc(ts.nanoseconds, std.time.ns_per_s));
}

pub fn esc(alloc: std.mem.Allocator, s: []const u8) []u8 {
    var out: List(u8) = .init(alloc);
    for (s) |c| {
        switch (c) {
            '&' => out.append_slice("&amp;") catch @panic("oom"),
            '<' => out.append_slice("&lt;") catch @panic("oom"),
            '>' => out.append_slice("&gt;") catch @panic("oom"),
            else => out.append(c) catch @panic("oom"),
        }
    }
    return out.to_owned_slice() catch @panic("oom");
}

/// Markdown table cell: HTML-escape, then `|` → `\|`, newlines → `<br>`
pub fn md_cell(alloc: std.mem.Allocator, s: []const u8) []u8 {
    const escaped = esc(alloc, s);
    defer alloc.free(escaped);
    var out: List(u8) = .init(alloc);
    var i: usize = 0;
    while (i < escaped.len) {
        if (escaped[i] == '|') {
            out.append_slice("\\|") catch @panic("oom");
            i += 1;
        } else if (i + 1 < escaped.len and escaped[i] == '\r' and escaped[i + 1] == '\n') {
            out.append_slice("<br>") catch @panic("oom");
            i += 2;
        } else if (escaped[i] == '\r' or escaped[i] == '\n') {
            out.append_slice("<br>") catch @panic("oom");
            i += 1;
        } else {
            out.append(escaped[i]) catch @panic("oom");
            i += 1;
        }
    }
    return out.to_owned_slice() catch @panic("oom");
}

/// Print a message to stderr and exit with a non-zero code.
pub fn fatal(comptime fmt: []const u8, args: anytype) noreturn {
    var buf: [4096]u8 = undefined;
    const msg = std.fmt.bufPrint(&buf, fmt, args) catch fmt;
    std.debug.print("wg: {s}\n", .{msg});
    std.process.exit(1);
}

test "hex round trip" {
    const a = std.testing.allocator;
    const h = to_hex(a, &[_]u8{ 0x0a, 0xff, 0x01 });
    defer a.free(h);
    try std.testing.expectEqualStrings("0aff01", h);
    const back = try from_hex(a, h);
    defer a.free(back);
    try std.testing.expectEqualSlices(u8, &[_]u8{ 0x0a, 0xff, 0x01 }, back);
}

test "b64 round trip" {
    const a = std.testing.allocator;
    const enc = b64_encode(a, "hello");
    defer a.free(enc);
    try std.testing.expectEqualStrings("aGVsbG8=", enc);
    const dec = try b64_decode(a, enc);
    defer a.free(dec);
    try std.testing.expectEqualStrings("hello", dec);
}
