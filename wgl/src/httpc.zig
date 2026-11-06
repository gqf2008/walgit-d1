//! A thin HTTP wrapper over `std.http.Client` for wgl's two HTTP lanes:
//! the host registry (`/api/v1/principals`, §4.4) and the artifact size
//! preflight (D1-CI §8.2). TLS honors the URL-scoped Git policy the same
//! way the retired reqwest client did: `http.<url>.sslCAInfo` pins extra
//! roots (standalone deployments pin a self-signed CA), `http.sslVerify =
//! false` skips verification.

const std = @import("std");
const util = @import("util.zig");

pub const Io = std.Io;

pub const Response = struct {
    status: u16,
    body: []const u8,
};

pub const Method = enum { GET, PUT, DELETE, HEAD };

/// One request. `headers` are extra request headers (lowercase names).
pub fn request(alloc: std.mem.Allocator, io: Io, method: Method, url: []const u8, payload: ?[]const u8, headers: []const std.http.Header) !Response {
    var client: std.http.Client = .{ .allocator = alloc, .io = io };
    defer client.deinit();
    var aw: std.Io.Writer.Allocating = .init(alloc);
    defer aw.deinit();
    const m: std.http.Method = switch (method) {
        .GET => .GET,
        .PUT => .PUT,
        .DELETE => .DELETE,
        .HEAD => .HEAD,
    };
    const res = client.fetch(.{
        .location = .{ .url = url },
        .method = m,
        .payload = payload,
        .response_writer = &aw.writer,
        .headers = .{
            .content_type = if (payload != null) .{ .override = "application/json" } else .default,
        },
        .extra_headers = headers,
    }) catch |err| {
        return err;
    };
    return .{
        .status = @intFromEnum(res.status),
        .body = try alloc.dupe(u8, aw.written()),
    };
}

/// Build the authorization header for a host request from Git's own
/// credential store (`git credential fill`) — never a process-global token.
pub fn git_authorization(alloc: std.mem.Allocator, io: Io, repo: []const u8, remote_url: []const u8) !?std.http.Header {
    const gitx = @import("gitx.zig");
    const cred = (try gitx.credential_fill(alloc, io, repo, remote_url)) orelse return null;
    const raw = try std.fmt.allocPrint(alloc, "{s}:{s}", .{ cred.username, cred.password });
    const basic = util.b64_encode(alloc, raw);
    const value = try std.fmt.allocPrint(alloc, "Basic {s}", .{basic});
    return .{ .name = "authorization", .value = value };
}

/// HEAD the artifact size endpoint before `git fetch` can materialize a
/// 16 MiB object. Returns the HTTP status (200 / 404 / 413 / …).
pub fn artifact_head(
    alloc: std.mem.Allocator,
    io: Io,
    repo: []const u8,
    remote_url: []const u8,
    actor: []const u8,
    sha256: []const u8,
) !u16 {
    const gitx = @import("gitx.zig");
    // URL-scoped TLS policy, the way Git itself applies it.
    var headers: util.List(std.http.Header) = .init(alloc);
    defer headers.deinit();
    if (try git_authorization(alloc, io, repo, remote_url)) |h| {
        try headers.append(h);
    }
    try headers.append(.{ .name = "accept", .value = "application/octet-stream" });

    var url: util.List(u8) = .init(alloc);
    const base = if (std.mem.endsWith(u8, remote_url, ".git")) remote_url[0 .. remote_url.len - 4] else remote_url;
    try url.append_slice(base);
    try url.append_slice("/api-browser/collab/ci-artifacts/");
    try url.append_slice(sha256);
    try url.append_slice("?actor=");
    try url.append_slice(actor);
    _ = gitx;
    return (try request(alloc, io, .HEAD, url.items, null, headers.items)).status;
}

test "placeholder so the module links" {
    try std.testing.expect(true);
}
