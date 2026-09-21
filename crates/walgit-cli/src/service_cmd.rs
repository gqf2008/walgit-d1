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
#[cfg(any(windows, test))]
use base64::Engine as _;
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
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
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

/// `setsid(2)` for a `pre_exec` child: async-signal-safe, no allocation, no
/// shared state — callable between fork and exec.
#[cfg(not(windows))]
#[allow(unsafe_code)] // setsid between fork and exec — the platform-seam exception.
fn child_setsid() -> std::io::Result<()> {
    // SAFETY: async-signal-safe libc call after fork and before exec in the
    // child; it touches no Rust allocator or shared state.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Put the server in its own session/process group so the short-lived
/// `walgit service start` process and the shell/tray that invoked it can exit
/// without reaping the server.
///
/// macOS/Linux only: on Windows the *Task Scheduler* creates the process (D48),
/// so there is nothing to detach.
#[cfg(not(windows))]
#[allow(unsafe_code)] // pre_exec is unsafe — the same platform-seam exception as `proc_group.rs`.
fn detach_process(cmd: &mut std::process::Command) {
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs after fork and before exec in the child; the
        // closure only calls the async-signal-safe helper above.
        unsafe {
            cmd.pre_exec(child_setsid);
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
    if !text
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200"))
    {
        return None;
    }
    Some(
        text.split_once("\r\n\r\n")
            .map_or(text.clone(), |(_, b)| b.to_string()),
    )
}

async fn healthy(listen: &str) -> bool {
    healthz_body(listen).await.is_some()
}

/// `"version":"v0.7.2"` out of a `/healthz` body (whitespace tolerated — the
/// JSON is produced by two different serializers).
#[cfg(any(windows, test))]
fn version_of(body: &str) -> Option<String> {
    let (_, rest) = body.split_once("\"version\"")?;
    let (_, rest) = rest.split_once(':')?;
    let rest = rest.get(rest.find('"')? + 1..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_string)
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
#[cfg(any(windows, test))]
const TASK_NAME: &str = "walgit";

#[cfg(any(windows, test))]
const POWERSHELL_SERVICE_MARKER: &str = "walgit-service-task-v1";

/// The windowless launcher the task action runs. It ships next to `walgit.exe`
/// and is started as a GUI-subsystem process, so no console is ever created —
/// see `deploy/tray/tray-rs/src/service_host.rs`.
#[cfg(any(windows, test))]
const SERVICE_HOST_IMAGE: &str = "walgit-service-host.exe";

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
    // The **port** is the ground truth and the owners are what actually hold it:
    // `/End` alone is not enough (measured on the CI runner — the task's `cmd /c`
    // wrapper is ended while the server child keeps the socket). So kill the
    // owners, verify the port is really free, and only then tidy up the task the
    // scheduler still tracks — and only when that task is provably ours.
    let owners = port_owners(listen).await?;
    if owners.is_empty() {
        end_task_if_ours()?;
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
    if !wait_port_free(listen, 20).await? {
        bail!("walgit: stop failed — {owners:?} still hold {listen}")
    }
    end_task_if_ours()?;
    println!("walgit: stopped (killed {owners:?})");
    Ok(())
}

/// `/End` the task, but only when it is provably ours — and then **verify** it
/// stopped. Doubt is a reason to do nothing (and say so), never to end somebody
/// else's task; a task left `Running` is a reason to fail loudly, because
/// `MultipleInstancesPolicy=IgnoreNew` would make the next `start` a no-op.
/// A transient query failure is retried, but persistent doubt still fails the
/// command: `stop` must never report success while ownership is unknown.
#[cfg(any(windows, test))]
const TASK_PROBE_ATTEMPTS: usize = 5;
#[cfg(any(windows, test))]
const TASK_PROBE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[cfg(windows)]
fn end_task_if_ours() -> Result<()> {
    end_task_if_ours_with(task::probe, task::end, task::is_running)
}

#[cfg(any(windows, test))]
fn end_task_if_ours_with(
    mut probe: impl FnMut() -> task::Task,
    mut end: impl FnMut() -> Result<()>,
    mut is_running: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let mut existing = task::Task::Unknown;
    for attempt in 0..TASK_PROBE_ATTEMPTS {
        existing = probe();
        if existing != task::Task::Unknown {
            break;
        }
        if attempt + 1 < TASK_PROBE_ATTEMPTS {
            std::thread::sleep(TASK_PROBE_RETRY_DELAY);
        }
    }

    match existing {
        task::Task::Ours => {
            end()?;
            for _ in 0..20 {
                if !is_running()? {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            bail!(
                "walgit: the `{TASK_NAME}` task is still running after `/End` — \
                 the scheduler would refuse the next start"
            )
        }
        task::Task::Foreign => {
            eprintln!(
                "walgit: a scheduled task named `{TASK_NAME}` exists but is not ours — leaving it alone"
            );
            Ok(())
        }
        task::Task::Unknown => {
            bail!(
                "walgit: could not determine who owns the `{TASK_NAME}` task after \
                 {TASK_PROBE_ATTEMPTS} attempts — refusing to report stop success"
            )
        }
        task::Task::Absent => Ok(()),
    }
}

/// Poll until nothing listens on the port.
#[cfg(windows)]
async fn wait_port_free(listen: &str, tries: u32) -> Result<bool> {
    for _ in 0..tries {
        if port_owners(listen).await?.is_empty() {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(port_owners(listen).await?.is_empty())
}

/// The pids LISTENING on `listen`'s port, from `netstat -ano`.
///
/// `Err` when the query itself could not be answered — a failed lookup is *not*
/// evidence that the port is free, and treating it as such is how `stop` came to
/// report success over a live server.
#[cfg(windows)]
async fn port_owners(listen: &str) -> Result<Vec<u32>> {
    let port: u16 = listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .with_context(|| format!("cannot derive a port from `{listen}`"))?;
    let out = tokio::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .await
        .context("running netstat")?;
    if !out.status.success() {
        bail!("netstat exited with {}", out.status);
    }
    listeners_on(&String::from_utf8_lossy(&out.stdout), listen, port).map_err(anyhow::Error::msg)
}

/// LISTENING rows for **our** `listen` address → their owning pids (the last
/// column of `netstat -ano`). Pure so the parse is unit-tested off-Windows.
///
/// Scoping to the address, not just the port number, is load-bearing: a socket
/// bound to another local address is a *different* socket. Matching on the port
/// alone meant any unrelated process that happened to use the same port number —
/// a dev server, a gateway on a VPN address — was reported as "the owner of our
/// port", and `stop` then refused to stop a perfectly healthy server (and the
/// installer's port proof never passed). Reported 2026-09-19, thread
/// `cc-ai-win-port-owner-scope`.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn listeners_on(netstat: &str, listen: &str, port: u16) -> Result<Vec<u32>, String> {
    let suffix = format!(":{port}");
    let ours = listen_host(listen);
    let mut out = Vec::new();
    for line in netstat.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || !cols[0].eq_ignore_ascii_case("tcp") {
            continue;
        }
        if !cols[3].eq_ignore_ascii_case("listening") || !cols[1].ends_with(&suffix) {
            continue;
        }
        // `[::1]:8081` → `::1`, `127.0.0.1:8081` → `127.0.0.1`.
        let host = cols[1][..cols[1].len() - suffix.len()]
            .trim_matches(['[', ']']);
        if !same_bind(host, &ours) {
            continue;
        }
        // The row *is* a listener on our port: failing to read its pid means we
        // cannot tell who owns it, and "cannot tell" must never be reported as
        // "nothing is listening" (that is how `stop` claimed success over a live
        // server). A line we could not match at all is simply not our row.
        match cols[cols.len() - 1].parse::<u32>() {
            Ok(pid) => out.push(pid),
            Err(_) => {
                return Err(format!(
                    "cannot read the owning pid from this netstat row: `{}`",
                    line.trim()
                ));
            }
        }
    }
    Ok(out)
}

/// The host part of a `host:port` listen address (`[::1]:8081` → `::1`).
#[cfg(any(windows, test))]
fn listen_host(listen: &str) -> String {
    listen.rsplit_once(':').map_or_else(
        || listen.trim_matches(['[', ']']).to_ascii_lowercase(),
        |(host, _)| host.trim_matches(['[', ']']).to_ascii_lowercase(),
    )
}

/// Can a socket bound to `row` be holding the port we are about to use on
/// `ours`? Same address, the other loopback family (`walgit` binds the `::1`
/// twin of `127.0.0.1`), or a wildcard bind — a wildcard really does own every
/// address of that port.
#[cfg(any(windows, test))]
fn same_bind(row: &str, ours: &str) -> bool {
    let row = row.to_ascii_lowercase();
    let ours = ours.to_ascii_lowercase();
    if row == ours {
        return true;
    }
    let wildcard = |host: &str| matches!(host, "0.0.0.0" | "::" | "*");
    if wildcard(&row) || wildcard(&ours) {
        return true;
    }
    let loopback = |host: &str| matches!(host, "127.0.0.1" | "::1" | "localhost");
    loopback(&row) && loopback(&ours)
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
        .is_some_and(|name| is_our_image(&name))
}

/// The images that *are* this product. A prefix match would happily kill
/// `walgit-backup.exe` — someone else's process that merely starts with the name.
#[cfg(any(windows, test))]
fn is_our_image(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    matches!(name.as_str(), "walgit.exe" | "walgit-server.exe")
}

/// Is this scheduled-task definition ours? **Only the action counts**: our task
/// runs `walgit.exe` directly, through the old `cmd /c … >> log 2>&1` wrapper,
/// through v0.7.7's hidden encoded PowerShell wrapper, or through the windowless
/// `walgit-service-host.exe` that replaced it. A definition that merely mentions
/// walgit in its description or arguments, or runs `walgit-backup.exe`, is not
/// ours. Getting this wrong means ending or deleting somebody else's task.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn task_is_ours(xml: &str) -> bool {
    // Only the **action** counts, and only where a command lives: a task whose
    // *description* mentions walgit, or whose action merely passes its path to
    // an unrelated program, is somebody else's — and `ensure`/`end` would
    // otherwise `/Create /F` over it or end it.
    let lower = xml.to_ascii_lowercase();
    let Some(actions_at) = lower.find("<actions") else {
        return false;
    };
    // Keep the original text: `-EncodedCommand` is base64, so lowercasing it
    // would corrupt the very value this parser needs to decode. The lowercased
    // copy is used only for case-insensitive tag lookup.
    let actions = &xml[actions_at..];
    let commands = xml_tag_values(actions, "command");
    let arguments = xml_tag_values(actions, "arguments");
    commands.iter().enumerate().any(|(i, command)| {
        action_is_ours(
            command.trim(),
            arguments.get(i).copied().unwrap_or_default().trim(),
        )
    })
}

/// One `<Exec>` action: does it launch *our* server?
///
/// Every shape we have ever generated stays acceptable — the old visible `cmd /c`
/// wrapper, v0.7.7's hidden PowerShell wrapper, and the current windowless host —
/// because `ensure` must be able to `/Create /F` over whatever task a previous
/// version left behind. Dropping a shape here would leave an upgradable machine
/// with a task that `ensure` refuses to touch.
#[cfg(any(windows, test))]
fn action_is_ours(command: &str, args: &str) -> bool {
    // The binary itself…
    if is_our_image(command) || ends_with_our_image(command) {
        return true;
    }
    // …or our redirect wrapper runs it at the command position after `/c`.
    if is_cmd_wrapper(command) {
        return cmd_runs_our_binary(args);
    }
    if !is_encoded_wrapper(command) {
        return false;
    }
    let Some(payload) = encoded_payload(args) else {
        return false;
    };
    // The payload must be *our* launch command — one of our exact images plus the
    // `serve --config` and `>> … 2>&1` logging shape — before the wrapper matters.
    if !(payload.contains("serve --config")
        && payload.contains("2>&1")
        && script_has_our_image(&payload))
    {
        return false;
    }
    if is_powershell_wrapper(command) {
        // v0.7.7's shape: a PowerShell *script* emitted by `task_xml`, so it
        // carries the marker. A PowerShell action that merely mentions our path
        // stays somebody else's.
        return payload.contains(POWERSHELL_SERVICE_MARKER) && payload.contains("cmd /c");
    }
    // The windowless host hands the payload to `cmd /d /s /c`, so the payload is
    // the *inner* command and the checks above are the whole predicate: one of our
    // exact images plus the `serve --config … >> … 2>&1` logging shape. Its image
    // is a name only this product ships, which is why the marker that guards the
    // generic PowerShell wrapper is not needed for it.
    true
}

/// Values of every `<tag>…</tag>` pair in `xml`, found case-insensitively while
/// preserving the original value bytes (base64 is case-sensitive).
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn xml_tag_values<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let lower = xml.to_ascii_lowercase();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut values = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find(&open) {
        let start = at + found + open.len();
        let Some(end) = lower[start..].find(&close) else {
            break;
        };
        values.push(&xml[start..start + end]);
        at = start + end + close.len();
    }
    values
}

/// `C:\…\walgit.exe` → true (and `evilwalgit.exe` → false).
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn ends_with_our_image(token: &str) -> bool {
    let token = token.trim_matches(['"', '\'']);
    if token.chars().any(char::is_whitespace) && !looks_like_windows_path(token) {
        return false;
    }
    let base = token.rsplit(['\\', '/']).next().unwrap_or(token);
    is_our_image(base)
}

/// A quoted command token may contain spaces, but it must still look like a
/// path — not a sentence such as `echo C:\tools\walgit.exe`.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn looks_like_windows_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || token.starts_with(r"\\")
        || token.starts_with(r".\")
        || token.starts_with("./")
        || token.starts_with(r"..\")
        || token.starts_with("../")
}

/// Does `cmd.exe`'s action text run one of our binaries in command position?
///
/// `cmd /c echo C:\...\walgit.exe` merely mentions our path; it does not run
/// it. Only the first token after `/c` may identify the task as ours.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn cmd_runs_our_binary(args: &str) -> bool {
    cmd_command_token(args).is_some_and(ends_with_our_image)
}

/// The command token `cmd.exe` runs after `/c` (`/d`/`/s` may precede it).
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn cmd_command_token(args: &str) -> Option<&str> {
    let mut rest = args.trim_start();
    loop {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..end];
        rest = rest[end..].trim_start();
        if token.eq_ignore_ascii_case("/c") {
            return first_cmd_token(rest);
        }
        if !token.starts_with('/') {
            return None;
        }
    }
}

/// Parse the first command token, including the usual `cmd /c ""…" args"`
/// quoting shape and quoted paths that contain spaces.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn first_cmd_token(args: &str) -> Option<&str> {
    let args = args.trim_start();
    if let Some(rest) = args.strip_prefix('"')
        && rest.starts_with('"')
    {
        return quoted_token(rest);
    }
    if args.starts_with('"') {
        return quoted_token(args);
    }
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    (!args[..end].is_empty()).then_some(&args[..end])
}

#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn quoted_token(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(any(windows, test))]
fn is_cmd_wrapper(command: &str) -> bool {
    let base = command.rsplit(['\\', '/']).next().unwrap_or(command);
    matches!(base.trim_matches('"'), "cmd.exe" | "cmd")
}

#[cfg(any(windows, test))]
fn is_powershell_wrapper(command: &str) -> bool {
    let base = command.rsplit(['\\', '/']).next().unwrap_or(command);
    matches!(base.trim_matches('"'), "powershell.exe" | "pwsh.exe")
}

/// The windowless service host — the wrapper the task uses now.
#[cfg(any(windows, test))]
fn is_service_host_wrapper(command: &str) -> bool {
    let base = command.rsplit(['\\', '/']).next().unwrap_or(command);
    base.trim_matches('"')
        .eq_ignore_ascii_case(SERVICE_HOST_IMAGE)
}

/// Both hidden wrappers carry the launch command as UTF-16LE base64 under
/// `-EncodedCommand`, so paths with spaces and quotes never face a second round
/// of command-line parsing.
#[cfg(any(windows, test))]
fn is_encoded_wrapper(command: &str) -> bool {
    is_powershell_wrapper(command) || is_service_host_wrapper(command)
}

/// The `-EncodedCommand` payload, decoded. `None` when the flag, the base64 or
/// the UTF-16 is unusable.
#[cfg(any(windows, test))]
fn encoded_payload(args: &str) -> Option<String> {
    let mut words = args.split_whitespace();
    words.find(|w| w.eq_ignore_ascii_case("-EncodedCommand"))?;
    let encoded = words.next()?.trim_matches('"');
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(any(windows, test))]
fn script_has_our_image(script: &str) -> bool {
    let script = script.to_ascii_lowercase();
    [
        "\\walgit.exe",
        "/walgit.exe",
        "\\walgit-server.exe",
        "/walgit-server.exe",
    ]
    .iter()
    .any(|image| script.contains(image))
}

/// Does a `schtasks /FO CSV /NH` listing contain our task? The first field is
/// the task name (`"\walgit","N/A","Ready"`), which — unlike the rest of that
/// output — is not localised.
#[cfg(any(windows, test))]
fn lists_task(list: &str, name: &str) -> bool {
    list.lines().any(|line| {
        line.trim_start_matches('"')
            .split('"')
            .next()
            .is_some_and(|field| field.trim_start_matches('\\').eq_ignore_ascii_case(name))
    })
}

/// First CSV field of a `tasklist /FO CSV /NH` line, unquoted.
#[cfg(any(windows, test))]
#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "fixed-format Windows metadata (netstat/tasklist/command lines): offsets come from find/split on ASCII delimiters, and a shifted format is exactly what a test should fail on"
)]
fn image_from_tasklist(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with('"'))?;
    let rest = line.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// Compiled on every platform (not just Windows) so that name-resolution and
// borrow errors in this module surface in a local `cargo test` instead of
// costing a 40-minute Windows CI round trip. Nothing here runs off-Windows:
// `schtasks` simply does not exist, which the callers treat as "unknown".
#[cfg(any(windows, test))]
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "off-Windows test builds compile this module only to type-check it"
    )
)]
mod task {
    //! `schtasks` wrappers for the one named task that carries the server.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use base64::Engine as _;

    use super::{
        SERVICE_HOST_IMAGE, TASK_NAME, lists_task, task_is_ours,
    };

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
                if s.is_empty() {
                    "absent".to_string()
                } else {
                    s
                }
            }
            // A failed query is *unknown*, not "no task": `status` must not
            // claim the task is gone because `Get-ScheduledTask` hiccupped.
            _ => "unknown".to_string(),
        }
    }

    /// `true` iff the scheduler reports the task as running. `Err` when we cannot
    /// tell — folding that into `false` is how a stuck task goes unnoticed.
    pub fn is_running() -> Result<bool> {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-ScheduledTask -TaskName '{TASK_NAME}' -ErrorAction Stop).State"),
            ])
            .output()
            .context("running Get-ScheduledTask")?;
        if !out.status.success() {
            bail!(
                "Get-ScheduledTask failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .trim()
            .eq_ignore_ascii_case("running"))
    }

    /// What the scheduler holds under our name. The states exist because
    /// conflating them is what let `stop` end a stranger's task and `ensure`
    /// overwrite it.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Task {
        Absent,
        Ours,
        Foreign,
        /// Could not tell (listing or lookup failed) — callers must not act.
        Unknown,
    }

    /// Enumerate first, then inspect. Task *names* are not localised (unlike
    /// `schtasks`' status labels), so a successful listing is authoritative
    /// about existence; anything we cannot answer comes back as `Unknown`.
    pub fn probe() -> Task {
        let Ok(list) = schtasks(&["/Query", "/FO", "CSV", "/NH"]) else {
            return Task::Unknown;
        };
        if !lists_task(&list, TASK_NAME) {
            return Task::Absent;
        }
        match schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]) {
            Ok(xml) if task_is_ours(&xml) => Task::Ours,
            Ok(_) => Task::Foreign,
            Err(_) => Task::Unknown,
        }
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
        // The action is the windowless host, so a task written without it would
        // only fail at logon with the scheduler's own code — say why instead.
        let host = service_host_path(exe);
        if !host.exists() {
            bail!(
                "walgit: {} is missing next to {} — reinstall walgit. The scheduled task \
                 launches that host so the service never gets a console window.",
                host.display(),
                exe.display()
            );
        }
        let existing = probe();
        match &existing {
            Task::Absent | Task::Ours => {}
            Task::Foreign => bail!(
                "walgit: a scheduled task named `{TASK_NAME}` already exists and is not ours — \
                 refusing to replace it. Rename or delete it, then retry."
            ),
            Task::Unknown => bail!(
                "walgit: could not determine whether a task named `{TASK_NAME}` already exists — \
                 refusing to replace it; inspect `schtasks /Query /TN {TASK_NAME} /XML` first"
            ),
        }
        let xml_path = home.join("walgit-task.xml");
        std::fs::write(&xml_path, task_xml(exe, config, log)?)
            .with_context(|| format!("writing the task definition to {}", xml_path.display()))?;
        let path = xml_path.display().to_string();
        // `/F` only when we are refreshing a task we proved is ours: if `walgit`
        // appeared between the probe and here, plain `/Create` fails instead of
        // overwriting a stranger's task.
        let mut args = vec!["/Create", "/TN", TASK_NAME, "/XML", &path];
        if existing == Task::Ours {
            args.push("/F");
        }
        let result = schtasks(&args);
        let _ = std::fs::remove_file(&xml_path);
        result.map(|_| ())
    }

    pub fn run_now() -> Result<()> {
        schtasks(&["/Run", "/TN", TASK_NAME]).map(|_| ())
    }

    pub fn end() -> Result<()> {
        schtasks(&["/End", "/TN", TASK_NAME]).map(|_| ())
    }

    /// The task launches `walgit-service-host.exe` — a **GUI-subsystem** helper
    /// shipped next to this binary — which starts `cmd /d /s /c … >> log 2>&1`
    /// with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`.
    ///
    /// A scheduled `Exec` action always gets a *console*, and with Windows
    /// Terminal as the machine's default terminal that console becomes a window
    /// on screen (or at least a taskbar button) — `-WindowStyle Hidden` only got
    /// it created minimised, so clicking the taskbar entry brought the black box
    /// back. A GUI-subsystem launcher is the only shape in which no console is
    /// ever created; `cmd` still owns the byte-for-byte append redirect.
    ///
    /// Do **not** rely on `/End` to stop the server: measured on the CI runner,
    /// `/End` ended the wrapper while the `walgit.exe` child kept the socket —
    /// so `stop` kills the port's owner first and only then tidies the task.
    /// `…\walgit.exe` → `…\walgit-service-host.exe`, the launcher the task runs.
    /// It is installed next to the server binary, and a debug checkout gets it
    /// from the same `cargo build -p walgit-cli`.
    fn service_host_path(exe: &Path) -> PathBuf {
        exe.parent()
            .unwrap_or(Path::new("."))
            .join(SERVICE_HOST_IMAGE)
    }

    fn task_xml(exe: &Path, config: &Path, log: &Path) -> Result<Vec<u8>> {
        // The scheduler starts the task in `WorkingDirectory`, so a *relative*
        // `--config` (which worked for the caller) would resolve against the
        // install directory instead — silently reading another file or none.
        // Write absolute paths, always.
        fn absolute(p: &Path) -> Result<PathBuf> {
            std::path::absolute(p)
                .with_context(|| format!("resolving `{}` to an absolute path", p.display()))
        }
        let exe = absolute(exe)?;
        let config = absolute(config)?;
        let log = absolute(log)?;
        let encoded = encode_command_line(&service_command_line(&exe, &config, &log));
        let host = service_host_path(&exe);
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
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{host}</Command>
      <Arguments>-EncodedCommand {encoded}</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
            encoded = xml_escape(&encoded),
            host = xml_escape(&host.display().to_string()),
            dir = xml_escape(&exe.parent().unwrap_or(Path::new(".")).display().to_string()),
        );
        // `schtasks /Create /XML` rejects UTF-8 on some builds: emit UTF-16LE
        // with a BOM, which every build accepts.
        let mut out = Vec::with_capacity(body.len() * 2 + 2);
        out.extend_from_slice(&[0xFF, 0xFE]);
        for unit in body.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(out)
    }

    /// `""<exe>" serve --config "<cfg>" >> "<log>" 2>&1"` — the argument the
    /// launcher hands to `cmd /d /s /c`. `/s` strips the outer quotes, the inner
    /// `""…""` keeps a path with spaces intact, and `cmd` opens the log itself:
    /// append, never truncate (the scheduler gives an `Exec` action no stdout).
    fn service_command_line(exe: &Path, config: &Path, log: &Path) -> String {
        format!(
            r#"""{exe}" serve --config "{config}" >> "{log}" 2>&1""#,
            exe = exe.display(),
            config = config.display(),
            log = log.display(),
        )
    }

    /// UTF-16LE/base64: the launcher's transport, so install paths with spaces or
    /// quotes never have to survive a second round of command-line parsing. (`WiX`
    /// and `schtasks /XML` both mangle a nested `""…""` in an attribute.)
    fn encode_command_line(command_line: &str) -> String {
        let mut utf16 = Vec::with_capacity(command_line.len() * 2);
        for unit in command_line.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        base64::engine::general_purpose::STANDARD.encode(utf16)
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    #[cfg(test)]
    mod tests {
        use base64::Engine as _;

        use super::{encode_command_line, service_command_line, task_xml};

        fn xml_text(bytes: &[u8]) -> String {
            assert_eq!(
                &bytes[..2],
                &[0xFF, 0xFE],
                "task XML must carry a UTF-16LE BOM"
            );
            let units: Vec<u16> = bytes
                .get(2..)
                .unwrap_or_default()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            String::from_utf16(&units).expect("task XML is valid UTF-16")
        }

        fn decode(encoded: &str) -> String {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("encoded command is base64");
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            String::from_utf16(&units).expect("encoded command is UTF-16LE")
        }

        /// The action must be the **GUI-subsystem** host, never a console program:
        /// a scheduled `Exec` action always gets a console, and with Windows
        /// Terminal as the default terminal that console is a window on screen.
        #[test]
        fn task_xml_runs_the_windowless_host_with_the_cmd_redirect() {
            let root = tempfile::tempdir().expect("tempdir");
            let dir = root.path().join("wal git & user's");
            std::fs::create_dir_all(&dir).expect("create test dir");
            let exe = dir.join("walgit.exe");
            let cfg = dir.join("walgit.toml");
            let log = dir.join("server.log");
            let host = dir.join("walgit-service-host.exe");

            let xml = xml_text(&task_xml(&exe, &cfg, &log).expect("task XML"));
            assert!(xml.contains("<Hidden>true</Hidden>"));
            // The path is XML-escaped (`&` in the temp dir name), so compare
            // against the escaped form the same way the XML carries it.
            let escaped_host = super::xml_escape(&host.display().to_string());
            assert!(
                xml.contains(&format!("<Command>{escaped_host}</Command>")),
                "the action must be the windowless host: {xml}"
            );
            for console_image in ["<Command>cmd.exe</Command>", "<Command>powershell.exe</Command>"] {
                assert!(
                    !xml.contains(console_image),
                    "a console-subsystem action can still flash a window: {xml}"
                );
            }
            assert!(super::super::task_is_ours(&xml));
            let mixed_case = xml
                .replace("<Command>", "<COMMAND>")
                .replace("</Command>", "</COMMAND>")
                .replace("<Arguments>", "<ARGUMENTS>")
                .replace("</Arguments>", "</ARGUMENTS>");
            assert!(super::super::task_is_ours(&mixed_case));

            // The launcher receives the whole command line, base64(UTF-16LE), so
            // the spaces, the `&` and the apostrophe never face another parser.
            let encoded = xml
                .split("-EncodedCommand ")
                .nth(1)
                .expect("carries an encoded command")
                .split('<')
                .next()
                .expect("value ends at the tag")
                .trim();
            let command_line = decode(encoded);
            assert_eq!(
                command_line,
                service_command_line(&exe, &cfg, &log),
                "the payload is exactly the command the host runs"
            );
            assert_eq!(encode_command_line(&command_line), encoded);
            // `cmd /d /s /c` is the launcher's own doing; the payload is the inner
            // command, and the redirect it carries is what makes the log append.
            for expected in ["serve --config", ">>", "2>&1"] {
                assert!(
                    command_line.contains(expected),
                    "missing {expected:?} in {command_line}"
                );
            }
            for path in [&exe, &cfg, &log] {
                let path = path.display().to_string();
                assert!(command_line.contains(&path), "missing {path:?} in {command_line}");
            }
        }

        /// v0.7.7 shipped a hidden PowerShell wrapper. Machines that installed it
        /// must still be seen as ours, or `ensure` refuses to replace the task and
        /// the upgrade leaves a dead action behind.
        #[test]
        fn the_legacy_powershell_shape_is_still_ours() {
            let script = "# walgit-service-task-v1: cmd /c wrapper with append logging\n\
                          $psi.Arguments = '/d /s /c \"\"C:\\walgit\\walgit.exe\" serve --config \
                          \"C:\\u\\walgit.toml\" >> \"C:\\u\\server.log\" 2>&1\"'";
            let args = format!("-NoLogo -WindowStyle Hidden -EncodedCommand {}", encode_command_line(script));
            assert!(super::super::action_is_ours("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", &args));
        }

        /// Both hidden wrappers stay narrow: a lookalike image (or a payload that
        /// is not our launch command) is somebody else's task, and `ensure`/`end`
        /// must never `/F`-over it.
        #[test]
        fn hidden_wrappers_reject_a_lookalike_image() {
            let script = "# walgit-service-task-v1: cmd /c wrapper\n\
                          & 'C:\\tools\\walgit-backup.exe' serve --config 'x' >> 'log' 2>&1";
            let powershell = format!("-EncodedCommand {}", encode_command_line(script));
            assert!(!super::super::action_is_ours("powershell.exe", &powershell));

            let host_line = r#"""C:\tools\walgit-backup.exe" serve --config "x" >> "y" 2>&1""#;
            let host = format!("-EncodedCommand {}", encode_command_line(host_line));
            assert!(!super::super::action_is_ours(
                r"C:\Program Files\walgit\walgit-service-host.exe",
                &host
            ));
            // …and a host action whose payload never starts the server is out too.
            let not_the_service = format!(
                "-EncodedCommand {}",
                encode_command_line(r#"""C:\walgit\walgit.exe" --version""#)
            );
            assert!(!super::super::action_is_ours(
                "walgit-service-host.exe",
                &not_the_service
            ));
        }
    }
}

/// Parsers that the Windows lifecycle leans on. They are `cfg(any(windows,
/// test))` so the copy that ships on Windows is the copy these tests exercise —
/// the alternative (test a second, unix-only implementation) is how the
/// "wrong pid" class of bug survives a green build.
#[cfg(test)]
mod listener_parse_tests {
    use std::cell::Cell;

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
    fn listeners_on_takes_only_our_port_and_address() {
        // Shape of `netstat -ano -p tcp` on a Chinese Windows: the v4 + v6 rows
        // for the address we bind, a decoy on another port, a non-LISTENING row —
        // and the row that stopped a healthy service for real: another process on
        // the *same port number* but a different local address (FreeSWITCH on the
        // Clash fake-IP 198.18.0.1; thread cc-ai-win-port-owner-scope).
        let netstat = "\
  TCP    127.0.0.1:8081         0.0.0.0:0              LISTENING       4242\r
  TCP    127.0.0.1:9999         0.0.0.0:0              LISTENING       1111\r
  TCP    127.0.0.1:8081         127.0.0.1:50812        ESTABLISHED     9999\r
  TCP    [::1]:8081             [::]:0                 LISTENING       4243\r
  TCP    198.18.0.1:8081        0.0.0.0:0              LISTENING       15772\r
  TCP    [2409:8a1e:7bd4::681d]:8081 [::]:0            LISTENING       15772\r";
        // The `::1` twin is ours (walgit binds both loopbacks); the listeners on
        // the other addresses are somebody else's sockets and are not owners.
        assert_eq!(
            listeners_on(netstat, "127.0.0.1:8081", 8081).unwrap(),
            vec![4242, 4243]
        );
        assert!(
            listeners_on(netstat, "127.0.0.1:1234", 1234)
                .unwrap()
                .is_empty()
        );
        // Read as a `::1`-only configuration, the v4 row stays out too.
        assert_eq!(
            listeners_on(netstat, "[::1]:8081", 8081).unwrap(),
            vec![4242, 4243]
        );
        // A wildcard bind really does own every address of that port.
        assert_eq!(
            listeners_on(
                "  TCP    0.0.0.0:8081    0.0.0.0:0    LISTENING    77\n",
                "127.0.0.1:8081",
                8081
            )
            .unwrap(),
            vec![77]
        );
        assert!(
            listeners_on(
                "  TCP    127.0.0.1:8081    0.0.0.0:0    LISTENING    78\n",
                "0.0.0.0:8081",
                8081
            )
            .unwrap()
            .contains(&78)
        );
        // A *matching* row whose pid we cannot read is an error, never an empty
        // list: "cannot tell who owns it" must not be reported as "port free".
        let malformed = "  TCP    127.0.0.1:8081    0.0.0.0:0    LISTENING    0x10a2\n";
        assert!(listeners_on(malformed, "127.0.0.1:8081", 8081).is_err());
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

    /// A prefix match would kill `walgit-backup.exe` — someone else's process that
    /// merely starts with our name.
    #[test]
    fn only_our_own_images_may_be_killed() {
        use super::is_our_image;
        assert!(is_our_image("walgit.exe"));
        assert!(is_our_image("WALGIT.EXE"));
        assert!(is_our_image("walgit-server.exe"));
        assert!(!is_our_image("walgit-backup.exe"));
        assert!(!is_our_image("walgit-helper.exe"));
        assert!(!is_our_image("notwalgit.exe"));
    }

    #[test]
    fn only_an_action_that_runs_our_binary_is_ours() {
        use super::task_is_ours;
        // Ours: the redirect wrapper, and a direct action.
        assert!(task_is_ours(
            r#"<Actions Context="Author"><Exec><Command>C:\Windows\System32\cmd.exe</Command>
               <Arguments>/c ""C:\Users\x\AppData\Local\Programs\walgit\walgit.exe" serve --config "x" &gt;&gt; "log" 2&gt;&amp;1"</Arguments></Exec></Actions>"#
        ));
        assert!(task_is_ours(
            r"<Actions><Exec><Command>C:\Program Files\walgit\walgit.exe</Command><Arguments>serve</Arguments></Exec></Actions>"
        ));
        // Not ours: the name only appears in the description…
        assert!(!task_is_ours(
            r"<Description>backup walgit.exe data</Description><Actions><Exec><Command>ntbackup.exe</Command></Exec></Actions>"
        ));
        // …or in another program's arguments.
        assert!(!task_is_ours(
            r#"<Actions><Exec><Command>powershell.exe</Command><Arguments>-c "C:\x\walgit.exe"</Arguments></Exec></Actions>"#
        ));
        // …or it is a *different* binary that starts with the same name.
        assert!(!task_is_ours(
            r"<Actions><Exec><Command>C:\tools\walgit-backup.exe</Command></Exec></Actions>"
        ));
        assert!(!task_is_ours("<Task>no actions at all</Task>"));
    }

    #[test]
    fn cmd_ownership_requires_the_binary_in_command_position() {
        use super::task_is_ours;
        assert!(task_is_ours(
            r#"<Actions><Exec><Command>cmd.exe</Command><Arguments>/c ""C:\Program Files\walgit\walgit.exe" serve --config "x"</Arguments></Exec></Actions>"#
        ));
        assert!(task_is_ours(
            r#"<Actions><Exec><Command>cmd.exe</Command><Arguments>/d /s /c "C:\walgit\walgit.exe" serve</Arguments></Exec></Actions>"#
        ));
        // A path merely printed or passed as an argument is not the command.
        assert!(!task_is_ours(
            r"<Actions><Exec><Command>cmd.exe</Command><Arguments>/c echo C:\walgit\walgit.exe</Arguments></Exec></Actions>"
        ));
        assert!(!task_is_ours(
            r#"<Actions><Exec><Command>cmd.exe</Command><Arguments>/c "echo C:\walgit\walgit.exe"</Arguments></Exec></Actions>"#
        ));
        assert!(!task_is_ours(
            r"<Actions><Exec><Command>cmd.exe</Command><Arguments>/c helper.exe C:\walgit\walgit.exe</Arguments></Exec></Actions>"
        ));
    }

    #[test]
    fn stop_retries_unknown_task_probes_then_fails_closed() {
        use super::{TASK_PROBE_ATTEMPTS, end_task_if_ours_with, task::Task};

        let probes = Cell::new(0);
        let ended = Cell::new(false);
        let result = end_task_if_ours_with(
            || {
                probes.set(probes.get() + 1);
                Task::Unknown
            },
            || -> anyhow::Result<()> {
                ended.set(true);
                Ok(())
            },
            || -> anyhow::Result<bool> { panic!("is_running called for an unknown task") },
        );

        assert!(result.is_err());
        assert_eq!(probes.get(), TASK_PROBE_ATTEMPTS);
        assert!(!ended.get());
    }

    #[test]
    fn task_probe_stops_retrying_once_ownership_is_known() {
        use super::{end_task_if_ours_with, task::Task};

        let mut probes = [Task::Unknown, Task::Unknown, Task::Absent].into_iter();
        let calls = Cell::new(0);
        let result = end_task_if_ours_with(
            || {
                calls.set(calls.get() + 1);
                probes.next().unwrap()
            },
            || -> anyhow::Result<()> { panic!("end called for an absent task") },
            || -> anyhow::Result<bool> { panic!("is_running called for an absent task") },
        );

        assert!(result.is_ok());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn foreign_task_is_left_alone_without_failing_stop() {
        use super::{end_task_if_ours_with, task::Task};

        let result = end_task_if_ours_with(
            || Task::Foreign,
            || -> anyhow::Result<()> { panic!("end called for a foreign task") },
            || -> anyhow::Result<bool> { panic!("is_running called for a foreign task") },
        );

        assert!(result.is_ok());
    }

    #[test]
    fn existence_comes_from_the_task_name_column() {
        use super::lists_task;
        // The first field of `schtasks /FO CSV /NH` is the task name — the one
        // column that is *not* localised.
        let list = concat!(
            r"\Microsoft\Windows\Defrag\ScheduledDefrag,N/A,Ready",
            "\n",
            r#""\walgit",N/A,Running"#,
            "\n",
        );
        assert!(lists_task(list, "walgit"));
        assert!(!lists_task(list, "walgit2"));
        assert!(!lists_task(
            r"\Microsoft\Windows\Defrag\ScheduledDefrag,N/A,Ready",
            "walgit"
        ));
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
    #[allow(unsafe_code)] // getsid on the child we just spawned, checked to fit pid_t below.
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
