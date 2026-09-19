//! Exercise the detached updater sequence with fake installers.
//!
//! The fake installers write only to a temporary state file; the helper still
//! drives the real stop -> install -> start -> health -> rollback sequence
//! through platform-native scripts. This keeps rollback from becoming a shell
//! of untested `if` statements.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_walgit-upgrade-helper"))
}

fn base(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "walgit-upgrade-helper-{name}-{}-{stamp}",
        std::process::id()
    ))
}

fn write(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = fs::metadata(path).unwrap().permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn exited_pid() -> u32 {
    let mut child = if cfg!(windows) {
        Command::new("cmd.exe")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap()
    } else {
        Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap()
    };
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn write_health_command(base: &Path, health: &Path) -> PathBuf {
    #[cfg(unix)]
    let body = format!("#!/bin/sh\ncat '{}'\n", health.display());
    #[cfg(windows)]
    let body = format!("@echo off\r\ntype \"{}\"\r\n", health.display());
    let path = base.join(if cfg!(windows) {
        "health.cmd"
    } else {
        "health"
    });
    write(&path, &body);
    make_executable(&path);
    path
}

fn write_service_dispatcher(
    base: &Path,
    current: &Path,
    calls: &Path,
    health: &Path,
    fail_health: &Path,
) -> PathBuf {
    let path = base.join(if cfg!(windows) {
        "walgit.cmd"
    } else {
        "walgit"
    });
    #[cfg(unix)]
    let body = format!(
        "#!/bin/sh\ncurrent=\"$(cat '{}')\"\nif [ \"$current\" = new ]; then ver=0.5.1; else ver=0.5.0; fi\ncase \"${{1:-}}\" in\n  --version) echo \"walgit v$ver\" ;;\n  service)\n    case \"${{2:-}}\" in\n      stop) echo \"service stop\" >>'{}' ;;\n      start) echo \"service start\" >>'{}'; health=\"$ver\"; if [ -e '{}' ] && [ \"$current\" = new ]; then health=9.9.9; fi; printf 'v%s\\n' \"$health\" >'{}' ;;\n    esac\n    ;;\nesac\nexit 0\n",
        current.display(),
        calls.display(),
        calls.display(),
        fail_health.display(),
        health.display()
    );
    #[cfg(windows)]
    let body = format!(
        "@echo off\r\nset /p CURRENT=<\"{}\"\r\nif \"%CURRENT%\"==\"new\" (set VER=0.5.1) else (set VER=0.5.0)\r\nif \"%1\"==\"--version\" (echo walgit v%VER% & exit /b 0)\r\nif \"%1\"==\"service\" if \"%2\"==\"stop\" echo service stop>>\"{}\"\r\nif \"%1\"==\"service\" if \"%2\"==\"start\" set HEALTHVER=%VER%\r\nif exist \"{}\" if \"%CURRENT%\"==\"new\" set HEALTHVER=9.9.9\r\nif \"%1\"==\"service\" if \"%2\"==\"start\" (echo service start>>\"{}\" & echo v%HEALTHVER%>\"{}\")\r\nexit /b 0\r\n",
        current.display(),
        calls.display(),
        fail_health.display(),
        calls.display(),
        health.display()
    );
    #[cfg(not(any(unix, windows)))]
    let body = String::new();
    write(&path, &body);
    make_executable(&path);
    path
}

fn write_installer(
    base: &Path,
    name: &str,
    marker: &str,
    current: &Path,
    calls: &Path,
) -> (PathBuf, String) {
    #[cfg(unix)]
    let body = format!(
        "#!/bin/sh\necho 'install {marker}' >>'{}'\nprintf '{marker}\\n' >'{}'\n",
        calls.display(),
        current.display()
    );
    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho install {marker}>>\"{}\"\r\necho {marker}>\"{}\"\r\n",
        calls.display(),
        current.display()
    );
    #[cfg(not(any(unix, windows)))]
    let body = String::new();
    let path = base.join(if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_string()
    });
    write(&path, &body);
    make_executable(&path);
    (path, sha256(&body))
}

fn write_tray(base: &Path, marker: &Path) -> PathBuf {
    let path = base.join(if cfg!(windows) { "tray.cmd" } else { "tray" });
    #[cfg(unix)]
    let body = format!("#!/bin/sh\necho launched >'{}'\n", marker.display());
    #[cfg(windows)]
    let body = format!("@echo off\r\necho launched>\"{}\"\r\n", marker.display());
    #[cfg(not(any(unix, windows)))]
    let body = String::new();
    write(&path, &body);
    make_executable(&path);
    path
}

fn run_helper(base: &Path, fail_health: bool) -> (std::process::Output, PathBuf) {
    let install = base.join("install");
    let state = base.join("state");
    let calls = state.join("calls.txt");
    let current = state.join("current.txt");
    let health = state.join("health.txt");
    let fail_marker = state.join("fail-health");
    let update = state.join("update").join("staging");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&update).unwrap();
    write(&current, "old\n");
    write(
        &state.join("walgit.toml"),
        "[server]\nlisten = \"127.0.0.1:9\"\n",
    );
    if fail_health {
        write(&fail_marker, "1\n");
    }

    let (new_installer, new_sha) =
        write_installer(&update, "new-installer", "new", &current, &calls);
    let (old_installer, old_sha) =
        write_installer(&update, "old-installer", "old", &current, &calls);
    let service = write_service_dispatcher(base, &current, &calls, &health, &fail_marker);
    let health_command = write_health_command(base, &health);
    let tray_marker = state.join("tray-launched.txt");
    let tray = write_tray(base, &tray_marker);

    let helper = update.join(if cfg!(windows) {
        "walgit-upgrade-helper.exe"
    } else {
        "walgit-upgrade-helper"
    });
    fs::copy(bin(), &helper).unwrap();
    make_executable(&helper);

    let mut command = Command::new(&helper);
    command
        .arg("--new-installer")
        .arg(new_installer)
        .arg("--rollback-installer")
        .arg(old_installer)
        .arg("--new-sha256")
        .arg(new_sha)
        .arg("--rollback-sha256")
        .arg(old_sha)
        .arg("--target-version")
        .arg("0.5.1")
        .arg("--rollback-version")
        .arg("0.5.0")
        .arg("--install-dir")
        .arg(&install)
        .arg("--state-dir")
        .arg(&state)
        .arg("--update-dir")
        .arg(&update)
        .arg("--log")
        .arg(state.join("tray.log"))
        .arg("--tray-pid")
        .arg(exited_pid().to_string())
        .arg("--tray-wait-ms")
        .arg("25")
        .arg("--health-wait-ms")
        .arg(if fail_health { "150" } else { "1000" })
        .arg("--service-bin")
        .arg(service)
        .arg("--tray-bin")
        .arg(tray)
        .arg("--health-command")
        .arg(health_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !tray_marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !tray_marker.exists() {
        let log = fs::read_to_string(state.join("tray.log")).unwrap_or_default();
        let calls_text = fs::read_to_string(&calls).unwrap_or_default();
        panic!(
            "timed out waiting for {}; status={:?} stdout={:?} stderr={:?} calls={calls_text:?} log={log:?}",
            tray_marker.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(calls.exists(), "helper did not invoke service/installer");
    (output, update)
}

fn wait_for_removed(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !path.exists(),
        "staging directory was not removed: {}",
        path.display()
    );
}

fn assert_staging_retained(path: &Path) {
    assert!(
        path.exists(),
        "failed upgrade must retain staging: {}",
        path.display()
    );
    let names: Vec<String> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|name| name.contains("new-installer")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|name| name.contains("old-installer")),
        "{names:?}"
    );
}

#[test]
fn helper_sequences_success_and_rollback_with_fake_installers() {
    let success = base("success");
    let (output, update) = run_helper(&success, false);
    assert!(output.status.success(), "success helper failed: {output:?}");
    wait_for_removed(&update);
    let calls = fs::read_to_string(success.join("state/calls.txt")).unwrap();
    let stop = calls.find("service stop").expect("stop");
    let install = calls.find("install new").expect("new installer");
    let start = calls.find("service start").expect("start");
    assert!(
        stop < install && install < start,
        "wrong success order: {calls}"
    );
    assert!(
        !calls.contains("install old"),
        "unexpected rollback: {calls}"
    );

    let rollback = base("rollback");
    let (output, update) = run_helper(&rollback, true);
    assert!(
        !output.status.success(),
        "rollback case unexpectedly succeeded"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_staging_retained(&update);
    let calls = fs::read_to_string(rollback.join("state/calls.txt")).unwrap();
    let first_stop = calls.find("service stop").expect("first stop");
    let new_install = calls.find("install new").expect("new installer");
    let first_start = calls.find("service start").expect("first start");
    let second_stop = calls[first_start + 1..]
        .find("service stop")
        .map(|offset| first_start + 1 + offset)
        .expect("rollback stop");
    let old_install = calls.find("install old").expect("old installer");
    let final_start = calls[old_install + 1..]
        .find("service start")
        .map(|offset| old_install + 1 + offset)
        .expect("final start");
    assert!(
        first_stop < new_install
            && new_install < first_start
            && first_start < second_stop
            && second_stop < old_install
            && old_install < final_start,
        "wrong rollback order: {calls}"
    );
    let log = fs::read_to_string(rollback.join("state/tray.log")).unwrap();
    assert!(
        log.contains("rollback complete"),
        "rollback not logged: {log}"
    );
}
