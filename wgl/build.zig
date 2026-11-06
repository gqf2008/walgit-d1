const std = @import("std");

// wgl — the standalone D1 collaboration client (`docs/D1_PROTOCOL.md`).
// Ported from the retired `walgit collab|principal|ci` command families.
pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const exe_mod = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        // identity.zig's POSIX mode fallback goes through std.c (Io.File has
        // no chmod lane); musl targets link it statically, the others use the
        // system C runtime. The module flag covers the exe and the tests.
        .link_libc = true,
    });

    const exe = b.addExecutable(.{ .name = "wgl", .root_module = exe_mod });
    b.installArtifact(exe);

    const tests = b.addTest(.{ .root_module = exe_mod });
    const run_tests = b.addRunArtifact(tests);
    const test_step = b.step("test", "Run unit tests");
    test_step.dependOn(&run_tests.step);
}
