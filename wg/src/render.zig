//! The read-command renderers: `report` and `board` (and the CI run table)
//! in text / markdown / html — byte-for-byte the shapes the retired Rust
//! commands printed. The report's list order
//! is the projection's (§7.4); renderers never re-sort.

const std = @import("std");
const util = @import("util.zig");
const collab = @import("d1/collab.zig");
const ci = @import("d1/ci.zig");

const Io = std.Io;

fn w(out: *util.List(u8), comptime fmt: []const u8, args: anytype) void {
    const s = std.fmt.allocPrint(out.alloc, fmt, args) catch @panic("oom");
    out.append_slice(s) catch @panic("oom");
}

/// What a work unit is called on a list: its title, falling back to the
/// thread id when it has none (§7.4 — the board cards and the report lists
/// label the same way).
fn unit_label(title: []const u8, id: []const u8) []const u8 {
    return if (title.len == 0) id else title;
}

fn unpushed_line(alloc: std.mem.Allocator, remote: []const u8, count: usize) []const u8 {
    const head = std.fmt.allocPrint(alloc, "unpushed: {d} (vs {s})", .{ count, remote }) catch @panic("oom");
    if (count == 0) return head;
    return std.fmt.allocPrint(alloc, "{s}\n  fix: git push {s} 'refs/collab/inbox/*:refs/collab/inbox/*' 'refs/collab/meta/*:refs/collab/meta/*'", .{ head, remote }) catch @panic("oom");
}

pub const Unpushed = struct {
    remote: []const u8,
    count: usize,
};

pub fn render_report_text(alloc: std.mem.Allocator, r: *const collab.Report, unpushed: ?Unpushed) []u8 {
    var out: util.List(u8) = .init(alloc);
    w(&out, "collab report: {d} threads, {d} PRs, {d}/{d} entries verified\n\n", .{ r.threads.len, r.prs.len, r.verified_entries, r.total_entries });
    if (unpushed) |u| {
        w(&out, "{s}\n\n", .{unpushed_line(alloc, u.remote, u.count)});
    }
    out.append_slice("threads\n") catch @panic("oom");
    for (r.threads) |t| {
        w(&out, "  {s}: {d} entries ({d} verified), kinds ", .{ unit_label(t.title, t.id), t.entries, t.verified });
        for (t.kinds, 0..) |k, i| {
            if (i > 0) out.append('/') catch @panic("oom");
            out.append_slice(k) catch @panic("oom");
        }
        w(&out, ", last {d}\n", .{t.last_ts});
    }
    out.append_slice("\nprs\n") catch @panic("oom");
    for (r.prs) |p| {
        w(&out, "  {s} [{s}] approvals={d} merge_allowed={} ({s})\n", .{ unit_label(p.title, p.id), p.status, p.approvals, p.merge_allowed, p.merge_reason });
    }
    out.append_slice("\nactivity\n") catch @panic("oom");
    for (r.by_actor) |a| {
        w(&out, "  {s}: {d}\n", .{ a.key, a.n });
    }
    return out.to_owned_slice() catch @panic("oom");
}

pub fn render_report_markdown(alloc: std.mem.Allocator, r: *const collab.Report, unpushed: ?Unpushed) []u8 {
    var out: util.List(u8) = .init(alloc);
    w(&out, "# collab report\n\n{d} threads, {d} PRs, **{d}/{d}** entries verified.\n\n", .{ r.threads.len, r.prs.len, r.verified_entries, r.total_entries });
    if (unpushed) |u| {
        w(&out, "{s}\n\n", .{unpushed_line(alloc, u.remote, u.count)});
    }
    out.append_slice("## threads\n\n| title | entries | verified | kinds | last |\n|---|---|---|---|---|\n") catch @panic("oom");
    for (r.threads) |t| {
        const kinds = kinds_joined(alloc, t.kinds);
        w(&out, "| {s} | {d} | {d} | {s} | {d} |\n", .{ util.md_cell(alloc, unit_label(t.title, t.id)), t.entries, t.verified, kinds, t.last_ts });
    }
    out.append_slice("\n## PRs\n\n| title | status | approvals | merge |\n|---|---|---|---|\n") catch @panic("oom");
    for (r.prs) |p| {
        w(&out, "| {s} | {s} | {d} | {} |\n", .{ util.md_cell(alloc, unit_label(p.title, p.id)), p.status, p.approvals, p.merge_allowed });
    }
    return out.to_owned_slice() catch @panic("oom");
}

fn kinds_joined(alloc: std.mem.Allocator, kinds: []const []const u8) []const u8 {
    var out: util.List(u8) = .init(alloc);
    for (kinds, 0..) |k, i| {
        if (i > 0) out.append('/') catch @panic("oom");
        out.append_slice(k) catch @panic("oom");
    }
    return out.items;
}

pub fn render_report_html(alloc: std.mem.Allocator, r: *const collab.Report, unpushed: ?Unpushed) []u8 {
    var rows: util.List(u8) = .init(alloc);
    for (r.threads) |t| {
        w(&rows, "<tr><td>{s}</td><td>{d}</td><td>{d}</td><td>{s}</td><td>{d}</td></tr>\n", .{ util.esc(alloc, unit_label(t.title, t.id)), t.entries, t.verified, util.esc(alloc, kinds_joined(alloc, t.kinds)), t.last_ts });
    }
    var prs: util.List(u8) = .init(alloc);
    for (r.prs) |p| {
        w(&prs, "<tr><td>{s}</td><td>{s}</td><td>{d}</td><td>{}</td></tr>\n", .{ util.esc(alloc, unit_label(p.title, p.id)), util.esc(alloc, p.status), p.approvals, p.merge_allowed });
    }
    const unpushed_html: []const u8 = blk: {
        const u = unpushed orelse break :blk "";
        if (u.count == 0) {
            break :blk std.fmt.allocPrint(alloc, "<p>unpushed: 0 (vs {s})</p>", .{util.esc(alloc, u.remote)}) catch @panic("oom");
        }
        break :blk std.fmt.allocPrint(alloc, "<p>unpushed: {d} (vs {s}) — fix: <code>git push {s} 'refs/collab/inbox/*:refs/collab/inbox/*' 'refs/collab/meta/*:refs/collab/meta/*'</code></p>", .{ u.count, util.esc(alloc, u.remote), util.esc(alloc, u.remote) }) catch @panic("oom");
    };
    var out: util.List(u8) = .init(alloc);
    w(
        &out,
        "<!doctype html><html><head><meta charset=utf-8><title>wg collab</title><style>body{{font-family:system-ui;margin:2rem;color:#111}}table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.3rem .6rem;text-align:left}}</style></head><body><h1>collab report</h1><p>{d} threads, {d} PRs, {d}/{d} entries verified</p>{s}<h2>threads</h2><table><tr><th>title</th><th>entries</th><th>verified</th><th>kinds</th><th>last</th></tr>{s}</table><h2>PRs</h2><table><tr><th>title</th><th>status</th><th>approvals</th><th>merge</th></tr>{s}</table></body></html>",
        .{ r.threads.len, r.prs.len, r.verified_entries, r.total_entries, unpushed_html, rows.items, prs.items },
    );
    return out.to_owned_slice() catch @panic("oom");
}

pub fn render_board_text(alloc: std.mem.Allocator, b: *const collab.Board) []u8 {
    var out: util.List(u8) = .init(alloc);
    for (b.columns) |*col| {
        w(&out, "== {s} ({d}) ==\n", .{ col.name, col.cards.len });
        for (col.cards) |*c| {
            w(&out, "  {s} [{s}] {d} entries ({d} verified), by {s}, last {d}\n", .{ unit_label(c.title, c.id), c.status, c.entries, c.verified, c.actor, c.last_ts });
            var parts: util.List([]const u8) = .init(alloc);
            if (c.owner.len > 0) parts.append(std.fmt.allocPrint(alloc, "owner={s}", .{c.owner}) catch @panic("oom")) catch @panic("oom");
            if (c.worktree.len > 0) parts.append(std.fmt.allocPrint(alloc, "worktree={s}", .{c.worktree}) catch @panic("oom")) catch @panic("oom");
            if (c.branch.len > 0) parts.append(std.fmt.allocPrint(alloc, "branch={s}", .{c.branch}) catch @panic("oom")) catch @panic("oom");
            if (parts.items.len > 0) {
                var ctx: util.List(u8) = .init(alloc);
                for (parts.items, 0..) |p, i| {
                    if (i > 0) ctx.append(' ') catch @panic("oom");
                    ctx.append_slice(p) catch @panic("oom");
                }
                w(&out, "      {s}\n", .{ctx.items});
            }
            if (c.work.len > 0) {
                w(&out, "      work: {s}\n", .{c.work});
            }
        }
        out.append('\n') catch @panic("oom");
    }
    return out.to_owned_slice() catch @panic("oom");
}

pub fn render_board_markdown(alloc: std.mem.Allocator, b: *const collab.Board) []u8 {
    var out: util.List(u8) = .init(alloc);
    out.append_slice("# board\n\n") catch @panic("oom");
    for (b.columns) |*col| {
        w(&out, "## {s} ({d})\n\n", .{ col.name, col.cards.len });
        if (col.cards.len == 0) {
            out.append_slice("_(empty)_\n\n") catch @panic("oom");
            continue;
        }
        out.append_slice("| card | status | owner | worktree | branch | work | entries | verified | last |\n|---|---|---|---|---|---|---|---|---|\n") catch @panic("oom");
        for (col.cards) |*c| {
            w(&out, "| {s} | {s} | {s} | {s} | {s} | {s} | {d} | {d} | {d} |\n", .{
                util.md_cell(alloc, unit_label(c.title, c.id)),
                util.md_cell(alloc, c.status),
                util.md_cell(alloc, c.owner),
                util.md_cell(alloc, c.worktree),
                util.md_cell(alloc, c.branch),
                util.md_cell(alloc, c.work),
                c.entries,
                c.verified,
                c.last_ts,
            });
        }
        out.append('\n') catch @panic("oom");
    }
    return out.to_owned_slice() catch @panic("oom");
}

// ---- CI run table (one aggregation, three renderings) --------------------------

fn short_oid(commit: []const u8) []const u8 {
    return if (commit.len <= 8) commit else commit[0..8];
}

fn sorted_runs(alloc: std.mem.Allocator, runs: []const ci.RunEntry) []const ci.RunEntry {
    const sorted = alloc.dupe(ci.RunEntry, runs) catch @panic("oom");
    std.mem.sort(ci.RunEntry, sorted, {}, struct {
        fn f(_: void, a: ci.RunEntry, b: ci.RunEntry) bool {
            return a.view.last_ts > b.view.last_ts;
        }
    }.f);
    return sorted;
}

pub fn ci_runs_text(alloc: std.mem.Allocator, runs: []const ci.RunEntry) []u8 {
    var out: util.List(u8) = .init(alloc);
    for (sorted_runs(alloc, runs)) |kv| {
        const r = &kv.view;
        w(&out, "{s:<19} {s:<10} {s:<24} @ {s:<8} {s:<8} {s:<9} attempt {d}{s}\n", .{
            r.id,
            r.task,
            r.repo_ref,
            short_oid(r.commit),
            r.state.as_str(),
            if (r.conclusion) |c| c.as_str() else "-",
            r.latest_attempt,
            if (r.runner) |a| std.fmt.allocPrint(alloc, " by {s}", .{a}) catch @panic("oom") else "",
        });
        if (r.unverified > 0) {
            w(&out, "{s:<19} {d} unverified/malformed entry(ies) — visible red, not counted\n", .{ "", r.unverified });
        }
    }
    return out.to_owned_slice() catch @panic("oom");
}

pub fn ci_runs_markdown(alloc: std.mem.Allocator, runs: []const ci.RunEntry) []u8 {
    var out: util.List(u8) = .init(alloc);
    out.append_slice("| run | task | ref | commit | state | conclusion | attempt | runner |\n|---|---|---|---|---|---|---|---|\n") catch @panic("oom");
    for (sorted_runs(alloc, runs)) |kv| {
        const r = &kv.view;
        w(&out, "| {s} | {s} | {s} | {s} | {s} | {s} | {d} | {s} |\n", .{
            r.id,
            r.task,
            r.repo_ref,
            short_oid(r.commit),
            r.state.as_str(),
            if (r.conclusion) |c| c.as_str() else "-",
            r.latest_attempt,
            r.runner orelse "-",
        });
    }
    return out.to_owned_slice() catch @panic("oom");
}

pub fn ci_runs_html(alloc: std.mem.Allocator, runs: []const ci.RunEntry) []u8 {
    var rows: util.List(u8) = .init(alloc);
    for (sorted_runs(alloc, runs)) |kv| {
        const r = &kv.view;
        w(&rows, "<tr><td>{s}</td><td>{s}</td><td>{s}</td><td>{s}</td><td>{s}</td><td>{s}</td><td>{d}</td><td>{s}</td></tr>\n", .{
            util.esc(alloc, r.id),
            util.esc(alloc, r.task),
            util.esc(alloc, r.repo_ref),
            util.esc(alloc, short_oid(r.commit)),
            r.state.as_str(),
            if (r.conclusion) |c| c.as_str() else "-",
            r.latest_attempt,
            util.esc(alloc, r.runner orelse "-"),
        });
    }
    return std.fmt.allocPrint(alloc, "<h2>CI runs</h2><table><tr><th>run</th><th>task</th><th>ref</th><th>commit</th><th>state</th><th>conclusion</th><th>attempt</th><th>runner</th></tr>{s}</table>", .{rows.items}) catch @panic("oom");
}
