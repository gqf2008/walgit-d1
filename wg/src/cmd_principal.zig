//! `wg principal` — the host-global principal registry commands (§4.4):
//! register / rotate / list / revoke against `/api/v1/principals`. HTTP
//! only, no bucket access. Ported from the retired Rust principal commands.

const std = @import("std");
const args = @import("args.zig");
const entry_mod = @import("d1/entry.zig");
const httpc = @import("httpc.zig");
const identity = @import("identity.zig");
const util = @import("util.zig");

const Io = std.Io;
const Ctx = @import("cmd_collab.zig").Ctx;

fn bearer_required(c: *Ctx, token: ?[]const u8) ![]const u8 {
    return c.token_opt(token) orelse {
        std.debug.print("wg: no bearer token: pass --token or set WALGIT_TOKEN\n", .{});
        return error.NoToken;
    };
}

pub fn run(c: *Ctx, action: []const u8, p: *args.Parsed) !u8 {
    if (std.mem.eql(u8, action, "register") or std.mem.eql(u8, action, "rotate")) {
        const url = try p.required("url");
        const principal = try p.required("principal");
        try identity.ref_segment("principal", principal);
        const token = try bearer_required(c, p.opt("token"));
        const key = identity.read_signing_key(c.alloc, c.io, try p.required("key")) catch return error.BadKey;
        const public_key = entry_mod.public_key_b64(c.alloc, key);
        const full_url = try std.fmt.allocPrint(c.alloc, "{s}/api/v1/principals/{s}", .{ url, principal });
        var body = jsonio_object();
        jsonio.obj_put(&body, c.alloc, "public_key", .{ .string = public_key });
        const payload = jsonio.to_string(c.alloc, .{ .object = body });
        var headers: util.List(std.http.Header) = .init(c.alloc);
        defer headers.deinit();
        try headers.append(.{ .name = "authorization", .value = try std.fmt.allocPrint(c.alloc, "Bearer {s}", .{token}) });
        const resp = try httpc.request(c.alloc, c.io, .PUT, full_url, payload, headers.items);
        if (resp.status / 100 != 2) {
            std.debug.print("wg: PUT {s} -> {d}: {s}\n", .{ full_url, resp.status, resp.body });
            return error.HttpFailed;
        }
        const verb: []const u8 = if (std.mem.eql(u8, action, "rotate")) "rotated" else "registered";
        c.stdout_line(try std.fmt.allocPrint(c.alloc, "{s} {s} at {s}", .{ verb, principal, url }));
        return 0;
    }
    if (std.mem.eql(u8, action, "list")) {
        const url = try p.required("url");
        const full_url = try std.fmt.allocPrint(c.alloc, "{s}/api/v1/principals", .{url});
        var headers: util.List(std.http.Header) = .init(c.alloc);
        defer headers.deinit();
        try headers.append(.{ .name = "accept", .value = "application/json" });
        if (c.token_opt(p.opt("token"))) |t| {
            try headers.append(.{ .name = "authorization", .value = try std.fmt.allocPrint(c.alloc, "Bearer {s}", .{t}) });
        }
        const resp = try httpc.request(c.alloc, c.io, .GET, full_url, null, headers.items);
        if (resp.status / 100 != 2) {
            std.debug.print("wg: GET {s} -> {d}: {s}\n", .{ full_url, resp.status, resp.body });
            return error.HttpFailed;
        }
        c.stdout(resp.body);
        if (resp.body.len > 0 and resp.body[resp.body.len - 1] != '\n') c.stdout("\n");
        return 0;
    }
    if (std.mem.eql(u8, action, "revoke")) {
        const url = try p.required("url");
        const principal = try p.required("principal");
        try identity.ref_segment("principal", principal);
        const token = try bearer_required(c, p.opt("token"));
        const full_url = try std.fmt.allocPrint(c.alloc, "{s}/api/v1/principals/{s}", .{ url, principal });
        var headers: util.List(std.http.Header) = .init(c.alloc);
        defer headers.deinit();
        try headers.append(.{ .name = "authorization", .value = try std.fmt.allocPrint(c.alloc, "Bearer {s}", .{token}) });
        const resp = try httpc.request(c.alloc, c.io, .DELETE, full_url, null, headers.items);
        if (resp.status / 100 != 2) {
            std.debug.print("wg: DELETE {s} -> {d}: {s}\n", .{ full_url, resp.status, resp.body });
            return error.HttpFailed;
        }
        c.stdout_line(try std.fmt.allocPrint(c.alloc, "revoked {s} at {s}", .{ principal, url }));
        return 0;
    }
    std.debug.print("wg: unknown principal action \"{s}\" (register|rotate|list|revoke)\n", .{action});
    return error.UnknownAction;
}

fn jsonio_object() std.json.ObjectMap {
    return .{};
}

const jsonio = @import("jsonio.zig");
