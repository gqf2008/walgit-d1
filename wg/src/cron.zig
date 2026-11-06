//! Cron schedule parsing and due evaluation — the port of
//! `walgit-bundle/src/schedule.rs` over the `cron` crate's supported subset:
//! 6/7-field UTC expressions (`sec min hour dom mon dow [year]`) with `*`,
//! `*/n`, ranges, lists and name aliases, plus `@hourly`/`@daily`/`@weekly`/
//! `@monthly`/`@yearly`. The runner uses `due_slot` (D1-CI §4.3): missed
//! slots fold into the newest due one, never a replay of history.

const std = @import("std");
const util = @import("util.zig");

pub const ParseError = error{InvalidSchedule};

/// One field's allowed values, as a sorted bitset over its domain.
pub const Field = struct {
    bits: []bool, // indexed by (value - offset)
    offset: u16,
    is_star: bool,

    pub fn contains(self: *const Field, v: u16) bool {
        if (v < self.offset) return false;
        const idx = v - self.offset;
        if (idx >= self.bits.len) return false;
        return self.bits[idx];
    }

    pub fn next(self: *const Field, from: u16) ?u16 {
        var v: u16 = from;
        while (true) {
            if (!self.contains(v)) {
                if (v - self.offset >= self.bits.len) return null;
                v += 1;
                continue;
            }
            return v;
        }
    }
};

pub const Schedule = struct {
    secs: Field,
    mins: Field,
    hours: Field,
    dom: Field,
    mons: Field,
    dows: Field, // 0..7 (both 0 and 7 are Sunday)
    years: ?Field = null, // indexed by (year - 1970)
    dom_restricted: bool,
    dow_restricted: bool,

    /// Whether a civil day matches, applying the standard vixie rule: when
    /// BOTH dom and dow are restricted, either may match; otherwise the
    /// restricted field must.
    fn day_matches(self: *const Schedule, dom: u16, dow0: u16) bool {
        const dow: u16 = @intCast(dow0 % 7);
        const dom_ok = self.dom.contains(dom);
        const dow_ok = self.dows.contains(dow);
        if (self.dom_restricted and self.dow_restricted) return dom_ok or dow_ok;
        if (self.dom_restricted) return dom_ok;
        if (self.dow_restricted) return dow_ok;
        return true;
    }
};

fn build_field(arena: std.mem.Allocator, offset: u16, size: usize, is_star: bool) ParseError!Field {
    const bits = arena.alloc(bool, size) catch return error.InvalidSchedule; // oom is fatal above us
    @memset(bits, false);
    return .{ .bits = bits, .offset = offset, .is_star = is_star };
}

fn set_bit(f: *Field, v: u16) !void {
    if (v < f.offset) return error.InvalidSchedule;
    const idx: usize = v - f.offset;
    if (idx >= f.bits.len) return error.InvalidSchedule;
    f.bits[idx] = true;
}

const month_names = [_][]const u8{ "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec" };
const dow_names = [_][]const u8{ "sun", "mon", "tue", "wed", "thu", "fri", "sat" };

fn parse_atom(spec: []const u8) ParseError!u16 {
    // Names (case-insensitive prefix, like the cron crate).
    for (month_names, 0..) |name, i| {
        if (spec.len >= 3 and std.ascii.eqlIgnoreCase(spec[0..3], name)) return @intCast(i + 1);
    }
    for (dow_names, 0..) |name, i| {
        if (spec.len >= 3 and std.ascii.eqlIgnoreCase(spec[0..3], name)) return @intCast(i);
    }
    return std.fmt.parseInt(u16, spec, 10) catch error.InvalidSchedule;
}

fn parse_field(arena: std.mem.Allocator, spec_in: []const u8, comptime kind: anytype, min_v: u16, max_v: u16) ParseError!Field {
    _ = kind;
    const size: usize = @as(usize, max_v - min_v) + 1;
    var f = try build_field(arena, min_v, size, false);
    var is_star = false;
    var parts = std.mem.splitScalar(u8, spec_in, ',');
    var any = false;
    while (parts.next()) |part_raw| {
        const part = std.mem.trim(u8, part_raw, " \t");
        if (part.len == 0) return error.InvalidSchedule;
        // `*`, `*/n`, `a`, `a-b`, `a-b/n`, `a/n`
        var range_spec = part;
        var step: u16 = 1;
        if (std.mem.indexOfScalar(u8, part, '/')) |slash| {
            range_spec = part[0..slash];
            step = std.fmt.parseInt(u16, part[slash + 1 ..], 10) catch return error.InvalidSchedule;
            if (step == 0) return error.InvalidSchedule;
        }
        var lo: u16 = min_v;
        var hi: u16 = max_v;
        if (std.mem.eql(u8, range_spec, "*")) {
            is_star = true;
        } else if (std.mem.indexOfScalar(u8, range_spec, '-')) |dash| {
            lo = try parse_atom(range_spec[0..dash]);
            hi = try parse_atom(range_spec[dash + 1 ..]);
        } else {
            lo = try parse_atom(range_spec);
            hi = lo;
            if (std.mem.indexOfScalar(u8, part, '/') != null) {
                // `a/n` runs from `a` to the domain's end.
                hi = max_v;
            }
        }
        if (lo < min_v or hi > max_v or lo > hi) return error.InvalidSchedule;
        var v: u16 = lo;
        while (true) {
            try set_bit(&f, v);
            if (v == hi or v + step > hi) break;
            v += step;
        }
        any = true;
    }
    if (!any) return error.InvalidSchedule;
    f.is_star = is_star;
    return f;
}

/// The dow field's 7 aliases to Sunday; expressed by normalizing at parse
/// time through this dedicated function.
fn parse_dow_field(arena: std.mem.Allocator, spec: []const u8) ParseError!Field {
    var f = try build_field(arena, 0, 8, false); // 0..7
    var parts = std.mem.splitScalar(u8, spec, ',');
    var is_star = false;
    while (parts.next()) |part_raw| {
        const part = std.mem.trim(u8, part_raw, " \t");
        if (part.len == 0) return error.InvalidSchedule;
        var range_spec = part;
        var step: u16 = 1;
        if (std.mem.indexOfScalar(u8, part, '/')) |slash| {
            range_spec = part[0..slash];
            step = std.fmt.parseInt(u16, part[slash + 1 ..], 10) catch return error.InvalidSchedule;
            if (step == 0) return error.InvalidSchedule;
        }
        var lo: u16 = 0;
        var hi: u16 = 6;
        if (std.mem.eql(u8, range_spec, "*")) {
            is_star = true;
        } else if (std.mem.indexOfScalar(u8, range_spec, '-')) |dash| {
            lo = try parse_atom(range_spec[0..dash]);
            hi = try parse_atom(range_spec[dash + 1 ..]);
        } else {
            lo = try parse_atom(range_spec);
            hi = lo;
            if (std.mem.indexOfScalar(u8, part, '/') != null) hi = 6;
        }
        if (hi == 7) hi = 0; // 7 is Sunday
        if (lo > 7 or hi > 7 or lo > hi) return error.InvalidSchedule;
        var v: u16 = lo;
        while (true) {
            f.bits[v] = true;
            if (v == hi or v + step > hi) break;
            v += step;
        }
    }
    f.is_star = is_star;
    return f;
}

/// Parse a cron expression or shorthand alias, allocating the field bitsets
/// from `arena`. The year field is optional (7 fields); `L`/`#`/`?` are
/// rejected — the runner only schedules what it can evaluate.
pub fn parse_schedule_a(arena: std.mem.Allocator, expr_in: []const u8) ParseError!Schedule {
    const expr = std.mem.trim(u8, expr_in, " \t\r\n");
    var body: []const u8 = expr;
    if (expr.len == 0) return error.InvalidSchedule;
    if (expr[0] == '@') {
        if (std.ascii.eqlIgnoreCase(expr, "@hourly")) {
            body = "0 0 * * * *";
        } else if (std.ascii.eqlIgnoreCase(expr, "@daily") or std.ascii.eqlIgnoreCase(expr, "@midnight")) {
            body = "0 0 0 * * *";
        } else if (std.ascii.eqlIgnoreCase(expr, "@weekly")) {
            body = "0 0 0 * * 0";
        } else if (std.ascii.eqlIgnoreCase(expr, "@monthly")) {
            body = "0 0 0 1 * *";
        } else if (std.ascii.eqlIgnoreCase(expr, "@yearly") or std.ascii.eqlIgnoreCase(expr, "@annually")) {
            body = "0 0 0 1 1 *";
        } else {
            return error.InvalidSchedule;
        }
    }
    var fields: util.List([]const u8) = util.List([]const u8).init(arena);
    var it = std.mem.tokenizeAny(u8, body, " \t");
    while (it.next()) |tok| {
        fields.append(tok) catch return error.InvalidSchedule; // oom surfaces as a rejected expression here
    }
    if (fields.items.len != 6 and fields.items.len != 7) return error.InvalidSchedule;

    // Seconds.
    const secs_f = parse_field(arena, fields.items[0], .second, 0, 59) catch return error.InvalidSchedule;
    const mins = parse_field(arena, fields.items[1], .minute, 0, 59) catch return error.InvalidSchedule;
    const hours = parse_field(arena, fields.items[2], .hour, 0, 23) catch return error.InvalidSchedule;
    const dom_spec = fields.items[3];
    const dom_restricted = !std.mem.eql(u8, dom_spec, "*") and !std.mem.startsWith(u8, dom_spec, "*/");
    const dom = parse_field(arena, dom_spec, .dom, 1, 31) catch return error.InvalidSchedule;
    const mons = parse_field(arena, fields.items[4], .month, 1, 12) catch return error.InvalidSchedule;
    const dow_spec = fields.items[5];
    const dow_restricted = !std.mem.eql(u8, dow_spec, "*") and !std.mem.startsWith(u8, dow_spec, "*/");
    const dows = parse_dow_field(arena, dow_spec) catch return error.InvalidSchedule;
    var years: ?Field = null;
    if (fields.items.len == 7) {
        years = parse_field(arena, fields.items[6], .year, 1970, 2262) catch return error.InvalidSchedule;
    }
    return .{
        .secs = secs_f,
        .mins = mins,
        .hours = hours,
        .dom = dom,
        .mons = mons,
        .dows = dows,
        .years = years,
        .dom_restricted = dom_restricted,
        .dow_restricted = dow_restricted,
    };
}

// ---- civil calendar (Howard Hinnant's algorithms) --------------------------------

/// Days since 1970-01-01 for a civil date.
pub fn days_from_civil(y_in: i64, m: u16, d: u16) i64 {
    var y = y_in;
    y -= if (m <= 2) @as(i64, 1) else 0;
    const era = @divFloor(if (y >= 0) y else y - 399, 400);
    const yoe: u64 = @intCast(y - era * 400); // [0, 399]
    const mp: u64 = @intCast((@as(u64, m) + 9) % 12); // [0, 11]
    const doy: u64 = @divFloor(153 * mp + 2, 5) + @as(u64, d) - 1; // [0, 365]
    const doe: u64 = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    return era * 146097 + @as(i64, @intCast(doe)) - 719468;
}

/// Civil date from days since 1970-01-01.
pub fn civil_from_days(z_in: i64) struct { y: i64, m: u16, d: u16 } {
    const z = z_in + 719468;
    const era = @divFloor(if (z >= 0) z else z - 146096, 146097);
    const doe: u64 = @intCast(z - era * 146097); // [0, 146096]
    const yoe: u64 = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    const y: i64 = @as(i64, @intCast(yoe)) + era * 400;
    const doy: u64 = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    const mp: u64 = (5 * doy + 2) / 153; // [0, 11]
    const d: u16 = @intCast(doy - (153 * mp + 2) / 5 + 1); // [1, 31]
    const m: u16 = @intCast(if (mp < 10) mp + 3 else mp - 9); // [1, 12]
    return .{ .y = y + (if (m <= 2) @as(i64, 1) else 0), .m = m, .d = d };
}

/// Weekday index with 0 = Sunday for a days-since-epoch count
/// (1970-01-01 was a Thursday).
fn weekday0(days: i64) u16 {
    const w = @mod(days + 4, 7);
    return @intCast(w);
}

const Civil = struct { year: i64, mon: u16, day: u16, hour: u16, min: u16, sec: u16 };

fn civil_from_epoch(epoch_secs: i64) Civil {
    const days = @divFloor(epoch_secs, 86400);
    const rem = @mod(epoch_secs, 86400);
    const c = civil_from_days(days);
    return .{
        .year = c.y,
        .mon = c.m,
        .day = c.d,
        .hour = @intCast(@divFloor(rem, 3600)),
        .min = @intCast(@mod(@divFloor(rem, 60), 60)),
        .sec = @intCast(@mod(rem, 60)),
    };
}

fn epoch_from_civil(c: Civil) i64 {
    return days_from_civil(c.year, c.mon, c.day) * 86400 +
        @as(i64, c.hour) * 3600 + @as(i64, c.min) * 60 + @as(i64, c.sec);
}

const MAX_DAY_SCAN: i64 = 366 * 30; // ~30 years of day-walking

/// Next fire time of `schedule` strictly after `after`, in Unix seconds, or
/// null when the schedule is exhausted (bounded year range passed).
pub fn next_fire_after(schedule: *const Schedule, after: i64) ?i64 {
    var t = after + 1;
    var c = civil_from_epoch(t);
    var walked: i64 = 0;
    while (walked < MAX_DAY_SCAN) : (walked += 1) {
        const days = days_from_civil(c.year, c.mon, c.day);
        if (schedule.years) |*yf| {
            if (!yf.contains(@intCast(c.year - 1970))) {
                // Jump to Jan 1 of the next year.
                const jan1 = Civil{ .year = c.year + 1, .mon = 1, .day = 1, .hour = 0, .min = 0, .sec = 0 };
                t = epoch_from_civil(jan1);
                if (t <= after) {
                    c = civil_from_epoch(t);
                    continue;
                }
                c = jan1;
                continue;
            }
        }
        if (!schedule.mons.contains(c.mon)) {
            // Jump to the 1st of the next month, 00:00.
            var y = c.year;
            var m: u16 = c.mon + 1;
            if (m > 12) {
                m = 1;
                y += 1;
            }
            c = .{ .year = y, .mon = m, .day = 1, .hour = 0, .min = 0, .sec = 0 };
            t = epoch_from_civil(c);
            continue;
        }
        if (!schedule.day_matches(c.day, weekday0(days))) {
            c = .{ .year = c.year, .mon = c.mon, .day = c.day + 1, .hour = 0, .min = 0, .sec = 0 };
            // Normalize overflowed day (day 32 etc.) through epoch round-trip.
            t = epoch_from_civil(c);
            c = civil_from_epoch(t);
            continue;
        }
        // The day matches; find the first allowed (h, m, s) at-or-after the
        // current time of day.
        var h = c.hour;
        while (h <= 23) : (h += 1) {
            if (!schedule.hours.contains(h)) continue;
            var mi: u16 = if (h == c.hour) c.min else 0;
            while (mi <= 59) : (mi += 1) {
                if (!schedule.mins.contains(mi)) continue;
                var s: u16 = if (h == c.hour and mi == c.min) c.sec else 0;
                while (s <= 59) : (s += 1) {
                    if (!schedule.secs.contains(s)) continue;
                    const candidate = epoch_from_civil(.{ .year = c.year, .mon = c.mon, .day = c.day, .hour = h, .min = mi, .sec = s });
                    if (candidate > after) return candidate;
                    continue;
                }
            }
        }
        // Nothing left today; move to tomorrow 00:00.
        c = .{ .year = c.year, .mon = c.mon, .day = c.day + 1, .hour = 0, .min = 0, .sec = 0 };
        t = epoch_from_civil(c);
        c = civil_from_epoch(t);
    }
    return null;
}

/// What a periodic trigger owes at `now`, given the last slot it processed.
/// At most `COALESCE_SCAN_MAX` fires are walked per evaluation; a deeper
/// backlog advances the bookkeeping without firing.
pub const COALESCE_SCAN_MAX: u32 = 4096;

pub const DueSlot = union(enum) {
    /// No fire in `(last_slot, now]`.
    not_due,
    /// The newest fire in `(last_slot, now]` — run exactly this slot.
    run: i64,
    /// Fires remain due even past this one (backlog deeper than the scan
    /// bound): advance the bookkeeping to this slot without running.
    skip: i64,
};

pub fn due_slot(schedule: *const Schedule, last_slot: i64, now: i64) DueSlot {
    var latest: ?i64 = null;
    var steps: u32 = 0;
    var fire = next_fire_after(schedule, last_slot);
    while (fire) |f| {
        if (f > now) break;
        latest = f;
        steps += 1;
        if (steps >= COALESCE_SCAN_MAX) break;
        fire = next_fire_after(schedule, f);
    }
    const slot = latest orelse return .not_due;
    // Is anything still due past this slot? Then this pass only advances the
    // bookkeeping.
    const after_slot = next_fire_after(schedule, slot);
    if (after_slot != null and after_slot.? <= now) {
        return .{ .skip = slot };
    }
    return .{ .run = slot };
}

// ---- tests -----------------------------------------------------------------------

const testing = std.testing;

fn schedule_for(arena: std.mem.Allocator, expr: []const u8) !Schedule {
    return parse_schedule_a(arena, expr);
}

fn epoch(y: i64, mo: u16, d: u16, h: u16, mi: u16, s: u16) i64 {
    return epoch_from_civil(.{ .year = y, .mon = mo, .day = d, .hour = h, .min = mi, .sec = s });
}

test "aliases and field syntax parse; junk fails" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    _ = try schedule_for(arena, "@hourly");
    _ = try schedule_for(arena, "@daily");
    _ = try schedule_for(arena, "@weekly");
    _ = try schedule_for(arena, "@monthly");
    _ = try schedule_for(arena, "@yearly");
    _ = try schedule_for(arena, "0 */5 * * * *");
    _ = try schedule_for(arena, "30 4 1,15 * 1-5 *");
    _ = try schedule_for(arena, "0 0 12 * jan sun");
    // 5-field expressions are not this dialect.
    try testing.expectError(error.InvalidSchedule, schedule_for(arena, "* * * * *"));
    try testing.expectError(error.InvalidSchedule, schedule_for(arena, "60 * * * * *"));
    try testing.expectError(error.InvalidSchedule, schedule_for(arena, ""));
    try testing.expectError(error.InvalidSchedule, schedule_for(arena, "@never"));
}

test "next fire: every five seconds, daily aliases" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();

    const every5 = try schedule_for(arena, "*/5 * * * * *");
    const t = epoch(2026, 11, 6, 12, 0, 2);
    const next = next_fire_after(&every5, t).?;
    const c = civil_from_epoch(next);
    try testing.expectEqual(@as(u16, 12), c.hour);
    try testing.expectEqual(@as(u16, 0), c.min);
    try testing.expectEqual(@as(u16, 5), c.sec);

    const daily = try schedule_for(arena, "@daily");
    const next2 = next_fire_after(&daily, epoch(2026, 11, 6, 23, 30, 0)).?;
    const c2 = civil_from_epoch(next2);
    try testing.expectEqual(@as(i64, 2026), c2.year);
    try testing.expectEqual(@as(u16, 11), c2.mon);
    try testing.expectEqual(@as(u16, 7), c2.day);
    try testing.expectEqual(@as(u16, 0), c2.hour);

    // Weekly fires on Sunday.
    const weekly = try schedule_for(arena, "@weekly");
    const next3 = next_fire_after(&weekly, epoch(2026, 11, 6, 1, 0, 0)).?; // Friday
    const c3 = civil_from_epoch(next3);
    try testing.expectEqual(@as(u16, 0), weekday0(days_from_civil(c3.year, c3.mon, c3.day)));
    try testing.expectEqual(@as(u16, 8), c3.day); // the following Sunday
}

test "due slot coalesces a backlog into the newest due fire" {
    var arena_state = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();
    const every5 = try schedule_for(arena, "*/5 * * * * *");

    // Nothing due in (last, now].
    const now = epoch(2026, 11, 6, 12, 0, 2);
    try expectDue(.not_due, due_slot(&every5, now - 1, now));
    // Fires land on 5s boundaries; at :02 the last one due is :00.
    const t0 = epoch(2026, 11, 6, 12, 0, 0);
    const d = due_slot(&every5, now - 3, now);
    try testing.expect(d == .run);
    try testing.expectEqual(t0, d.run);
    // A long backlog folds into the newest due slot... but the newest due
    // is also the LAST fire ≤ now, and nothing is due past it → run.
    const backlogged = due_slot(&every5, now - 600, now);
    try testing.expect(backlogged == .run);
    try testing.expectEqual(t0, backlogged.run); // fires land on 5s boundaries
}

fn expectDue(want: DueSlot, got: DueSlot) !void {
    errdefer {}
    if (want == .not_due and got == .not_due) return;
    if (want == .run and got == .run and want.run == got.run) return;
    if (want == .skip and got == .skip and want.skip == got.skip) return;
    return error.TestExpectedEqual;
}
