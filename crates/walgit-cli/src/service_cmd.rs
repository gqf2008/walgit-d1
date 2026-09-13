//! `walgit service` — start/stop/status/restart the local server.
//!
//! Liveness is the **binary's** job: the tray and the terminal share this one
//! implementation, and there is no separate shell supervisor. State is a
//! pidfile under the deployment home (`~/.walgit/walgit.pid`) plus the
//! `/healthz` probe; the log is `~/.walgit/server.log` (appended, rotated at
//! 32 MiB, never truncated out from under a running server).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use walgit_config::Config;

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Start the server in the background if it is not already serving.
    Start,
    /// Stop it (SIGTERM, then SIGKILL after a grace period).
    Stop,
    /// Report whether it is serving, and its pid.
    Status,
    /// Stop, then start.
    Restart,
}

/// `<deploy home>/walgit.log` rotation threshold — the old script truncated
/// with `>` instead, which zeroed a *running* server's log.
const LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;

pub async fn run(action: &ServiceAction, config: &Path) -> Result<()> {
    let config = config.to_path_buf();
    let cfg = Config::load(&config)
        .with_context(|| format!("loading {}", config.display()))?;
    let home = walgit_config::deploy_home();
    std::fs::create_dir_all(&home)
        .with_context(|| format!("creating {}", home.display()))?;
    let pidfile = home.join("walgit.pid");
    let log = home.join("server.log");
    let listen = cfg.server.listen.to_string();

    match action {
        ServiceAction::Status => status(&listen, &pidfile).await,
        ServiceAction::Stop => stop(&listen, &pidfile).await,
        ServiceAction::Start => start(&config, &listen, &pidfile, &log).await,
        ServiceAction::Restart => {
            stop(&listen, &pidfile).await?;
            start(&config, &listen, &pidfile, &log).await
        }
    }
}

async fn status(listen: &str, pidfile: &Path) -> Result<()> {
    let pid = read_pid(pidfile);
    if healthy(listen).await {
        match pid {
            Some(p) => println!("walgit: running (pid {p}) — http://{listen}"),
            None => println!("walgit: running — http://{listen}"),
        }
        return Ok(());
    }
    if let Some(p) = pid
        && process_alive(p)
    {
        // The port answers nothing but the process is alive: it is up but
        // stuck (a slow I/O), which is different from "not running".
        bail!("walgit: running but unresponsive (pid {p}) — http://{listen}");
    }
    bail!("walgit: not running — http://{listen}");
}

async fn stop(listen: &str, pidfile: &Path) -> Result<()> {
    let Some(pid) = read_pid(pidfile) else {
        if healthy(listen).await {
            bail!("walgit: serving but no pidfile at {} — stop it by hand", pidfile.display());
        }
        println!("walgit: not running — http://{listen}");
        return Ok(());
    };
    if !process_alive(pid) {
        let _ = std::fs::remove_file(pidfile);
        println!("walgit: not running (stale pidfile) — http://{listen}");
        return Ok(());
    }
    signal(pid, Signal::Term);
    for _ in 0..20 {
        if !process_alive(pid) {
            let _ = std::fs::remove_file(pidfile);
            println!("walgit: stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    signal(pid, Signal::Kill);
    for _ in 0..10 {
        if !process_alive(pid) {
            let _ = std::fs::remove_file(pidfile);
            println!("walgit: stopped (killed)");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("walgit: stop failed — pid {pid} still alive (stuck in I/O?)")
}

async fn start(config: &Path, listen: &str, pidfile: &Path, log: &Path) -> Result<()> {
    if healthy(listen).await {
        match read_pid(pidfile) {
            Some(p) => println!("walgit: already running (pid {p}) — http://{listen}"),
            None => println!("walgit: already running — http://{listen}"),
        }
        return Ok(());
    }
    rotate_log(log);
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let exe = std::env::current_exe().context("locating the walgit binary")?;
    let mut cmd = std::process::Command::new(&exe);
    // `walgit-server` is the standalone `serve` binary: it takes no subcommand.
    if !exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("walgit-server"))
    {
        cmd.arg("serve");
    }
    cmd.arg("--config").arg(config);
    // The old `run-walgit.sh` used to `source` `<home>/.r2-credentials` before
    // exec'ing the server; the binary does it now, so there is no shell in the
    // path (the config reaches these keys through `access_key_env`).
    for (k, v) in credential_env(&walgit_config::deploy_home()) {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    let child = cmd.spawn().context("spawning walgit serve")?;
    std::fs::write(pidfile, format!("{}\n", child.id()))
        .with_context(|| format!("writing {}", pidfile.display()))?;
    for _ in 0..40 {
        if healthy(listen).await {
            println!("walgit: started (pid {}) — http://{listen}", child.id());
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = std::fs::remove_file(pidfile);
    bail!(
        "walgit: 启动失败，日志尾部：\n{}",
        tail(log, 5).unwrap_or_default()
    )
}

/// Append-only log that rotates rather than truncating: `>` used to zero a
/// running server's log out from under it (a sparse 100 MB+ file).
fn rotate_log(log: &Path) {
    if let Ok(meta) = std::fs::metadata(log)
        && meta.len() > LOG_ROTATE_BYTES
    {
        let _ = std::fs::rename(log, log.with_extension("log.1"));
    }
}

fn tail(path: &Path, lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut all: Vec<&str> = text.lines().collect();
    let tail = all.split_off(all.len().saturating_sub(lines));
    Some(tail.join("\n"))
}

fn read_pid(pidfile: &Path) -> Option<u32> {
    std::fs::read_to_string(pidfile)
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// One GET /healthz over a bare TCP socket: the CLI owes nothing to an HTTP
/// client dependency for a liveness probe.
async fn healthy(listen: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let connect = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(listen),
    );
    let Ok(Ok(mut stream)) = connect.await else {
        return false;
    };
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n");
    if tokio::time::timeout(Duration::from_secs(2), stream.write_all(req.as_bytes()))
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200"))
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal(pid: u32, sig: Signal) {
    let flag = match sig {
        Signal::Term => "-TERM",
        Signal::Kill => "-KILL",
    };
    let _ = std::process::Command::new("kill")
        .arg(flag)
        .arg(pid.to_string())
        .status();
}

#[cfg(windows)]
fn signal(pid: u32, sig: Signal) {
    // Windows has no SIGTERM: `/F` is the only reliable stop, and the SIGTERM
    // step above becomes a graceful `taskkill /PID` (which posts WM_CLOSE to
    // GUI apps and terminates a console app).
    let mut cmd = std::process::Command::new("taskkill");
    cmd.arg("/PID").arg(pid.to_string());
    if matches!(sig, Signal::Kill) {
        cmd.arg("/F");
    }
    let _ = cmd.status();
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // `kill -0` is the portable "does this pid exist" (and on Unix it also
    // reaches a zombie's status through its parent, which is what we want).
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

/// `<home>/.r2-credentials` as KEY=VALUE (`export ` tolerated): the object
/// store credentials the server used to get from a sourced shell script.
fn credential_env(home: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(home.join(".r2-credentials")) else {
        return out;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        out.push((
            k.to_string(),
            v.trim().trim_matches(|c| c == '"' || c == '\'').to_string(),
        ));
    }
    out
}

