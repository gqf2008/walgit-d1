//! `walgit service` — start/stop/status/restart the local server.
//!
//! Liveness is the **binary's** job: the tray and the terminal share this one
//! implementation, and there is no separate shell supervisor. State is a
//! pidfile under the deployment home (`~/.walgit/walgit.pid`) plus the
//! `/healthz` probe; the log is `~/.walgit/server.log` (appended, rotated at
//! 32 MiB, never truncated out from under a running server).
//!
//! **Windows is different (D48):** the Task Scheduler owns the process, not a
//! pidfile. `start` runs a named task (`MultipleInstancesPolicy=IgnoreNew`, so
//! a second `start` can never fork a second server), `stop` ends it and then
//! verifies the port is really free, and `status` reports the task state next
//! to the version the serving process actually answers with. The old shape —
//! spawn a detached child and write *its* pid — is what let a stale pidfile
//! orphan a live server.

use std::path::Path;
#[cfg(not(windows))]
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
    let log = home.join("server.log");
    let listen = cfg.server.listen.to_string();

    match action {
        ServiceAction::Status => status(&listen, &home).await,
        ServiceAction::Stop => stop(&listen, &home).await,
        ServiceAction::Start => start(&config, &listen, &home, &log).await,
        ServiceAction::Restart => {
            stop(&listen, &home).await?;
            start(&config, &listen, &home, &log).await
        }
    }
}

/// `<home>/walgit.pid` — the pidfile the macOS/Linux supervisor writes.
#[cfg(not(windows))]
fn pidfile(home: &Path) -> std::path::PathBuf {
    home.join("walgit.pid")
}

#[cfg(not(windows))]
async fn status(listen: &str, home: &Path) -> Result<()> {
    let pidfile = &pidfile(home);
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

#[cfg(not(windows))]
async fn stop(listen: &str, home: &Path) -> Result<()> {
    let pidfile = &pidfile(home);
    let Some(pid) = read_pid(pidfile) else {
        if healthy(listen).await {
            bail!("walgit: serving but no pidfile at {} — stop it by hand", pidfile.display());
        }
        println!("walgit: not running — http://{listen}");
        return Ok(());
    };
    if !process_alive(pid) {
        let _ = std::fs::remove_file(pidfile);
        // A stale pid is not proof the port is free: a server whose pidfile was
        // overwritten by another (or older) supervisor keeps answering. Saying
        // "not running" here is how a live server became un-stoppable.
        if healthy(listen).await {
            bail!(
                "walgit: pid {pid} is gone but http://{listen} still answers — a server is running \
                 without a valid pidfile; find and stop it by hand (e.g. \
                 `lsof -tiTCP:<port> -sTCP:LISTEN | xargs kill`)"
            );
        }
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

#[cfg(not(windows))]
async fn start(config: &Path, listen: &str, home: &Path, log: &Path) -> Result<()> {
    let pidfile = &pidfile(home);
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
    detach_process(&mut cmd);
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

/// Put the server in its own session/process group so the short-lived
/// `walgit service start` process and the shell/tray that invoked it can exit
/// without reaping the server.
///
/// macOS/Linux only: on Windows the *Task Scheduler* creates the process (D48),
/// so there is nothing to detach.
#[cfg(not(windows))]
fn detach_process(cmd: &mut std::process::Command) {
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs after fork and before exec in the child. The
        // closure only calls the async-signal-safe libc::setsid and constructs
        // an io::Error on failure; it does not touch Rust allocator/shared state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
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

#[cfg(not(windows))]
fn read_pid(pidfile: &Path) -> Option<u32> {
    std::fs::read_to_string(pidfile)
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// One GET /healthz over a bare TCP socket: the CLI owes nothing to an HTTP
/// client dependency for a liveness probe. `Some(body)` iff the server answered
/// 200 — the body carries the version, which is how "an *old* server is still
/// holding the port" stops hiding behind a plain "already running".
async fn healthz_body(listen: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let connect = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(listen),
    );
    let Ok(Ok(mut stream)) = connect.await else {
        return None;
    };
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n");
    if tokio::time::timeout(Duration::from_secs(2), stream.write_all(req.as_bytes()))
        .await
        .is_err()
    {
        return None;
    }
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).to_string();
    if !text.lines().next().is_some_and(|line| line.contains(" 200")) {
        return None;
    }
    Some(text.split_once("\r\n\r\n").map_or(text.clone(), |(_, b)| b.to_string()))
}

async fn healthy(listen: &str) -> bool {
    healthz_body(listen).await.is_some()
}

/// `"version":"v0.7.2"` out of a `/healthz` body (whitespace tolerated — the
/// JSON is produced by two different serializers).
#[cfg(any(windows, test))]
fn version_of(body: &str) -> Option<String> {
    let i = body.find("\"version\"")?;
    let rest = &body[i + "\"version\"".len()..];
    let rest = &rest[rest.find(':')? + 1..];
    let start = rest.find('"')? + 1;
    let rest = &rest[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The build the *release* workflow stamped into every crate it compiled, when
/// this binary has one (a dev build has neither and we stay quiet).
#[cfg(any(windows, test))]
fn own_build() -> Option<&'static str> {
    option_env!("WALGIT_BUILD_SHA").filter(|s| !s.is_empty())
}

/// `Some(own)` when the port is answered by a *different* build — the shape of
/// "I reinstalled but it still reports the old version". Split from the printing
/// so the rule itself is testable.
#[cfg(any(windows, test))]
fn mismatch_with<'a>(running: &str, own: Option<&'a str>) -> Option<&'a str> {
    let own = own?;
    // Exact: `/healthz` echoes the build string it was compiled with, so a
    // substring test would call `v0.7.20` a match for `v0.7.2`.
    (running.trim() != own).then_some(own)
}

/// Say it out loud: a bare "already running" is exactly what let an old build
/// keep serving after an upgrade.
#[cfg(any(windows, test))]
fn warn_version_mismatch(running: &str) {
    if let Some(own) = mismatch_with(running, own_build()) {
        eprintln!(
            "walgit: 警告 — 端口上在跑的是 {running}，而本二进制是 {own}：\
             很可能是升级前的老进程没退。先 `walgit service stop`（必要时按端口找 owner 杀掉）再 start。"
        );
    }
}

#[cfg(unix)]
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

/// `<home>/.r2-credentials` as KEY=VALUE (`export ` tolerated): the object
/// store credentials the server used to get from a sourced shell script.
#[cfg(not(windows))]
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

// ---------------------------------------------------------------------------
// Windows: the Task Scheduler owns the process (D48)
//
// The old shape — spawn a detached child, write *its* pid, later kill that pid —
// had two failure modes that cost a user an afternoon:
//
//   * `start` never asked whether something already served the port, so a
//     second server was spawned, died on bind, and its (already dead) pid went
//     into the pidfile — orphaning the live server;
//   * every later `stop` matched nothing and reported success/failure while the
//     real server kept holding :8081, so `/healthz` kept reporting the *old*
//     build after an upgrade.
//
// A named task removes the identity problem: the scheduler owns exactly one
// instance, `start` is `schtasks /Run`, `stop` is `schtasks /End`, and
// `MultipleInstancesPolicy=IgnoreNew` turns a double `start` into a no-op
// instead of a fork.
// ---------------------------------------------------------------------------

/// Name of the scheduled task that carries the Windows server process.
#[cfg(windows)]
const TASK_NAME: &str = "walgit";

#[cfg(windows)]
async fn status(listen: &str, _home: &Path) -> Result<()> {
    let version = healthz_body(listen).await.and_then(|b| version_of(&b));
    let state = task::state();
    match version {
        Some(v) => {
            println!("walgit: running (version {v}, task {TASK_NAME}: {state}) — http://{listen}");
            warn_version_mismatch(&v);
        }
        None => println!("walgit: not running (task {TASK_NAME}: {state}) — http://{listen}"),
    }
    Ok(())
}

#[cfg(windows)]
async fn start(config: &Path, listen: &str, home: &Path, log: &Path) -> Result<()> {
    // Ask first: spawning blind is what forked a second server and clobbered
    // the pidfile. A healthy port means the job is already done.
    if let Some(v) = healthz_body(listen).await.and_then(|b| version_of(&b)) {
        println!("walgit: already running (version {v}) — http://{listen}");
        warn_version_mismatch(&v);
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating the walgit binary")?;
    rotate_log(log);
    task::ensure(&exe, config, log, home)?;
    task::run_now()?;
    for _ in 0..40 {
        if healthy(listen).await {
            println!("walgit: started (task {TASK_NAME}) — http://{listen}");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "walgit: 启动失败（task {TASK_NAME}），日志尾部：\n{}",
        tail(log, 5).unwrap_or_default()
    )
}

#[cfg(windows)]
async fn stop(listen: &str, _home: &Path) -> Result<()> {
    // `/End` first — it takes the task's whole tree in one shot. Its failure is
    // *not* fatal: the task may never have run, and a server an older install
    // left behind is not its child either. The **port** is the ground truth
    // throughout: `/healthz` can be silent while a hung process still owns the
    // socket, which is exactly the case that used to be reported as "stopped".
    let mut owners = port_owners(listen).await;
    if task::exists() {
        if let Err(e) = task::end() {
            eprintln!("walgit: `schtasks /End` failed ({e}); falling back to the port");
        }
        if wait_port_free(listen, 20).await {
            println!("walgit: stopped");
            return Ok(());
        }
        owners = port_owners(listen).await;
    }
    if owners.is_empty() {
        println!("walgit: not running — http://{listen}");
        return Ok(());
    }
    for pid in &owners {
        if !image_is_walgit(*pid) {
            bail!(
                "walgit: http://{listen} is served by pid {pid}, which is not a walgit binary — \
                 not killing it"
            );
        }
    }
    for pid in &owners {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
    if wait_port_free(listen, 10).await {
        println!("walgit: stopped (killed {owners:?})");
        return Ok(());
    }
    bail!("walgit: stop failed — {owners:?} still hold {listen}")
}

/// Poll until nothing listens on the port.
#[cfg(windows)]
async fn wait_port_free(listen: &str, tries: u32) -> bool {
    for _ in 0..tries {
        if port_owners(listen).await.is_empty() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    port_owners(listen).await.is_empty()
}

/// The pids LISTENING on `listen`'s port, from `netstat -ano`.
#[cfg(windows)]
async fn port_owners(listen: &str) -> Vec<u32> {
    let Some(port) = listen.rsplit(':').next().and_then(|p| p.parse().ok()) else {
        return Vec::new();
    };
    let out = tokio::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    listeners_on(&String::from_utf8_lossy(&out.stdout), port)
}

/// LISTENING rows for `port` → their owning pids (the last column of
/// `netstat -ano`). Pure so the parse is unit-tested off-Windows.
#[cfg(any(windows, test))]
fn listeners_on(netstat: &str, port: u16) -> Vec<u32> {
    let suffix = format!(":{port}");
    let mut out = Vec::new();
    for line in netstat.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || !cols[0].eq_ignore_ascii_case("tcp") {
            continue;
        }
        if cols[1].ends_with(&suffix) && cols[3].eq_ignore_ascii_case("listening") {
            if let Ok(pid) = cols[cols.len() - 1].parse() {
                out.push(pid);
            }
        }
    }
    out
}

/// Is `pid` running a walgit image? Refuse to kill anything else — a reused pid
/// must never cost the user an unrelated process.
#[cfg(windows)]
fn image_is_walgit(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    let Ok(out) = out else { return false };
    image_from_tasklist(&String::from_utf8_lossy(&out.stdout))
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("walgit"))
}

/// Is this scheduled-task definition ours? Ours names a walgit binary in its
/// action (`cmd.exe /c ""…\walgit.exe" serve …"`), so a task that merely shares
/// the name `walgit` is somebody else's and must not be overwritten or deleted.
#[cfg(any(windows, test))]
fn task_is_ours(xml: &str) -> bool {
    xml.to_ascii_lowercase().contains("walgit")
}

/// First CSV field of a `tasklist /FO CSV /NH` line, unquoted.
#[cfg(any(windows, test))]
fn image_from_tasklist(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with('"'))?;
    let rest = line.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(windows)]
mod task {
    //! `schtasks` wrappers for the one named task that carries the server.

    use std::path::Path;
    use std::process::Command;

    use anyhow::{Context, Result, bail};

    use super::TASK_NAME;

    fn schtasks(args: &[&str]) -> Result<String> {
        let out = Command::new("schtasks")
            .args(args)
            .output()
            .with_context(|| format!("running schtasks {args:?}"))?;
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            bail!("schtasks {args:?} failed: {}", text.trim());
        }
        Ok(text)
    }

    /// `Running` / `Ready` / `Disabled` / `absent` — for the human readout.
    ///
    /// NOT from `schtasks /FO LIST`: those labels are localised (`状态:` on a
    /// Chinese Windows), so parsing them silently reports a *running* task as
    /// absent — and the old stop logic then decided not to end it. The
    /// scheduled-task object's `.State` is a .NET enum and stays English.
    /// Decisions do not use this at all any more; the port is the ground truth.
    pub fn state() -> String {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-ScheduledTask -TaskName '{TASK_NAME}' -ErrorAction Stop).State"),
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { "absent".to_string() } else { s }
            }
            _ => "absent".to_string(),
        }
    }

    /// Does a task with our name exist at all (without judging whose it is)?
    pub fn exists() -> bool {
        schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]).is_ok()
    }

    /// Its definition, for the ownership check below.
    fn definition() -> Option<String> {
        schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]).ok()
    }

    /// Create the task if missing, or refresh it when the install moved.
    ///
    /// XML, not `/TR`: the action carries quotes inside quotes and
    /// `Command::args` cannot express that reliably. XML also sets the two
    /// defaults that are wrong for a long-lived server — `ExecutionTimeLimit`
    /// `PT0S` (the default kills the task after 72 h) and
    /// `MultipleInstancesPolicy=IgnoreNew` (a duplicate `start` must not fork).
    pub fn ensure(exe: &Path, config: &Path, log: &Path, home: &Path) -> Result<()> {
        // `/Create /F` overwrites by name. Refuse to clobber a task that happens
        // to be called `walgit` but is somebody else's (the same care the port
        // sweep takes before it kills a pid).
        if let Some(existing) = definition()
            && !super::task_is_ours(&existing)
        {
            bail!(
                "walgit: a scheduled task named `{TASK_NAME}` already exists and does not look like \
                 ours — refusing to replace it. Rename or delete it, then retry."
            );
        }
        let xml_path = home.join("walgit-task.xml");
        std::fs::write(&xml_path, task_xml(exe, config, log)).with_context(|| {
            format!("writing the task definition to {}", xml_path.display())
        })?;
        let path = xml_path.display().to_string();
        let result = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path, "/F"]);
        let _ = std::fs::remove_file(&xml_path);
        result.map(|_| ())
    }

    pub fn run_now() -> Result<()> {
        schtasks(&["/Run", "/TN", TASK_NAME]).map(|_| ())
    }

    pub fn end() -> Result<()> {
        schtasks(&["/End", "/TN", TASK_NAME]).map(|_| ())
    }

    /// `cmd /c … >> log 2>&1` is a **redirect, not a supervisor**: the task's own
    /// job object still owns the tree, so `schtasks /End` kills the server and
    /// `MultipleInstancesPolicy` still forbids a second copy. The scheduler gives
    /// an `Exec` action no stdout, and losing `server.log` would take away the
    /// only window into a failed start.
    fn task_xml(exe: &Path, config: &Path, log: &Path) -> Vec<u8> {
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>walgit — local git server (managed by `walgit service`)</Description>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{comspec}</Command>
      <Arguments>/c ""{exe}" serve --config "{cfg}" &gt;&gt; "{log}" 2&gt;&amp;1"</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
            exe = xml_escape(&exe.display().to_string()),
            cfg = xml_escape(&config.display().to_string()),
            // The scheduler hands the task no stdout: without this the only
            // window into a failed start would be gone.
            log = xml_escape(&log.display().to_string()),
            comspec = xml_escape(&std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())),
            dir = xml_escape(&exe.parent().unwrap_or(Path::new(".")).display().to_string()),
        );
        // `schtasks /Create /XML` rejects UTF-8 on some builds: emit UTF-16LE
        // with a BOM, which every build accepts.
        let mut out = Vec::with_capacity(body.len() * 2 + 2);
        out.extend_from_slice(&[0xFF, 0xFE]);
        for unit in body.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
}

/// Parsers that the Windows lifecycle leans on. They are `cfg(any(windows,
/// test))` so the copy that ships on Windows is the copy these tests exercise —
/// the alternative (test a second, unix-only implementation) is how the
/// "wrong pid" class of bug survives a green build.
#[cfg(test)]
mod listener_parse_tests {
    use super::{image_from_tasklist, listeners_on, version_of};

    #[test]
    fn version_of_reads_both_healthz_shapes() {
        assert_eq!(
            version_of(r#"{"status":"ok","version":"v0.7.2"}"#).as_deref(),
            Some("v0.7.2")
        );
        assert_eq!(
            version_of("{\"status\": \"ok\", \"version\" : \"0.1.0+abc\"}").as_deref(),
            Some("0.1.0+abc")
        );
        assert_eq!(version_of(r#"{"status":"ok"}"#), None);
    }

    #[test]
    fn listeners_on_takes_only_the_matching_port() {
        // Shape of `netstat -ano -p tcp` on a Chinese Windows: v4 + v6 rows for
        // the port we want, a decoy on another port, and a non-LISTENING row.
        let netstat = "\
  TCP    127.0.0.1:8081         0.0.0.0:0              LISTENING       4242\r
  TCP    127.0.0.1:9999         0.0.0.0:0              LISTENING       1111\r
  TCP    127.0.0.1:8081         127.0.0.1:50812        ESTABLISHED     9999\r
  TCP    [::1]:8081             [::]:0                 LISTENING       4243\r";
        assert_eq!(listeners_on(netstat, 8081), vec![4242, 4243]);
        assert!(listeners_on(netstat, 1234).is_empty());
    }

    #[test]
    fn a_different_build_on_the_port_is_a_mismatch() {
        use super::mismatch_with;
        assert_eq!(mismatch_with("v0.7.2", Some("v0.7.2")), None);
        assert_eq!(mismatch_with("v0.7.2\n", Some("v0.7.2")), None);
        // Not a prefix match: v0.7.20 is a different build.
        assert_eq!(mismatch_with("v0.7.20", Some("v0.7.2")), Some("v0.7.2"));
        // The shape the user hit: the port still answers with the *old* build.
        assert_eq!(mismatch_with("v0.7.0", Some("v0.7.2")), Some("v0.7.2"));
        // A dev build has nothing stamped: stay quiet rather than cry wolf.
        assert_eq!(mismatch_with("v0.7.2", None), None);
        // Smoke the printing path too (silent in a dev build, which has no stamp).
        super::warn_version_mismatch("v0.7.2");
    }

    #[test]
    fn only_our_own_task_may_be_replaced() {
        use super::task_is_ours;
        // Ours: the redirect wrapper still names the binary in its arguments.
        let ours = r#"<Exec><Command>C:\Windows\System32\cmd.exe</Command>
            <Arguments>/c ""C:\Users\x\AppData\Local\Programs\walgit\walgit.exe" serve --config "…" >> "…" 2>&1"</Arguments></Exec>"#;
        assert!(task_is_ours(ours));
        // Someone else's task that happens to be called `walgit`.
        let theirs = r#"<Exec><Command>C:\tools\backup.exe</Command><Arguments>--nightly</Arguments></Exec>"#;
        assert!(!task_is_ours(theirs));
    }

    #[test]
    fn image_from_tasklist_reads_the_csv_name() {
        assert_eq!(
            image_from_tasklist(r#""walgit.exe","4242","Console","1","12,345 K""#).as_deref(),
            Some("walgit.exe")
        );
        // The localised "no tasks match" line has no CSV row: unknown, never a
        // licence to kill.
        assert_eq!(
            image_from_tasklist("信息: 没有运行的任务匹配指定标准。"),
            None
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::detach_process;

    #[test]
    fn detached_child_gets_its_own_session() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("5");
        detach_process(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = i32::try_from(child.id()).expect("child pid fits pid_t");
        // SAFETY: getsid only reads the session id for the live child process;
        // `pid` was checked to fit pid_t above.
        let sid = unsafe { libc::getsid(pid) };
        assert_eq!(sid, pid, "child must lead its own session");
        let _ = child.kill();
        let _ = child.wait();
    }
}
