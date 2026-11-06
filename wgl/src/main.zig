//! wgl — the standalone D1 collaboration client (`docs/D1_PROTOCOL.md`).
//! Commands: collab / principal / ci (the surfaces the retired `walgit`
//! command families owned). This entrypoint wires the process bootstrap;
//! behavior lives in the `cmd_*.zig` modules.

const std = @import("std");
const entry = @import("d1/entry.zig");
const collab = @import("d1/collab.zig");
const ci = @import("d1/ci.zig");
const snapshot = @import("d1/snapshot.zig");
const gitx = @import("gitx.zig");
const toml = @import("toml.zig");
const identity = @import("identity.zig");
const boarddef = @import("boarddef.zig");
const ciconfig = @import("ciconfig.zig");
const cron = @import("cron.zig");
const jsonio = @import("jsonio.zig");
const util = @import("util.zig");

comptime {
    _ = collab;
    _ = ci;
    _ = snapshot;
    _ = gitx;
    _ = toml;
    _ = identity;
    _ = boarddef;
    _ = ciconfig;
    _ = cron;
}

pub fn main(init: std.process.Init) !u8 {
    _ = entry;
    _ = jsonio;
    _ = util;
    var it = init.args.iterate();
    var wrote = false;
    while (it.next()) |arg| {
        try init.io.writeFile(.stdout(), arg);
        try init.io.writeFile(.stdout(), " ");
        wrote = true;
    }
    if (wrote) try init.io.writeFile(.stdout(), "\n");
    return 0;
}
