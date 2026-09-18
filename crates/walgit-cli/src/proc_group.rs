//! Platform helpers for launching and killing a command's whole process tree.
//!
//! Both the CI runner and the MCP child executor run shell commands that may
//! fork (for example `walgit collab entry --push` -> `git push`). A timeout or
//! output cap must not leave that grandchild behind holding the capture pipes,
//! so the command is placed in its own process group at spawn time and the
//! group/tree is killed on the failure path.

use std::process::Child;
use std::process::Command;

/// A command type that can be put into a fresh process group.
pub(crate) trait ProcessCommand {
    fn as_std_mut(&mut self) -> &mut Command;
}

impl ProcessCommand for Command {
    fn as_std_mut(&mut self) -> &mut Command {
        self
    }
}

impl ProcessCommand for tokio::process::Command {
    fn as_std_mut(&mut self) -> &mut Command {
        tokio::process::Command::as_std_mut(self)
    }
}

/// A child type whose process tree can be killed.
pub(crate) trait ProcessChild {
    fn process_id(&self) -> Option<u32>;
    fn kill_direct(&mut self) -> std::io::Result<()>;
}

impl ProcessChild for Child {
    fn process_id(&self) -> Option<u32> {
        Some(Child::id(self))
    }

    fn kill_direct(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
}

impl ProcessChild for tokio::process::Child {
    fn process_id(&self) -> Option<u32> {
        tokio::process::Child::id(self)
    }

    fn kill_direct(&mut self) -> std::io::Result<()> {
        tokio::process::Child::start_kill(self)
    }
}

/// Put a child in its own process group: Unix `setpgid` via
/// `process_group(0)`, Windows a fresh console process group. `sh -c` does
/// not always exec its single command (it forks on some shells), so a timeout
/// that killed only the direct child would leave a grandchild holding the
/// capture pipes open.
pub(crate) fn spawn_in_own_group<C: ProcessCommand>(cmd: &mut C) {
    let cmd = cmd.as_std_mut();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Kill a command's whole tree: one group signal (Unix) or a tree walk
/// (`taskkill /T`, Windows) so every forked descendant dies with the child.
/// Falls back to killing only the direct child if the platform tree kill fails.
pub(crate) fn kill_tree<C: ProcessChild>(child: &mut C) {
    let Some(pid) = child.process_id() else {
        let _ = child.kill_direct();
        return;
    };
    if !kill_pid_tree(pid) {
        let _ = child.kill_direct();
    }
}

#[cfg(unix)]
#[allow(unsafe_code)] // killpg — the same platform-seam exception as walgit-wal/src/platform.rs
fn kill_pid_tree(pid: u32) -> bool {
    // `spawn_in_own_group` made the child its own group leader: pgid == pid.
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: a plain signal dispatch — SIGKILL to the child's own process
    // group, no state read or written.
    unsafe { libc::killpg(pgid, libc::SIGKILL) == 0 }
}

#[cfg(windows)]
fn kill_pid_tree(pid: u32) -> bool {
    use std::process::Stdio;

    let pid = pid.to_string();
    Command::new("taskkill")
        .arg("/PID")
        .arg(&pid)
        .args(["/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(any(unix, windows)))]
fn kill_pid_tree(_pid: u32) -> bool {
    false
}
