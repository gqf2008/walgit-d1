//! Test-only plumbing shared by the CLI's `#[cfg(test)]` modules: the two-stage
//! git pipeline the suites used to spell through `/bin/sh`, which does not exist
//! on Windows (and every integration binary of a workspace crate is its own
//! world, so this small helper is duplicated rather than exported as API).

#![cfg(test)]

/// Whether this account may create a symlink in `dir` at all: NTFS needs
/// Developer Mode or a privilege. Detect-and-report (never silently): a false
/// return says exactly why, mirroring the probe in `walgit-wal/tests/wal.rs`.
pub(crate) fn symlinks_available(dir: &std::path::Path) -> bool {
    let target = dir.join(".symlink-probe-target");
    let link = dir.join(".symlink-probe-link");
    let _ = std::fs::remove_file(&link);
    match std::fs::write(&target, b"probe") {
        Ok(()) => {}
        Err(e) => {
            eprintln!("skipped (probe target unwritable: {e})");
            return false;
        }
    }
    let created = walgit_wal::platform::symlink(&target, &link);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&target);
    match created {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "skipped: creating symlinks failed ({e}); Windows needs Developer Mode or administrator rights"
            );
            false
        }
    }
}

/// `git <first> | git <second>` without a POSIX shell. Collects the first
/// process's whole output, then feeds it to the second through its stdin:
/// suites here exchange small packs, and a buffered hop avoids both pipe-volume
/// deadlocks and the platform quirks of piping two live children directly
/// (observed flaky on Windows: an instantly-empty read from a healthy upstream).
/// A failure names its cause instead of yielding a silent empty pack.
pub(crate) fn git_pipe(
    cwd: &std::path::Path,
    first: &[&str],
    second: &[&str],
) -> std::process::Output {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let up = Command::new("git")
        .args(first)
        .current_dir(cwd)
        .stderr(Stdio::piped())
        .output()
        .expect("run upstream git");
    assert!(up.status.success(), 
        "git {first:?} failed: {} — stderr: {}",
        up.status,
        String::from_utf8_lossy(&up.stderr)
    );

    let mut down = Command::new("git")
        .args(second)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn downstream git");
    // The whole input is already in memory: write it, close stdin, then wait.
    // A write error means the child died early — its status says how.
    if let Some(mut stdin) = down.stdin.take() {
        let _ = stdin.write_all(&up.stdout);
    }
    let out = down.wait_with_output().expect("wait downstream git");
    assert!(out.status.success(), 
        "git {second:?} failed: {} — stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
