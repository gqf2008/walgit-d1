//! Deterministic aggregation over `refs/collab/*` (`docs/D1_PROTOCOL.md`
//! §6/§7/§8): thread ordering, the PR view, merge rules, the report, the board
//! projection and work-card claims. Pure functions of
//! `(entries, principals, rules, now)` — every client holding the same refs
//! computes the same answer. Ported from `walgit-wal/src/collab.rs`.

const std = @import("std");
const jval = std.json;
const entry_mod = @import("entry.zig");
const ci_mod = @import("ci.zig");
const util = @import("../util.zig");

pub const EntryRef = entry_mod.EntryRef;
pub const Entry = entry_mod.Entry;
pub const Principals = entry_mod.Principals;
pub const Value = jval.Value;
pub const jsonio = @import("../jsonio.zig");

pub const CARD_CLAIM_KIND = entry_mod.CARD_CLAIM_KIND;
pub const CARD_CLAIM_TTL_DEFAULT_SECS: u64 = 300;
pub const CARD_CLAIM_TTL_MIN_SECS: u64 = ci_mod.CLAIM_TTL_MIN_SECS;
pub const CARD_CLAIM_TTL_MAX_SECS: u64 = ci_mod.CLAIM_TTL_MAX_SECS;

fn less3(ts_a: i64, actor_a: []const u8, oid_a: []const u8, ts_b: i64, actor_b: []const u8, oid_b: []const u8) bool {
    if (ts_a != ts_b) return ts_a < ts_b;
    const by_actor = std.mem.order(u8, actor_a, actor_b);
    if (by_actor != .eq) return by_actor == .lt;
    return std.mem.order(u8, oid_a, oid_b) == .lt;
}

fn entry_less(_: void, a: EntryRef, b: EntryRef) bool {
    return less3(a.entry.ts, a.entry.actor, a.oid, b.entry.ts, b.entry.actor, b.oid);
}

// ---- thread ordering (§6.2) -----------------------------------------------------------------

/// The deterministic order of one thread: sort by `(ts, actor, oid)`, then
/// emit parents before children round by round; a round with no progress
/// (malformed input) appends the remainder in sorted order. A dangling parent
/// counts as a root.
pub fn thread(alloc: std.mem.Allocator, entries: []const EntryRef) ![]EntryRef {
    var by_oid = std.StringHashMap(void).init(alloc);
    defer by_oid.deinit();
    for (entries) |e| try by_oid.put(e.oid, {});

    var pending = try alloc.alloc(EntryRef, entries.len);
    @memcpy(pending, entries);
    std.mem.sort(EntryRef, pending, {}, entry_less);

    var emitted = std.StringHashMap(void).init(alloc);
    defer emitted.deinit();

    var out = util.List(EntryRef).init(alloc);
    errdefer out.deinit();
    while (pending.len > 0) {
        const before = pending.len;
        var next = util.List(EntryRef).init(alloc);
        errdefer next.deinit();
        for (pending) |e| {
            const ready = e.entry.parent.len == 0 or
                !by_oid.contains(e.entry.parent) or
                emitted.contains(e.entry.parent);
            if (ready) {
                try emitted.put(e.oid, {});
                try out.append(e);
            } else {
                try next.append(e);
            }
        }
        alloc.free(pending);
        pending = try next.toOwnedSlice();
        if (pending.len == before) break;
    }
    try out.appendSlice(pending);
    alloc.free(pending);
    return try out.toOwnedSlice();
}

pub const ThreadHead = struct { id: []const u8, head: []const u8 };

/// `thread id -> tip oid` for many threads in one pass. Emitted in sorted-id
/// order (the retired Rust command serialized a `HashMap` whose order was
/// process-random; wgl is deterministic).
pub fn thread_head_map(alloc: std.mem.Allocator, entries: []const EntryRef) ![]ThreadHead {
    // group by id (sorted ids → deterministic output)
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
    var out = util.List(ThreadHead).init(alloc);
    errdefer out.deinit();
    for (ids.items) |id| {
        var group = util.List(EntryRef).init(alloc);
        defer group.deinit();
        for (entries) |e| {
            if (std.mem.eql(u8, e.entry.id, id)) try group.append(e);
        }
        const ordered = try thread(alloc, group.items);
        if (ordered.len > 0) {
            try out.append(.{ .id = id, .head = ordered[ordered.len - 1].oid });
        }
    }
    return try out.toOwnedSlice();
}

// ---- PR view (§7.2) ---------------------------------------------------------------------------

pub const Review = struct {
    actor: []const u8,
    decision: []const u8,
    ts: i64,
    oid: []const u8,
};

pub const PrView = struct {
    id: []const u8,
    base: ?[]const u8 = null,
    head: ?[]const u8 = null,
    status: []const u8 = "open",
    reviews: []Review,
    human_approvals: []Review,
    authors: [][]const u8,
    unverified: [][]const u8,
};

fn is_agent(actor: []const u8) bool {
    return std.mem.startsWith(u8, actor, "svc-");
}

pub fn pr_view(alloc: std.mem.Allocator, entries: []const EntryRef, principals: *const Principals) !PrView {
    const ordered = try thread(alloc, entries);
    var base: ?[]const u8 = null;
    var head: ?[]const u8 = null;
    var status: []const u8 = "open";
    var reviews = util.List(Review).init(alloc);
    var human_approvals = util.List(Review).init(alloc);
    var unverified = util.List([]const u8).init(alloc);
    var authors = util.List([]const u8).init(alloc);

    for (ordered) |r| {
        const e = &r.entry;
        const verified = r.is_verified(alloc, principals);
        if (std.mem.eql(u8, e.kind, "patch")) {
            if (e.refs) |rs| {
                if (base == null) base = rs.base;
                if (head == null) head = rs.head;
            }
            if (verified and !contains_str(authors.items, e.actor)) {
                try authors.append(e.actor);
            }
        }
        if (!verified) {
            const tag = try std.fmt.allocPrint(alloc, "{s}@{s}", .{ e.actor, r.oid });
            try unverified.append(tag);
        }
        if (std.mem.eql(u8, e.kind, "status")) {
            if (body_str(e.body, "status")) |st| {
                if (std.mem.eql(u8, st, "merged") or std.mem.eql(u8, st, "closed")) {
                    status = st;
                }
            }
        } else if (std.mem.eql(u8, e.kind, "merge_result")) {
            if (body_bool(e.body, "merged") orelse false) {
                status = "merged";
            }
        } else if (std.mem.eql(u8, e.kind, "review")) {
            const decision = body_str(e.body, "decision") orelse "comment";
            const review = Review{ .actor = e.actor, .decision = decision, .ts = e.ts, .oid = r.oid };
            if (std.mem.eql(u8, decision, "approve") and verified) {
                try human_approvals.append(review);
            }
            try reviews.append(review);
        }
    }
    return .{
        .id = if (ordered.len > 0) ordered[0].entry.id else "",
        .base = base,
        .head = head,
        .status = status,
        .reviews = try reviews.toOwnedSlice(),
        .human_approvals = try human_approvals.toOwnedSlice(),
        .authors = try authors.toOwnedSlice(),
        .unverified = try unverified.toOwnedSlice(),
    };
}

// ---- merge rules (§7.3) ---------------------------------------------------------------------

pub const MergeRules = struct {
    protect: []const []const u8 = &.{},
    require_human_approvals: usize = 1,
};

/// Parse the rules document (`refs/collab/meta/rules` or `--rules <file>`).
/// Unknown keys are tolerated (§7.3); a malformed document is an error — the
/// caller fails closed.
pub fn parse_merge_rules(arena: std.mem.Allocator, bytes: []const u8) !MergeRules {
    const v = jval.parseFromSliceLeaky(Value, arena, bytes, .{}) catch return error.BadRules;
    const o = switch (v) {
        .object => |o| o,
        else => return error.BadRules,
    };
    var rules = MergeRules{};
    if (o.get("require_human_approvals")) |n| {
        rules.require_human_approvals = switch (n) {
            .integer => |i| if (i >= 0) @intCast(i) else return error.BadRules,
            else => return error.BadRules,
        };
    }
    if (o.get("protect")) |pv| switch (pv) {
        .array => |arr| {
            var list = util.List([]const u8).init(arena);
            for (arr.items) |item| switch (item) {
                .string => |s| try list.append(s),
                else => return error.BadRules,
            };
            rules.protect = try list.toOwnedSlice();
        },
        else => return error.BadRules,
    };
    return rules;
}

pub const MergeEval = struct {
    allowed: bool,
    reason: []const u8,
    satisfied_by: []const []const u8,
};

/// The approvals a merge rule can count: verified `approve` reviews by
/// non-`svc-` actors, distinct, excluding the verified patch authors.
fn countable_approvers(alloc: std.mem.Allocator, pr: *const PrView) ![]const []const u8 {
    var out = util.List([]const u8).init(alloc);
    for (pr.human_approvals) |r| {
        if (!std.mem.eql(u8, r.decision, "approve") or is_agent(r.actor) or contains_str(pr.authors, r.actor)) {
            continue;
        }
        if (!contains_str(out.items, r.actor)) {
            try out.append(r.actor);
        }
    }
    return try out.toOwnedSlice();
}

pub fn merge_rule_eval(alloc: std.mem.Allocator, rules: *const MergeRules, pr: *const PrView) !MergeEval {
    var protected = false;
    if (pr.base) |b| {
        for (rules.protect) |p| {
            if (std.mem.eql(u8, p, b) or std.mem.startsWith(u8, b, p)) {
                protected = true;
                break;
            }
        }
    }
    if (!protected) {
        return .{ .allowed = true, .reason = "base is not protected", .satisfied_by = &.{} };
    }
    const satisfied_by = try countable_approvers(alloc, pr);
    if (satisfied_by.len >= rules.require_human_approvals) {
        return .{
            .allowed = true,
            .reason = try std.fmt.allocPrint(alloc, "{d} distinct non-author human approval(s) on protected base", .{satisfied_by.len}),
            .satisfied_by = satisfied_by,
        };
    }
    return .{
        .allowed = false,
        .reason = try std.fmt.allocPrint(alloc, "protected base needs {d} distinct non-author human approval(s), got {d}", .{ rules.require_human_approvals, satisfied_by.len }),
        .satisfied_by = satisfied_by,
    };
}

// ---- status transitions (§7.5 done gate) ------------------------------------------------------

/// The write-side `done` gate: the thread must be at `needs-review` and carry
/// a verified `approve` review that is not a verified patch author's. The
/// thread must already be in chain order (`thread()` first).
pub fn validate_status_transition(alloc: std.mem.Allocator, ordered: []const EntryRef, principals: *const Principals) !void {
    const current = card_status(ordered);
    if (!std.mem.eql(u8, current, "needs-review")) {
        return error.TransitionNotNeedsReview;
    }
    var authors = util.List([]const u8).init(alloc);
    defer authors.deinit();
    for (ordered) |r| {
        if (std.mem.eql(u8, r.entry.kind, "patch") and r.is_verified(alloc, principals)) {
            try authors.append(r.entry.actor);
        }
    }
    for (ordered) |r| {
        if (std.mem.eql(u8, r.entry.kind, "review") and
            mem_opt_str(r.entry.body, "decision", "approve") and
            r.is_verified(alloc, principals) and
            !contains_str(authors.items, r.entry.actor))
        {
            return;
        }
    }
    return error.TransitionNoApprove;
}

// ---- card claims (§8.4) ----------------------------------------------------------------------

pub const CardClaim = struct {
    oid: []const u8,
    actor: []const u8,
    ts: i64,
    ttl_secs: u64,
    worktree: []const u8,
    branch: []const u8,
    work: []const u8,
    session: []const u8,

    pub fn live(self: *const CardClaim, now: i64) bool {
        return now <= std.math.add(i64, self.ts, @intCast(self.ttl_secs)) catch std.math.maxInt(i64);
    }

    pub fn expires_at(self: *const CardClaim) i64 {
        return std.math.add(i64, self.ts, @intCast(self.ttl_secs)) catch std.math.maxInt(i64);
    }
};

fn optional_body_str(body: Value, key: []const u8) ?[]const u8 {
    switch (body) {
        .object => |o| {
            const v = o.get(key) orelse return "";
            return switch (v) {
                .null => "",
                .string => |s| s,
                else => null,
            };
        },
        else => return "",
    }
}

/// Parse a `card_claim` body (§8.4). Strict: a reply (non-empty parent), a
/// wrong-typed field or an out-of-range `ttl` makes the entry malformed —
/// visible, but never owning a card.
pub fn parse_card_claim(r: *const EntryRef) ?CardClaim {
    if (!std.mem.eql(u8, r.entry.kind, CARD_CLAIM_KIND) or r.entry.parent.len != 0) return null;
    const body = r.entry.body;
    var ttl_secs: u64 = CARD_CLAIM_TTL_DEFAULT_SECS;
    switch (body) {
        .object => |o| {
            if (o.get("ttl")) |tv| switch (tv) {
                .null => {},
                .integer => |i| {
                    if (i < 0) return null;
                    const n: u64 = @intCast(i);
                    if (n < CARD_CLAIM_TTL_MIN_SECS or n > CARD_CLAIM_TTL_MAX_SECS) return null;
                    ttl_secs = n;
                },
                else => return null,
            };
        },
        else => {},
    }
    return .{
        .oid = r.oid,
        .actor = r.entry.actor,
        .ts = r.entry.ts,
        .ttl_secs = ttl_secs,
        .worktree = optional_body_str(body, "worktree") orelse return null,
        .branch = optional_body_str(body, "branch") orelse return null,
        .work = optional_body_str(body, "work") orelse return null,
        .session = optional_body_str(body, "session") orelse return null,
    };
}

pub const CardClaimView = struct {
    claims: []CardClaim,
    winner: ?CardClaim,
    unverified: usize,

    /// The board projection of the claim state, `null` when the thread
    /// carries no claim entries at all (an old card's projection is unchanged).
    pub fn to_board(self: *const CardClaimView) !?BoardClaim {
        if (self.claims.len == 0 and self.unverified == 0) return null;
        var state: []const u8 = undefined;
        var winner: ?*const CardClaim = null;
        if (self.winner != null) {
            state = "claimed";
            winner = &self.winner.?;
        } else if (self.claims.len == 0) {
            state = "pending";
        } else {
            state = "stale";
        }
        return .{
            .state = state,
            .oid = if (winner) |w| w.oid else "",
            .actor = if (winner) |w| w.actor else "",
            .ts = if (winner) |w| w.ts else 0,
            .ttl = if (winner) |w| w.ttl_secs else 0,
            .expires_at = if (winner) |w| w.expires_at() else 0,
        };
    }
};

/// The convergence function of a work-card thread (§8.4): only verified,
/// well-formed claims participate; the winner is `min(ts, actor, oid)` over
/// the live ones.
pub fn card_claim_view(alloc: std.mem.Allocator, entries: []const EntryRef, principals: *const Principals, now: i64) !CardClaimView {
    var claims = util.List(CardClaim).init(alloc);
    var unverified: usize = 0;
    for (entries) |*r| {
        if (!std.mem.eql(u8, r.entry.kind, CARD_CLAIM_KIND)) continue;
        // Parse before verifying: a malformed claim is rejected without
        // paying for the signature check; both failure modes land in the
        // same `unverified` bucket.
        const parsed = parse_card_claim(r);
        if (parsed == null or !r.is_verified(alloc, principals)) {
            unverified += 1;
            continue;
        }
        try claims.append(parsed.?);
    }
    std.mem.sort(CardClaim, claims.items, {}, struct {
        fn f(_: void, a: CardClaim, b: CardClaim) bool {
            return less3(a.ts, a.actor, a.oid, b.ts, b.actor, b.oid);
        }
    }.f);
    var winner: ?CardClaim = null;
    for (claims.items) |c| {
        if (c.live(now)) {
            winner = c;
            break;
        }
    }
    return .{
        .claims = try claims.toOwnedSlice(),
        .winner = winner,
        .unverified = unverified,
    };
}

/// The statuses a live claim governs (§8.4): a claim owns an `open`/
/// `in-progress` card; everything after is the status entries' business.
pub fn card_claim_governs(status: []const u8) bool {
    return std.mem.eql(u8, status, "open") or std.mem.eql(u8, status, "in-progress");
}

/// Kinds that never give a thread a work-card identity by themselves: a pure
/// CI run, or claims without a card (§8.3/§8.4).
pub fn non_card_kind(kind: []const u8) bool {
    return std.mem.eql(u8, kind, ci_mod.CI_CLAIM_KIND) or
        std.mem.eql(u8, kind, ci_mod.CI_RESULT_KIND) or
        std.mem.eql(u8, kind, CARD_CLAIM_KIND);
}

// ---- the board projection (§8) ----------------------------------------------------------------

pub const BoardSortBy = enum { ts, id };
pub const BoardSortDirection = enum { desc, asc };

pub const BoardColumnDef = struct {
    name: []const u8,
    kind: []const u8 = "",
    status: []const u8 = "",
    merge: []const u8 = "",
    unverified: bool = false,
};

pub const BoardSort = struct {
    by: BoardSortBy = .ts,
    direction: BoardSortDirection = .desc,
};

pub const BoardDef = struct {
    version: u32,
    sort: BoardSort = .{},
    columns: []BoardColumnDef,
};

/// Fails closed: an unsupported version, no columns, an empty/duplicate
/// column name or an unknown `merge` verdict is a broken board.
pub fn validate_board_def(def: *const BoardDef) !void {
    if (def.version != 1) return error.BoardVersion;
    if (def.columns.len == 0) return error.BoardNoColumns;
    var seen = std.StringHashMap(void).init(std.heap.page_allocator);
    defer seen.deinit();
    for (def.columns) |c| {
        if (c.name.len == 0) return error.BoardColumnName;
        if (c.merge.len != 0 and !std.mem.eql(u8, c.merge, "allowed") and !std.mem.eql(u8, c.merge, "blocked")) {
            return error.BoardMergeVerdict;
        }
        if (seen.contains(c.name)) return error.BoardDuplicateColumn;
        try seen.put(c.name, {});
    }
}

/// The board when the repository defines none: one lane per well-known
/// status plus a catch-all.
pub fn default_board(alloc: std.mem.Allocator) !BoardDef {
    var columns = try alloc.alloc(BoardColumnDef, 4);
    columns[0] = .{ .name = "open", .status = "open" };
    columns[1] = .{ .name = "merged", .status = "merged" };
    columns[2] = .{ .name = "closed", .status = "closed" };
    columns[3] = .{ .name = "other" };
    return .{ .version = 1, .sort = .{}, .columns = columns };
}

pub const BoardClaim = struct {
    state: []const u8,
    oid: []const u8,
    actor: []const u8,
    ts: i64,
    ttl: u64,
    expires_at: i64,
};

pub const BoardMergeCard = struct {
    allowed: bool,
    reason: []const u8,
};

pub const BoardCard = struct {
    id: []const u8,
    title: []const u8,
    prose: []const u8,
    actor: []const u8,
    status: []const u8,
    owner: []const u8,
    worktree: []const u8,
    branch: []const u8,
    work: []const u8,
    created_ts: i64,
    last_ts: i64,
    entries: usize,
    verified: usize,
    unverified: usize,
    kinds: []const []const u8,
    last_oid: []const u8,
    merge: ?BoardMergeCard,
    claim: ?BoardClaim,
};

pub const BoardColumn = struct {
    name: []const u8,
    cards: []BoardCard,
};

pub const Board = struct {
    columns: []BoardColumn,

    /// The wire form: exactly the compact bytes the server endpoint
    /// serializes — struct field order, body-less cards (§8.2).
    pub fn to_json(self: *const Board, alloc: std.mem.Allocator) []u8 {
        var out = jsonio.Out.init(alloc);
        jsonio.put_raw(&out, "{\"columns\":[");
        for (self.columns, 0..) |col, ci| {
            if (ci > 0) jsonio.put_raw(&out, ",");
            jsonio.put_raw(&out, "{\"name\":");
            jsonio.write_string(&out, col.name);
            jsonio.put_raw(&out, ",\"cards\":[");
            for (col.cards, 0..) |card, ki| {
                if (ki > 0) jsonio.put_raw(&out, ",");
                write_card(&out, &card);
            }
            jsonio.put_raw(&out, "]}");
        }
        jsonio.put_raw(&out, "]}");
        return out.to_owned();
    }
};

fn write_card(out: *jsonio.Out, c: *const BoardCard) void {
    const w = struct {
        fn field(o: *jsonio.Out, name: []const u8, v: []const u8, first: *bool) void {
            if (!first.*) jsonio.put_raw(o, ",");
            first.* = false;
            jsonio.write_string(o, name);
            jsonio.put_raw(o, ":");
            jsonio.write_string(o, v);
        }
        fn num(o: *jsonio.Out, name: []const u8, v: i64, first: *bool) void {
            if (!first.*) jsonio.put_raw(o, ",");
            first.* = false;
            jsonio.write_string(o, name);
            jsonio.put_raw(o, ":");
            var b: [24]u8 = undefined;
            jsonio.put_raw(o, std.fmt.bufPrint(&b, "{d}", .{v}) catch unreachable);
        }
    };
    var first = true;
    jsonio.put_raw(out, "{");
    w.field(out, "id", c.id, &first);
    w.field(out, "title", c.title, &first);
    w.field(out, "prose", c.prose, &first);
    w.field(out, "actor", c.actor, &first);
    w.field(out, "status", c.status, &first);
    w.field(out, "owner", c.owner, &first);
    w.field(out, "worktree", c.worktree, &first);
    w.field(out, "branch", c.branch, &first);
    w.field(out, "work", c.work, &first);
    w.num(out, "created_ts", c.created_ts, &first);
    w.num(out, "last_ts", c.last_ts, &first);
    w.num(out, "entries", @intCast(c.entries), &first);
    w.num(out, "verified", @intCast(c.verified), &first);
    w.num(out, "unverified", @intCast(c.unverified), &first);
    jsonio.put_raw(out, ",\"kinds\":[");
    for (c.kinds, 0..) |k, i| {
        if (i > 0) jsonio.put_raw(out, ",");
        jsonio.write_string(out, k);
    }
    jsonio.put_raw(out, "]");
    jsonio.put_raw(out, ",\"last_oid\":");
    jsonio.write_string(out, c.last_oid);
    if (c.merge) |m| {
        jsonio.put_raw(out, ",\"merge\":{\"allowed\":");
        jsonio.put_raw(out, if (m.allowed) "true" else "false");
        jsonio.put_raw(out, ",\"reason\":");
        jsonio.write_string(out, m.reason);
        jsonio.put_raw(out, "}");
    } else {
        jsonio.put_raw(out, ",\"merge\":null");
    }
    if (c.claim) |cl| {
        jsonio.put_raw(out, ",\"claim\":{\"state\":");
        jsonio.write_string(out, cl.state);
        jsonio.put_raw(out, ",\"oid\":");
        jsonio.write_string(out, cl.oid);
        jsonio.put_raw(out, ",\"actor\":");
        jsonio.write_string(out, cl.actor);
        var b: [24]u8 = undefined;
        jsonio.put_raw(out, ",\"ts\":");
        jsonio.put_raw(out, std.fmt.bufPrint(&b, "{d}", .{cl.ts}) catch unreachable);
        jsonio.put_raw(out, ",\"ttl\":");
        jsonio.put_raw(out, std.fmt.bufPrint(&b, "{d}", .{cl.ttl}) catch unreachable);
        jsonio.put_raw(out, ",\"expires_at\":");
        jsonio.put_raw(out, std.fmt.bufPrint(&b, "{d}", .{cl.expires_at}) catch unreachable);
        jsonio.put_raw(out, "}");
    } else {
        jsonio.put_raw(out, ",\"claim\":null");
    }
    jsonio.put_raw(out, "}");
}

/// The card's effective status: entries applied in thread order, last match
/// wins (§8.2). Deliberately wider than the PR state machine.
pub fn card_status(ordered: []const EntryRef) []const u8 {
    var status: []const u8 = "open";
    for (ordered) |r| {
        const e = &r.entry;
        if (std.mem.eql(u8, e.kind, "status")) {
            if (body_str(e.body, "status")) |st| status = st;
        } else if (std.mem.eql(u8, e.kind, "merge_result")) {
            if (body_bool(e.body, "merged") orelse false) status = "merged";
        }
    }
    return status;
}

const WorkContext = struct {
    owner: []const u8 = "",
    worktree: []const u8 = "",
    branch: []const u8 = "",
    work: []const u8 = "",
    mentioned: [3]bool = .{ false, false, false },
};

/// Work context is inherited across status moves; an explicit `""` clears a
/// field; `work` falls back to the same entry's `note`. `actor` restricts the
/// replay to that principal's **verified** status entries (§8.4).
fn card_work_context_by(alloc: std.mem.Allocator, ordered: []const EntryRef, actor: ?[]const u8, principals: *const Principals) !WorkContext {
    var ctx = WorkContext{};
    for (ordered) |r| {
        if (!std.mem.eql(u8, r.entry.kind, "status")) continue;
        if (actor) |a| {
            if (!std.mem.eql(u8, a, r.entry.actor) or !r.is_verified(alloc, principals)) continue;
        }
        const o = switch (r.entry.body) {
            .object => |o| o,
            else => continue,
        };
        if (o.get("owner")) |v| {
            if (v == .string) ctx.owner = v.string;
        }
        if (o.get("worktree")) |v| {
            if (v == .string) {
                ctx.worktree = v.string;
                ctx.mentioned[0] = true;
            }
        }
        if (o.get("branch")) |v| {
            if (v == .string) {
                ctx.branch = v.string;
                ctx.mentioned[1] = true;
            }
        }
        if (o.get("work")) |v| {
            if (v == .string) {
                ctx.work = v.string;
                ctx.mentioned[2] = true;
            }
        } else if (o.get("note")) |v| {
            if (v == .string) {
                ctx.work = v.string;
                ctx.mentioned[2] = true;
            }
        }
    }
    return ctx;
}

/// The thread's canonical root: the first verified root entry (parent empty,
/// not a claim) in thread order, falling back to the first non-claim entry,
/// then to the first entry (§8.2).
fn canonical_root(ordered: []const EntryRef, alloc: std.mem.Allocator, principals: *const Principals) ?*const EntryRef {
    for (ordered) |*r| {
        if (r.entry.parent.len == 0 and
            !std.mem.eql(u8, r.entry.kind, CARD_CLAIM_KIND) and
            r.is_verified(alloc, principals))
        {
            return r;
        }
    }
    for (ordered) |*r| {
        if (!std.mem.eql(u8, r.entry.kind, CARD_CLAIM_KIND)) return r;
    }
    if (ordered.len > 0) return &ordered[0];
    return null;
}

fn root_title(alloc: std.mem.Allocator, ordered: []const EntryRef, principals: *const Principals) []const u8 {
    const root = canonical_root(ordered, alloc, principals) orelse return "";
    return body_str(root.entry.body, "title") orelse "";
}

/// The human-written prose of an entry: the first non-empty of
/// text/body/note/message/summary, trimmed (§8.2).
pub fn entry_prose(alloc: std.mem.Allocator, body: Value) []const u8 {
    const fields = [_][]const u8{ "text", "body", "note", "message", "summary" };
    switch (body) {
        .object => |o| {
            for (fields) |k| {
                if (o.get(k)) |v| {
                    if (v == .string) {
                        const t = std.mem.trim(u8, v.string, " \t\r\n");
                        if (t.len > 0) return t;
                    }
                }
            }
        },
        else => {},
    }
    _ = alloc;
    return "";
}

fn card_matches(card: *const BoardCard, col: *const BoardColumnDef) bool {
    if (col.kind.len != 0 and !contains_str(card.kinds, col.kind)) return false;
    if (col.status.len != 0 and !std.mem.eql(u8, card.status, col.status)) return false;
    if (col.merge.len != 0) {
        const hit = if (card.merge) |m|
            (if (std.mem.eql(u8, col.merge, "allowed")) m.allowed else !m.allowed)
        else
            false;
        if (!hit) return false;
    }
    if (col.unverified and card.unverified == 0) return false;
    return true;
}

fn sort_cards(cards: []BoardCard, sort: BoardSort) void {
    const Ctx = struct {
        sort: BoardSort,
        fn less(self: @This(), a: BoardCard, b: BoardCard) bool {
            const primary = switch (self.sort.by) {
                .ts => std.math.order(a.last_ts, b.last_ts),
                .id => std.mem.order(u8, a.id, b.id),
            };
            const oriented = if (self.sort.direction == .desc) primary.reverse() else primary;
            if (oriented != .eq) return oriented == .lt;
            // The card id breaks ties ascending either way — a total order.
            return std.mem.order(u8, a.id, b.id) == .lt;
        }
    };
    std.mem.sort(BoardCard, cards, Ctx{ .sort = sort }, Ctx.less);
}

/// The board projection: a pure function `(entries, principals, rules, board
/// definition) → columns`. First-match-wins placement; `unverified = true`
/// columns are alert lanes, not placement lanes (§8.3). Pure-CI threads and
/// claim-only threads are not cards (§8.3/§8.4).
pub fn build_board(alloc: std.mem.Allocator, entries: []const EntryRef, principals: *const Principals, rules: *const MergeRules, board: *const BoardDef, now: i64) !Board {
    // group by id
    var groups = std.StringHashMap(util.List(EntryRef)).init(alloc);
    defer {
        var it = groups.iterator();
        while (it.next()) |g| g.value_ptr.deinit();
        groups.deinit();
    }
    for (entries) |e| {
        const gop = try groups.getOrPut(e.entry.id);
        if (!gop.found_existing) gop.value_ptr.* = util.List(EntryRef).init(alloc);
        try gop.value_ptr.append(e);
    }

    var columns = try alloc.alloc(BoardColumn, board.columns.len);
    var columns_lists = try alloc.alloc(util.List(BoardCard), board.columns.len);
    for (board.columns, 0..) |def, i| {
        columns[i] = .{ .name = def.name, .cards = &.{} };
        columns_lists[i] = .empty;
    }

    var git = groups.iterator();
    while (git.next()) |g| {
        const group = g.value_ptr.items;
        var all_non_card = true;
        for (group) |r| {
            if (!non_card_kind(r.entry.kind)) {
                all_non_card = false;
                break;
            }
        }
        if (all_non_card) continue;

        const ordered = try thread(alloc, group);
        var verified: usize = 0;
        for (ordered) |r| {
            if (r.is_verified(alloc, principals)) verified += 1;
        }
        var kinds = util.List([]const u8).init(alloc);
        for (group) |r| {
            if (!contains_str(kinds.items, r.entry.kind)) try kinds.append(r.entry.kind);
        }
        std.mem.sort([]const u8, kinds.items, {}, struct {
            fn f(_: void, a: []const u8, b: []const u8) bool {
                return std.mem.order(u8, a, b) == .lt;
            }
        }.f);

        const root = canonical_root(ordered, alloc, principals);
        var merge: ?BoardMergeCard = null;
        var has_patch = false;
        for (group) |r| {
            if (std.mem.eql(u8, r.entry.kind, "patch")) {
                has_patch = true;
                break;
            }
        }
        if (has_patch) {
            const pr = try pr_view(alloc, group, principals);
            const eval = try merge_rule_eval(alloc, rules, &pr);
            merge = .{ .allowed = eval.allowed, .reason = eval.reason };
        }
        const claim_view = try card_claim_view(alloc, group, principals, now);
        var status = card_status(ordered);
        const winner: ?*const CardClaim = blk: {
            if (card_claim_governs(status)) {
                if (claim_view.winner) |*w| break :blk w;
            }
            break :blk null;
        };
        const work = if (winner) |w| blk: {
            var wk = try card_work_context_by(alloc, ordered, w.actor, principals);
            wk.owner = w.actor;
            const fills = [3]struct { mentioned: bool, value: []const u8 }{
                .{ .mentioned = wk.mentioned[0], .value = w.worktree },
                .{ .mentioned = wk.mentioned[1], .value = w.branch },
                .{ .mentioned = wk.mentioned[2], .value = w.work },
            };
            const slots = [3]*[]const u8{ &wk.worktree, &wk.branch, &wk.work };
            for (fills, slots) |fill, slot| {
                if (!fill.mentioned and slot.len == 0 and fill.value.len != 0) {
                    slot.* = fill.value;
                }
            }
            if (std.mem.eql(u8, status, "open")) status = "in-progress";
            break :blk wk;
        } else try card_work_context_by(alloc, ordered, null, principals);

        var last_ts: i64 = 0;
        for (group) |r| {
            if (r.entry.ts > last_ts) last_ts = r.entry.ts;
        }
        const card = BoardCard{
            .id = g.key_ptr.*,
            .title = root_title(alloc, ordered, principals),
            .prose = if (root) |r| entry_prose(alloc, r.entry.body) else "",
            .actor = if (root) |r| r.entry.actor else "",
            .status = status,
            .owner = work.owner,
            .worktree = work.worktree,
            .branch = work.branch,
            .work = work.work,
            .created_ts = if (root) |r| r.entry.ts else 0,
            .last_ts = last_ts,
            .entries = group.len,
            .verified = verified,
            .unverified = group.len - verified,
            .kinds = try kinds.toOwnedSlice(),
            .last_oid = if (ordered.len > 0) ordered[ordered.len - 1].oid else "",
            .merge = merge,
            .claim = try claim_view.to_board(),
        };
        // First-match-wins placement; alert lanes never consume the card.
        var alerted = false;
        for (board.columns, 0..) |*def, i| {
            if (!card_matches(&card, def)) continue;
            if (def.unverified) {
                if (!alerted) {
                    try columns_lists[i].append(alloc, card);
                    alerted = true;
                }
                continue;
            }
            try columns_lists[i].append(alloc, card);
            break;
        }
    }
    for (columns_lists, 0..) |*list, i| {
        sort_cards(list.items, board.sort);
        columns[i].cards = list.items;
    }
    return .{ .columns = columns };
}

// ---- the report (§7.4) -----------------------------------------------------------------------

pub const ReportThread = struct {
    id: []const u8,
    title: []const u8,
    entries: usize,
    verified: usize,
    last_ts: i64,
    kinds: []const []const u8,
};

pub const ReportPr = struct {
    id: []const u8,
    title: []const u8,
    base: ?[]const u8,
    head: ?[]const u8,
    status: []const u8,
    approvals: usize,
    merge_allowed: bool,
    merge_reason: []const u8,
};

/// PR rows carry the thread's last activity through the sort (§7.4).
const TaggedPr = struct {
    ts: i64,
    p: ReportPr,
};

pub const ReportRun = struct {
    id: []const u8,
    task: []const u8,
    repo_ref: []const u8,
    commit: []const u8,
    state: []const u8,
    conclusion: ?[]const u8,
    runner: ?[]const u8,
    claims: usize,
    last_ts: i64,
};

pub const Report = struct {
    threads: []ReportThread = &.{},
    prs: []ReportPr = &.{},
    runs: []ReportRun = &.{},
    total_entries: usize = 0,
    verified_entries: usize = 0,
    unverified_entries: usize = 0,
    missing_principals: usize = 0,
    by_actor: []const Count = &.{},
    by_kind: []const Count = &.{},

    pub const Count = struct { key: []const u8, n: usize };
};

pub fn build_report(alloc: std.mem.Allocator, entries: []const EntryRef, principals: *const Principals, rules: *const MergeRules, now: i64) !Report {
    var groups = std.StringHashMap(util.List(EntryRef)).init(alloc);
    defer {
        var it = groups.iterator();
        while (it.next()) |g| g.value_ptr.deinit();
        groups.deinit();
    }
    for (entries) |e| {
        const gop = try groups.getOrPut(e.entry.id);
        if (!gop.found_existing) gop.value_ptr.* = util.List(EntryRef).init(alloc);
        try gop.value_ptr.append(e);
    }

    var threads = util.List(ReportThread).init(alloc);
    var prs = util.List(TaggedPr).init(alloc);

    var git = groups.iterator();
    while (git.next()) |g| {
        const group = g.value_ptr.items;
        var all_non_card = true;
        for (group) |r| {
            if (!non_card_kind(r.entry.kind)) {
                all_non_card = false;
                break;
            }
        }
        if (all_non_card) continue;
        const ordered = try thread(alloc, group);
        var verified: usize = 0;
        for (ordered) |r| {
            if (r.is_verified(alloc, principals)) verified += 1;
        }
        var kinds = util.List([]const u8).init(alloc);
        for (group) |r| {
            if (!contains_str(kinds.items, r.entry.kind)) try kinds.append(r.entry.kind);
        }
        std.mem.sort([]const u8, kinds.items, {}, struct {
            fn f(_: void, a: []const u8, b: []const u8) bool {
                return std.mem.order(u8, a, b) == .lt;
            }
        }.f);
        var last_ts: i64 = 0;
        for (group) |r| {
            if (r.entry.ts > last_ts) last_ts = r.entry.ts;
        }
        try threads.append(.{
            .id = g.key_ptr.*,
            .title = root_title(alloc, ordered, principals),
            .entries = group.len,
            .verified = verified,
            .last_ts = last_ts,
            .kinds = try kinds.toOwnedSlice(),
        });

        var has_patch = false;
        for (group) |r| {
            if (std.mem.eql(u8, r.entry.kind, "patch")) {
                has_patch = true;
                break;
            }
        }
        if (has_patch) {
            const pr = try pr_view(alloc, group, principals);
            const eval = try merge_rule_eval(alloc, rules, &pr);
            var last_ts2: i64 = 0;
            for (group) |r| {
                if (r.entry.ts > last_ts2) last_ts2 = r.entry.ts;
            }
            const approvers = try countable_approvers(alloc, &pr);
            try prs.append(.{ .ts = last_ts2, .p = .{
                .id = g.key_ptr.*,
                .title = root_title(alloc, ordered, principals),
                .base = pr.base,
                .head = pr.head,
                .status = pr.status,
                .approvals = approvers.len,
                .merge_allowed = eval.allowed,
                .merge_reason = eval.reason,
            } });
        }
    }

    // The list order is part of the projection (§7.4): threads newest
    // activity first, id ascending breaks ties; PRs open → merged → rest,
    // then newest first, id ascending.
    std.mem.sort(ReportThread, threads.items, {}, struct {
        fn f(_: void, a: ReportThread, b: ReportThread) bool {
            if (a.last_ts != b.last_ts) return a.last_ts > b.last_ts;
            return std.mem.order(u8, a.id, b.id) == .lt;
        }
    }.f);
    const PrCtx = struct {
        fn rank(s: []const u8) u8 {
            if (std.mem.eql(u8, s, "open")) return 0;
            if (std.mem.eql(u8, s, "merged")) return 1;
            return 2;
        }
        fn less(_: void, a: TaggedPr, b: TaggedPr) bool {
            const ra = rank(a.p.status);
            const rb = rank(b.p.status);
            if (ra != rb) return ra < rb;
            if (a.ts != b.ts) return a.ts > b.ts;
            return std.mem.order(u8, a.p.id, b.p.id) == .lt;
        }
    };
    std.mem.sort(TaggedPr, prs.items, {}, PrCtx.less);
    var stripped = try alloc.alloc(ReportPr, prs.items.len);
    for (prs.items, 0..) |tp, i| stripped[i] = tp.p;

    var verified_total: usize = 0;
    var unverified_total: usize = 0;
    var missing: usize = 0;
    for (entries) |r| {
        if (r.is_verified(alloc, principals)) {
            verified_total += 1;
        } else {
            unverified_total += 1;
            if (!principals.contains(r.entry.actor)) missing += 1;
        }
    }

    // by_actor / by_kind sorted ascending by key (BTreeMap order).
    var actor_map = std.StringHashMap(usize).init(alloc);
    defer actor_map.deinit();
    var kind_map = std.StringHashMap(usize).init(alloc);
    defer kind_map.deinit();
    for (entries) |r| {
        const a = try actor_map.getOrPut(r.entry.actor);
        if (!a.found_existing) a.value_ptr.* = 0;
        a.value_ptr.* += 1;
        const k = try kind_map.getOrPut(r.entry.kind);
        if (!k.found_existing) k.value_ptr.* = 0;
        k.value_ptr.* += 1;
    }
    const by_actor = try sorted_counts(alloc, &actor_map);
    const by_kind = try sorted_counts(alloc, &kind_map);

    // The CI section rides the same log (§10): pure-CI threads are skipped
    // above and projected here by the CI aggregation.
    var runs = util.List(ReportRun).init(alloc);
    const run_map = try ci_mod.collect_runs(alloc, entries, principals, now);
    for (run_map) |kv| {
        const run = kv.view;
        var claims: usize = 0;
        for (run.attempts) |a| claims += a.claims.len;
        try runs.append(.{
            .id = kv.id,
            .task = run.task,
            .repo_ref = run.repo_ref,
            .commit = run.commit,
            .state = run.state.as_str(),
            .conclusion = if (run.conclusion) |c| c.as_str() else null,
            .runner = run.runner,
            .claims = claims,
            .last_ts = run.last_ts,
        });
    }

    return .{
        .threads = try threads.toOwnedSlice(),
        .prs = stripped,
        .runs = try runs.toOwnedSlice(),
        .total_entries = entries.len,
        .verified_entries = verified_total,
        .unverified_entries = unverified_total,
        .missing_principals = missing,
        .by_actor = by_actor,
        .by_kind = by_kind,
    };
}

fn sorted_counts(alloc: std.mem.Allocator, map: *std.StringHashMap(usize)) ![]Report.Count {
    var keys = util.List([]const u8).init(alloc);
    defer keys.deinit();
    var it = map.iterator();
    while (it.next()) |e| try keys.append(e.key_ptr.*);
    std.mem.sort([]const u8, keys.items, {}, struct {
        fn f(_: void, a: []const u8, b: []const u8) bool {
            return std.mem.order(u8, a, b) == .lt;
        }
    }.f);
    var out = util.List(Report.Count).init(alloc);
    for (keys.items) |k| {
        try out.append(.{ .key = k, .n = map.get(k).? });
    }
    return try out.toOwnedSlice();
}

// ---- small value helpers ----------------------------------------------------------------------

pub fn body_str(body: Value, key: []const u8) ?[]const u8 {
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

pub fn body_bool(body: Value, key: []const u8) ?bool {
    switch (body) {
        .object => |o| {
            const v = o.get(key) orelse return null;
            return switch (v) {
                .bool => |b| b,
                else => null,
            };
        },
        else => return null,
    }
}

fn mem_opt_str(body: Value, key: []const u8, want: []const u8) bool {
    const s = body_str(body, key) orelse return false;
    return std.mem.eql(u8, s, want);
}

fn contains_str(list: []const []const u8, s: []const u8) bool {
    for (list) |item| {
        if (std.mem.eql(u8, item, s)) return true;
    }
    return false;
}
