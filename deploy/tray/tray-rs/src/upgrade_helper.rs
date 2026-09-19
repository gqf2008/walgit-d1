//! Detached Windows tray-update helper.
//!
//! The helper is deliberately a separate executable: Inno Setup must replace
//! `walgit-tray.exe` and `walgit.exe`, and Windows will not let the installer
//! replace a running image. The tray copies this helper into its writable state
//! directory, launches the copy, then exits. The copy waits for that tray PID,
//! stops the scheduled service, runs the verified installer, starts and
//! health-checks the new service, and rolls back with the old installer on any
//! failure.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const DEFAULT_TRAY_WAIT: Duration = Duration::from_secs(30);
const DEFAULT_HEALTH_WAIT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
struct Args {
    new_installer: PathBuf,
    rollback_installer: PathBuf,
    new_sha256: String,
    rollback_sha256: String,
    target_version: String,
    rollback_version: String,
    install_dir: PathBuf,
    state_dir: PathBuf,
    log: PathBuf,
    tray_pid: u32,
    tray_wait: Duration,
    health_wait: Duration,
    #[cfg(debug_assertions)]
    service_bin: Option<PathBuf>,
    #[cfg(debug_assertions)]
    tray_bin: Option<PathBuf>,
    #[cfg(debug_assertions)]
    health_command: Option<PathBuf>,
}

impl Args {
    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut new_installer = None;
        let mut rollback_installer = None;
        let mut new_sha256 = None;
        let mut rollback_sha256 = None;
        let mut target_version = None;
        let mut rollback_version = None;
        let mut install_dir = None;
        let mut state_dir = None;
        let mut log = None;
        let mut tray_pid = None;
        let mut tray_wait = DEFAULT_TRAY_WAIT;
        let mut health_wait = DEFAULT_HEALTH_WAIT;
        #[cfg(debug_assertions)]
        let mut service_bin = None;
        #[cfg(debug_assertions)]
        let mut tray_bin = None;
        #[cfg(debug_assertions)]
        let mut health_command = None;

        let mut iter = args.into_iter();
        while let Some(raw) = iter.next() {
            let key = raw.to_string_lossy();
            let mut value = || {
                iter.next()
                    .ok_or_else(|| format!("missing value after {key}"))
            };
            match key.as_ref() {
                "--new-installer" => new_installer = Some(PathBuf::from(value()?)),
                "--rollback-installer" => {
                    rollback_installer = Some(PathBuf::from(value()?));
                }
                "--new-sha256" => new_sha256 = Some(value()?.to_string_lossy().into_owned()),
                "--rollback-sha256" => {
                    rollback_sha256 = Some(value()?.to_string_lossy().into_owned());
                }
                "--target-version" => {
                    target_version = Some(value()?.to_string_lossy().into_owned());
                }
                "--rollback-version" => {
                    rollback_version = Some(value()?.to_string_lossy().into_owned());
                }
                "--install-dir" => install_dir = Some(PathBuf::from(value()?)),
                "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
                "--log" => log = Some(PathBuf::from(value()?)),
                "--tray-pid" => {
                    let text = value()?.to_string_lossy().into_owned();
                    tray_pid = Some(
                        text.parse::<u32>()
                            .map_err(|_| format!("invalid --tray-pid: {text}"))?,
                    );
                }
                "--tray-wait-ms" if cfg!(debug_assertions) => {
                    let text = value()?.to_string_lossy().into_owned();
                    let millis = text
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --tray-wait-ms: {text}"))?;
                    tray_wait = Duration::from_millis(millis);
                }
                "--health-wait-ms" if cfg!(debug_assertions) => {
                    let text = value()?.to_string_lossy().into_owned();
                    let millis = text
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --health-wait-ms: {text}"))?;
                    health_wait = Duration::from_millis(millis);
                }
                #[cfg(debug_assertions)]
                "--service-bin" => service_bin = Some(PathBuf::from(value()?)),
                #[cfg(debug_assertions)]
                "--tray-bin" => tray_bin = Some(PathBuf::from(value()?)),
                #[cfg(debug_assertions)]
                "--health-command" => health_command = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        let missing = |name: &str| format!("missing --{name}");
        let state_dir: PathBuf = state_dir.ok_or_else(|| missing("state-dir"))?;
        let log = log.unwrap_or_else(|| state_dir.join("tray.log"));
        Ok(Self {
            new_installer: new_installer.ok_or_else(|| missing("new-installer"))?,
            rollback_installer: rollback_installer.ok_or_else(|| missing("rollback-installer"))?,
            new_sha256: new_sha256.ok_or_else(|| missing("new-sha256"))?,
            rollback_sha256: rollback_sha256.ok_or_else(|| missing("rollback-sha256"))?,
            target_version: target_version.ok_or_else(|| missing("target-version"))?,
            rollback_version: rollback_version.ok_or_else(|| missing("rollback-version"))?,
            install_dir: install_dir.ok_or_else(|| missing("install-dir"))?,
            state_dir,
            log,
            tray_pid: tray_pid.ok_or_else(|| missing("tray-pid"))?,
            tray_wait,
            health_wait,
            #[cfg(debug_assertions)]
            service_bin,
            #[cfg(debug_assertions)]
            tray_bin,
            #[cfg(debug_assertions)]
            health_command,
        })
    }
}

pub fn main_entry() -> i32 {
    let args = match Args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("walgit-upgrade-helper: {error}");
            return 1;
        }
    };
    match run(&args) {
        Ok(()) => 0,
        Err(error) => {
            log(&args, &format!("FAIL: {error}"));
            1
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    log(
        args,
        &format!(
            "helper start: {} -> {}",
            args.rollback_version, args.target_version
        ),
    );
    log(args, "verify new installer hash");
    let new_hash = verify_sha256_file(&args.new_installer, &args.new_sha256);
    log(args, "verify rollback installer hash");
    let rollback_hash = verify_sha256_file(&args.rollback_installer, &args.rollback_sha256);
    if let Err(error) = new_hash.and(rollback_hash) {
        // Nothing was replaced yet: restore the user's entry point and fail
        // without invoking either installer.
        let _ = launch_tray(args);
        return Err(error);
    }

    let result = (|| -> Result<(), String> {
        log(args, &format!("wait for tray pid {}", args.tray_pid));
        wait_for_pid(args.tray_pid, args.tray_wait)?;
        log(args, "stop service");
        stop_service(args)?;
        log(args, "run new installer");
        run_installer(&args.new_installer, "new", args)?;
        log(args, "verify installed version");
        verify_install_version(args, &args.target_version)?;
        log(args, "start service");
        start_service(args)?;
        log(args, "wait for health");
        wait_for_health(args, &args.target_version)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            launch_tray(args)?;
            log(
                args,
                &format!("SUCCESS: v{}", normalized(&args.target_version)),
            );
            Ok(())
        }
        Err(why) => {
            log(args, &format!("rollback: {why}"));
            rollback(args, &why)?;
            Err(why)
        }
    }
}

fn normalized(version: &str) -> String {
    crate::release::strip_version_prefix(version)
}

fn log(args: &Args, message: &str) {
    if let Some(parent) = args.log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "[{stamp}] update v{}: {message}\n",
        normalized(&args.target_version)
    );
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log)
        .and_then(|mut file| file.write_all(line.as_bytes()));
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    // Heap-backed: Windows' default 1 MiB main-thread stack cannot hold a
    // 1 MiB array once debug frames are counted (observed as 0xC00000FD).
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn verify_sha256_file(path: &Path, expected: &str) -> Result<(), String> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid expected SHA-256 for {}: {expected}",
            path.display()
        ));
    }
    let got = sha256_file(path)?;
    if got != expected {
        return Err(format!(
            "SHA-256 mismatch for {}: expected {expected}, got {got}",
            path.display()
        ));
    }
    Ok(())
}

fn service_program(args: &Args) -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = &args.service_bin {
        return path.clone();
    }
    if cfg!(target_os = "windows") {
        args.install_dir.join("walgit.exe")
    } else {
        args.install_dir.join("walgit")
    }
}

fn tray_program(args: &Args) -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = &args.tray_bin {
        return path.clone();
    }
    if cfg!(target_os = "windows") {
        args.install_dir.join("walgit-tray.exe")
    } else {
        args.install_dir.join("walgit-tray")
    }
}

fn command_for(program: &Path, args: &[OsString], detached: bool) -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let flags = CREATE_NO_WINDOW
            | if detached {
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
            } else {
                0
            };
        let is_cmd = program
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
        if is_cmd {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(program).args(args);
            command.creation_flags(flags);
            return command;
        }
        let mut command = Command::new(program);
        command.args(args);
        command.creation_flags(flags);
        command
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = detached;
        let mut command = Command::new(program);
        command.args(args);
        command
    }
}

fn run_capture(program: &Path, args: &[OsString]) -> Result<String, String> {
    let output = command_for(program, args, false)
        .output()
        .map_err(|e| format!("run {}: {e}", program.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "{} exited {}: {}",
            program.display(),
            output.status.code().unwrap_or(-1),
            tail(&text, 240)
        ))
    }
}

fn tail(text: &str, max: usize) -> String {
    let text = text.trim();
    let skip = text.chars().count().saturating_sub(max);
    text.chars().skip(skip).collect()
}

fn run_service(args: &Args, verb: &str) -> Result<(), String> {
    let config = args.state_dir.join("walgit.toml");
    let output = run_capture(
        &service_program(args),
        &[
            OsString::from("service"),
            OsString::from(verb),
            OsString::from("--config"),
            config.into_os_string(),
        ],
    )?;
    log(args, &format!("service {verb}: {}", tail(&output, 160)));
    Ok(())
}

fn stop_service(args: &Args) -> Result<(), String> {
    run_service(args, "stop")
}

fn start_service(args: &Args) -> Result<(), String> {
    run_service(args, "start")
}

fn run_installer(installer: &Path, label: &str, args: &Args) -> Result<(), String> {
    let output = run_capture(
        installer,
        &[
            OsString::from("/VERYSILENT"),
            OsString::from("/SUPPRESSMSGBOXES"),
            OsString::from("/NORESTART"),
        ],
    )?;
    log(
        args,
        &format!("{label} installer ok: {}", tail(&output, 160)),
    );
    Ok(())
}

fn verify_install_version(args: &Args, expected: &str) -> Result<(), String> {
    let output = run_capture(&service_program(args), &[OsString::from("--version")])?;
    let got = crate::release::parse_tool_version(&output)
        .ok_or_else(|| format!("cannot parse installed version from {output:?}"))?;
    let expected = normalized(expected);
    if got != expected {
        return Err(format!(
            "installed version mismatch: expected {expected}, got {got}"
        ));
    }
    Ok(())
}

fn health_url(args: &Args) -> String {
    let config = args.state_dir.join("walgit.toml");
    let mut listen = String::new();
    if let Ok(text) = std::fs::read_to_string(config) {
        for line in text.lines() {
            let flat = line.trim().split('#').next().unwrap_or("").trim();
            if let Some(value) = flat
                .strip_prefix("listen = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                listen = value.to_string();
                break;
            }
        }
    }
    if listen.is_empty() {
        listen = "127.0.0.1:8081".into();
    }
    format!("http://{listen}/healthz")
}

fn http_health_version(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported health URL: {url}"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "healthz"));
    let addr: SocketAddr = authority
        .parse()
        .map_err(|e| format!("invalid health address {authority}: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("connect {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("health timeout: {e}"))?;
    let host = authority
        .trim_start_matches('[')
        .split([']', ':'])
        .next()
        .unwrap_or("localhost");
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("health request: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("health response: {e}"))?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    version_from_body(body).ok_or_else(|| format!("health response has no version: {body:?}"))
}

fn version_from_body(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

fn wait_for_health(args: &Args, expected: &str) -> Result<(), String> {
    let expected = normalized(expected);
    let deadline = Instant::now() + args.health_wait;
    let url = health_url(args);
    loop {
        let result = {
            #[cfg(debug_assertions)]
            if let Some(command) = &args.health_command {
                run_capture(command, &[]).and_then(|out| {
                    crate::release::parse_tool_version(&out)
                        .ok_or_else(|| format!("health command output has no version: {out:?}"))
                })
            } else {
                http_health_version(&url)
            }
            #[cfg(not(debug_assertions))]
            {
                http_health_version(&url)
            }
        };
        let detail = match result {
            Ok(version) if normalized(&version) == expected => {
                log(args, &format!("health ok: v{expected}"));
                return Ok(());
            }
            Ok(version) => format!("got v{}", normalized(&version)),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(format!("health check did not reach v{expected}: {detail}"));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn launch_tray(args: &Args) -> Result<(), String> {
    let tray = tray_program(args);
    if !tray.is_file() {
        return Err(format!("missing tray binary: {}", tray.display()));
    }
    let mut command = command_for(&tray, &[], true);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|e| format!("launch {}: {e}", tray.display()))?;
    log(args, &format!("launched tray {}", tray.display()));
    Ok(())
}

fn rollback(args: &Args, why: &str) -> Result<(), String> {
    let _ = stop_service(args);
    run_installer(&args.rollback_installer, "rollback", args)
        .map_err(|e| format!("rollback installer failed: {e}"))?;
    verify_install_version(args, &args.rollback_version)
        .map_err(|e| format!("rollback version check failed: {e}"))?;
    start_service(args).map_err(|e| format!("rollback service start failed: {e}"))?;
    wait_for_health(args, &args.rollback_version)
        .map_err(|e| format!("rollback health check failed: {e}"))?;
    launch_tray(args).map_err(|e| format!("rollback tray launch failed: {e}"))?;
    log(args, &format!("rollback complete after: {why}"));
    Ok(())
}

fn wait_for_pid(pid: u32, timeout: Duration) -> Result<(), String> {
    if pid == 0 {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return Err(format!("old tray pid {pid} did not exit"));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return false;
    }
    let status = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    status == WAIT_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_args() {
        let args = Args::parse(
            [
                "--new-installer",
                "new.exe",
                "--rollback-installer",
                "old.exe",
                "--new-sha256",
                "aa",
                "--rollback-sha256",
                "bb",
                "--target-version",
                "v0.2.0",
                "--rollback-version",
                "0.1.0",
                "--install-dir",
                "C:\\walgit",
                "--state-dir",
                "C:\\state",
                "--tray-pid",
                "42",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .expect("parse");
        assert_eq!(args.tray_pid, 42);
        assert_eq!(args.target_version, "v0.2.0");
        assert_eq!(args.log, PathBuf::from("C:\\state").join("tray.log"));
    }

    #[test]
    fn hashes_match_known_vector() {
        let path = std::env::temp_dir().join(format!(
            "walgit-helper-sha-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"abc").expect("write");
        let got = sha256_file(&path).expect("hash");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
