//! walgit-tray — walgit 服务的系统托盘(macOS / Windows / Linux 一套代码)。
//!
//! 菜单:状态行 · 启动/停止 · 版本升级状态行(abb 同款状态机) ·
//!      打开 Web UI · 退出托盘(服务保持运行)。
//! 升级语义:自动的只有「检测」(每 30 分钟 fetch 比对,启动 30 秒先查一次);
//!      发现新版本只把菜单行变成「⬆️ 升级到新版本」,**由用户点击才升级**:
//!      ff-merge main → cargo 构建 → 备份 → 停 → 热换 → 健康验证,失败回滚。
//!
//! 服务控制:shell 到 `walgit-ensure`(转发壳)→ `walgit service`(存活/起停在二进制里);Windows/Linux
//!          部署目录下的 walgit(.exe) 分离进程 + pidfile。
//! 健康检查:内置裸 HTTP(loopback),零额外依赖。
//! 打开 Web UI:直接开新页面(三平台一致)。

// Windows release 不带控制台:双击静默驻留托盘(debug 构建保留控制台便于排查)。
// 注意 start 引号:cmd 只认双引号,`start "" "url"` 的 "" 是占位标题。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};

const DEFAULT_LISTEN: &str = "127.0.0.1:8081";

// 升级状态机(abb app.slint 同款)
const ST_IDLE: u8 = 0; // 未查:检查更新…
const ST_CHECKING: u8 = 1; // 正在检查更新…
const ST_LATEST: u8 = 2; // 已是最新 ✓(点击重查)
const ST_AVAILABLE: u8 = 3; // ⬆️ 升级到新版本
const ST_INSTALLING: u8 = 4; // 安装中…
const ST_FAILED: u8 = 5; // 失败(点击重查)

fn home() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default())
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    }
}

fn deploy_dir() -> PathBuf {
    home().join("walgit")
}

fn repo_dir() -> PathBuf {
    std::env::var("WALGIT_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            #[cfg(target_os = "macos")]
            {
                PathBuf::from("/Volumes/Workspace/GitHub/walgit")
            }
            #[cfg(not(target_os = "macos"))]
            {
                home().join("walgit-repo")
            }
        })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn pid_file() -> PathBuf {
    deploy_dir().join("walgit.pid")
}

fn exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "walgit.exe"
    } else {
        "walgit"
    }
}

fn log_line(s: &str) {
    let dir = deploy_dir();
    // home 缺失时退化为相对路径——宁可丢日志也不在 CWD/System32 下建杂散目录
    if dir.as_os_str().is_empty() {
        return;
    }
    // 部署目录首次运行可能不存在:建出来,否则日志被 OpenOptions 静默丢弃。
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tray.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{s}]");
    }
}

// ---------- 裸 HTTP ----------

/// 部署目录 walgit.toml 的行扫描解析 → (listen, backend, memory_intentional)。注释行跳过;
/// 找不到/解析失败回退默认。简单位扫描即可——`listen` 只在 [server] 节、
/// `backend` 只在 [store] 节出现(#73:托盘探活与配置同源,用户改 listen
/// 不再使状态行恒「已停止」、升级健康验证恒失败)。
fn deploy_config() -> (String, String, bool) {
    let path = deploy_dir().join("walgit.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (DEFAULT_LISTEN.to_string(), String::new(), false);
    };
    let mut listen = String::new();
    let mut backend = String::new();
    let mut intentional = false;
    for line in text.lines() {
        // TOML 行尾注释(#115 审查修正):仓库模板全是
        // `listen = "127.0.0.1:8081"  # 注释` 风格,不剥则解析恒落空。
        // listen/backend 的值不可能含 #,split 安全。
        let t = line.trim().split('#').next().unwrap_or("").trim();
        if t.is_empty() {
            continue;
        }
        if let Some(v) = t
            .strip_prefix("listen = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            listen = v.to_string();
        } else if let Some(v) = t
            .strip_prefix("backend = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            backend = v.to_string();
        } else if t.starts_with("memory_backend_intentional = ") {
            intentional = t.ends_with("true");
        }
    }
    if listen.is_empty() {
        listen = DEFAULT_LISTEN.to_string();
    }
    (listen, backend, intentional)
}

/// 返回 healthz 响应体(含 version 字段);服务不在时 None。
fn healthz() -> Option<String> {
    let (host, _, _) = deploy_config();
    let mut stream = TcpStream::connect(&host).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    // [::1]:8081 的 IPv6 括号形式:取 ] 前的部分当 Host。
    let hostname = host
        .trim_start_matches('[')
        .split([']', ':'])
        .next()
        .unwrap_or("localhost");
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {hostname}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    let body = buf.split_once("\r\n\r\n")?.1;
    body.contains("ok").then(|| body.trim().to_string())
}

/// 从 healthz 响应体提取版本串(short sha)。
#[allow(dead_code)] // 供状态行显示;macOS 上由 Swift 版承担
fn version_of(body: &str) -> String {
    body.split("\"version\":\"")
        .nth(1)
        .map(|rest| rest.split('"').next().unwrap_or("").to_string())
        .unwrap_or_default()
}

// ---------- shell ----------

// POSIX sh:macOS 的 walgit-ensure / open / xdg-open / kill 仍走字符串(真 sh
// 认单引号);Windows 一律走 run() 的 argv 直传,不过 shell。
#[cfg(not(target_os = "windows"))]
fn sh(cmd: &str) -> (i32, String) {
    match std::process::Command::new("sh")
        .arg("-lc")
        .arg(cmd)
        .output()
    {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => (-1, format!("{e}")),
    }
}

/// 起子进程的统一入口:argv 直传,**不拼 shell 字符串**——Rust std 会给含
/// 空格/引号的参数做 MSVCRT 式转义,而 cmd 的 /C 只剥最外层一对引号,两者
/// 规则互不兼容:嵌套引号全灭(`start "" "x"` 挂起、`set "VAR=v"&&` 变垃圾
/// 变量),cmd 又从不认单引号(`git -C 'x'` fatal)。Windows 侧加
/// CREATE_NO_WINDOW:GUI 进程每 spawn 一个控制台程序(cmd/git/taskkill)
/// 不带它就闪一次黑窗。环境变量经 .env() 传,不经 `set`。
fn run(
    dir: Option<&std::path::Path>,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (i32, String) {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => (-1, format!("{e}")),
    }
}

// ---------- 服务控制 ----------

#[cfg(target_os = "macos")]
fn ensure_path() -> PathBuf {
    let candidates = [
        home().join(".claude/skills/walgit/scripts/walgit-ensure"),
        deploy_dir().join("walgit-ensure"),
    ];
    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

fn service_start() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (code, out) = sh(&format!("'{}' 2>&1", ensure_path().display()));
        if code == 0 {
            Ok(())
        } else {
            Err(out)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let child = spawn_service()?;
        std::fs::write(pid_file(), child.id().to_string()).map_err(|e| format!("pidfile: {e}"))?;
        // D43: the setup wizard's save exits 75 ("restart me"). Supervise the
        // child so 保存并重启 is one click — respawn on 75, bounded (a real
        // restart loop means the written config does not hold).
        std::thread::spawn(move || supervise_service(child));
        Ok(())
    }
}

/// Spawn `walgit serve --config walgit.toml` detached (windows: no console
/// window, new group; unix: own session).
#[cfg(not(target_os = "macos"))]
fn spawn_service() -> Result<std::process::Child, String> {
    let exe = deploy_dir().join(exe_name());
    let cfg = deploy_dir().join("walgit.toml");
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve").arg("--config").arg(&cfg);
    // D43: the setup save exits 75 only when the server knows a supervisor
    // will respawn it — this marker arms the exit.
    cmd.env("WALGIT_SUPERVISED", "1");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED | NEW_GROUP
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()
        .map_err(|e| format!("spawn {}: {e}", exe.display()))
}

/// Watch the service process: exit code 75 = the setup wizard saved and asks
/// for a restart (D43) — respawn, at most five times in a row (a loop means
/// the written config does not hold; the user reads the log). Any other exit
/// is final.
#[cfg(not(target_os = "macos"))]
fn supervise_service(mut child: std::process::Child) {
    let mut restarts: u32 = 0;
    loop {
        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("walgit service watcher: {e}");
                return;
            }
        };
        if status.code() != Some(75) {
            eprintln!(
                "walgit service exited ({})",
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string())
            );
            return;
        }
        restarts += 1;
        if restarts > 5 {
            eprintln!("walgit service restarted 5 times in a row — giving up (check walgit.toml)");
            return;
        }
        eprintln!("walgit service: restart after setup save ({restarts})");
        match spawn_service() {
            Ok(c) => {
                let _ = std::fs::write(pid_file(), c.id().to_string());
                child = c;
            }
            Err(e) => {
                eprintln!("walgit service restart failed: {e}");
                return;
            }
        }
    }
}

fn service_stop() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (code, out) = sh(&format!("'{}' stop 2>&1", ensure_path().display()));
        if code == 0 {
            Ok(())
        } else {
            Err(out)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let pid: u32 = std::fs::read_to_string(pid_file())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| "no pidfile".to_string())?;
        // 双过滤:PID 与映像名同时匹配才杀——裸 /PID 会撞上 pid 复用误杀
        // 无关进程树(pidfile 在服务崩溃后就是陈旧的)。
        #[cfg(target_os = "windows")]
        let (code, out) = {
            let pid_filter = format!("PID eq {pid}");
            let name_filter = format!("IMAGENAME eq {}", exe_name());
            run(
                None,
                "taskkill",
                &["/F", "/T", "/FI", &pid_filter, "/FI", &name_filter],
                &[],
            )
        };
        #[cfg(not(target_os = "windows"))]
        let (code, out) = sh(&format!("kill {pid}"));
        if code == 0 {
            Ok(())
        } else {
            Err(out)
        }
    }
}

/// 升级管线(仅由用户点击触发):fetch → ff-merge → 构建 → 备份 → 停 → 换 → 起 → 验证。
/// 换装阶段任何一步失败都走同一条回滚路径(还原备份 → 重启服务),并把回滚
/// 本身的结果如实写进错误串——不谎报「已回滚」。
fn upgrade_pipeline(report: &dyn Fn(String)) -> Result<String, String> {
    let repo = repo_dir();
    let bin = deploy_dir().join(exe_name());

    report("对齐 main…".into());
    let _ = run(Some(&repo), "git", &["fetch", "origin", "main"], &[]);
    let (mc, mout) = run(
        Some(&repo),
        "git",
        &["merge", "--ff-only", "origin/main"],
        &[],
    );
    if mc != 0 {
        log_line(&format!(
            "upgrade: ff-merge FAILED {}",
            &mout[..mout.len().min(200)]
        ));
        return Err("本地 main 无法快进到 origin/main".into());
    }

    report("构建中…".into());
    let (bc, bout) = run(
        Some(&repo),
        "cargo",
        &["build", "--release", "-p", "walgit-cli"],
        &[("RUSTUP_TOOLCHAIN", "1.98.0")],
    );
    if bc != 0 {
        log_line(&format!(
            "upgrade: build FAILED {}",
            &bout[..bout.len().min(300)]
        ));
        return Err("cargo 构建失败,旧版本继续运行".into());
    }
    let (_, sha_out) = run(Some(&repo), "git", &["rev-parse", "--short=7", "HEAD"], &[]);
    let sha = sha_out.trim().to_string();

    report("换装中…".into());
    let bak = deploy_dir().join("walgit.bak-tray");
    let _ = std::fs::copy(&bin, &bak);
    let _ = service_stop();
    let swap = std::fs::copy(repo.join("target/release").join(exe_name()), &bin)
        .map_err(|e| format!("copy binary: {e}"))
        .and_then(|_| service_start());

    // 健康验证:换装成功才等;15 秒内 /healthz 报出本次 sha 即成功。
    let healthy = swap.is_ok()
        && (0..15).any(|_| {
            let ok = healthz().is_some_and(|h| h.contains(&sha));
            if !ok {
                std::thread::sleep(Duration::from_secs(1));
            }
            ok
        });
    if healthy {
        log_line(&format!("upgrade: success {sha}"));
        return Ok(sha);
    }

    // 回滚:无论换装死在哪一步,还原备份并尽力重启服务,结果如实报告。
    let restore = std::fs::copy(&bak, &bin).map_err(|e| format!("restore backup: {e}"));
    let _ = service_stop();
    let restart = service_start();
    let why = swap.err().unwrap_or_else(|| "健康检查未过".into());
    log_line(&format!(
        "upgrade: {why} — rollback restore={restore:?} restart={restart:?}"
    ));
    Err(match (restore.is_ok(), restart.is_ok()) {
        (true, true) => format!("{why},已回滚旧版本并重启"),
        (true, false) => format!("{why},备份已还原但服务重启失败(托盘菜单「启动服务」重试)"),
        _ => format!(
            "{why},回滚失败——备份损坏,请重装或手动处理 {}",
            bak.display()
        ),
    })
}

// ---------- 打开 Web UI(不重复开页) ----------

/// 打开 Web UI:直接开新页面(三平台一致)。Windows 用 explorer(GUI 进程,
/// spawn 立即返回、无控制台、不经 shell——cmd 的 start 经 Rust 参数转义后
/// 引号全灭,实测会挂起事件循环线程 2 分钟以上)。
fn open_web() {
    log_line("web: opening page");
    // 与探活同源的地址(#73):用户改 walgit.toml 的 listen 后,打开的页面
    // 仍是同一个服务。
    let (host, _, _) = deploy_config();
    // 通配监听(0.0.0.0/::)浏览器开不出地址——回环替换,只取端口(#115 审查)。
    let (hostname, port) = host
        .rsplit_once(':')
        .map_or((host.as_str(), ""), |(h, p)| (h, p));
    let hostname = match hostname {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    let url = if port.is_empty() {
        format!("http://{hostname}/")
    } else {
        format!("http://{hostname}:{port}/")
    };
    #[cfg(target_os = "macos")]
    sh(&format!("open '{url}'"));
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    sh(&format!("xdg-open '{url}'"));
}

// ---------- 图标:分支汇入桶(手工光栅化,分状态着色) ----------

fn seg_dist(px: f32, py: f32, p0: [f32; 2], p1: [f32; 2]) -> f32 {
    let (vx, vy) = (p1[0] - p0[0], p1[1] - p0[1]);
    let (wx, wy) = (px - p0[0], py - p0[1]);
    let len2 = vx * vx + vy * vy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    };
    let (dx, dy) = (px - (p0[0] + t * vx), py - (p0[1] + t * vy));
    (dx * dx + dy * dy).sqrt()
}

/// 状态色:运行绿 / 升级橙 / 检查与切换黄 / 停止灰。
fn state_color(running: bool, busy: u8) -> [u8; 3] {
    if busy == 2 {
        [232, 160, 32] // 橙:升级中
    } else if busy == 1 {
        [255, 204, 0] // 黄:切换中
    } else if running {
        [46, 160, 67] // 绿:运行中
    } else {
        [138, 138, 138] // 灰:已停止
    }
}

fn icon_rgba(size: usize, color: [u8; 3]) -> Vec<u8> {
    let n = size;
    let s = 256.0 / n as f32;
    let mut rgba = vec![0u8; n * n * 4];
    let mut segments: Vec<([f32; 2], [f32; 2])> = Vec::new();
    segments.push(([78.0, 180.0], [78.0, 88.0]));
    let mut prev = [186.0_f32, 180.0];
    for i in 1..=24 {
        let t = i as f32 / 24.0;
        let mt = 1.0 - t;
        let x = mt * mt * mt * 186.0
            + 3.0 * mt * mt * t * 186.0
            + 3.0 * mt * t * t * 78.0
            + t * t * t * 78.0;
        let y = mt * mt * mt * 180.0
            + 3.0 * mt * mt * t * 152.0
            + 3.0 * mt * t * t * 152.0
            + t * t * t * 138.0;
        segments.push((prev, [x, y]));
        prev = [x, y];
    }
    let circles: [(f32, f32, f32); 2] = [(78.0, 200.0, 17.0), (186.0, 200.0, 17.0)];
    let slabs: [(f32, f32, f32, f32); 2] = [(52.0, 22.0, 152.0, 24.0), (52.0, 50.0, 152.0, 24.0)];
    let half = 7.5;

    for py in 0..n {
        for px in 0..n {
            let cx = (px as f32 + 0.5) * s;
            let cy = 256.0 - (py as f32 + 0.5) * s;
            let mut cov = 0.0f32;
            for sy in 0..4u8 {
                for sx in 0..4u8 {
                    let x = cx + (f32::from(sx) - 1.5) / 4.0 * s;
                    let y = cy + (f32::from(sy) - 1.5) / 4.0 * s;
                    let inside = segments
                        .iter()
                        .any(|(p0, p1)| seg_dist(x, y, *p0, *p1) <= half)
                        || circles.iter().any(|(ccx, ccy, r)| {
                            (x - ccx) * (x - ccx) + (y - ccy) * (y - ccy) <= r * r
                        })
                        || slabs.iter().any(|(rx, ry, rw, rh)| {
                            x >= *rx && x <= rx + rw && y >= *ry && y <= ry + rh
                        });
                    if inside {
                        cov += 1.0 / 16.0;
                    }
                }
            }
            let i = (py * n + px) * 4;
            rgba[i] = color[0];
            rgba[i + 1] = color[1];
            rgba[i + 2] = color[2];
            rgba[i + 3] = (cov * 255.0) as u8;
        }
    }
    rgba
}

// ---------- 事件与状态 ----------

#[derive(Debug, Clone)]
enum Msg {
    Status(bool),
    UpdateState(u8),
    Available(String),
    Note(String),
    Busy(u8),
}

struct MenuHandles {
    status: MenuItem,
    toggle: MenuItem,
    upgrade: MenuItem,
    quit: MenuItem,
}

struct App {
    tray: Option<TrayIcon>,
    items: Option<MenuHandles>,
    proxy: Option<Arc<EventLoopProxy<Msg>>>,
    running: bool,
    busy: u8,  // 0 idle, 1 switching, 2 upgrading
    state: u8, // 升级状态机(ST_*)
    version: String,
    available_sha: String,
    note: String,
}

impl App {
    fn current_version(&self) -> String {
        let v = self.version.clone();
        if v.is_empty() {
            "…".to_string()
        } else {
            v[..7.min(v.len())].to_string()
        }
    }

    fn update_icon(&mut self) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let color = state_color(self.running, self.busy);
        if let Ok(icon) = tray_icon::Icon::from_rgba(icon_rgba(32, color), 32, 32) {
            let _ = tray.set_icon(Some(icon));
        }
    }

    fn rebuild_menu(&mut self) {
        let Some(h) = &self.items else { return };
        let running = self.running;
        // 运行层警示(#73):**有意选择**的 memory 后端数据不落盘——状态行显式
        // 标注,不再只靠配置文件注释。未配置态(无 intentional 标志)由 D43
        // 向导接管,不显示「数据不落盘」(那态连数据都没有)。
        let (_, backend, intentional) = deploy_config();
        let backend_note = if backend == "memory" && intentional {
            " · 内存后端(数据不落盘)"
        } else {
            ""
        };
        h.status.set_text(if self.busy == 1 {
            "walgit 服务:切换中…".into()
        } else if running {
            format!(
                "walgit 服务:运行中 · {}{}",
                self.current_version(),
                backend_note
            )
        } else {
            format!("walgit 服务:已停止{}", backend_note)
        });
        h.toggle.set_text(
            if self.busy == 1 {
                "切换中…"
            } else if running {
                "停止服务"
            } else {
                "启动服务"
            }
            .to_string(),
        );
        h.toggle.set_enabled(self.busy == 0);
        let cur = self.current_version();
        // 无源码仓库(经安装器部署的机器):升级菜单禁点并指路(#73)——检测
        // 管线需要 git 仓库,恒失败只会误导。
        let has_repo = repo_dir().join(".git").exists();
        if !has_repo {
            h.upgrade.set_text(format!("版本 {cur}(经安装器升级)"));
            h.upgrade.set_enabled(false);
        } else {
            h.upgrade.set_text(match self.state {
                ST_CHECKING => format!("版本 {cur} · 正在检查更新…"),
                ST_LATEST => format!("版本 {cur} · 已是最新 ✓(点击重查)"),
                ST_AVAILABLE => format!("⬆️ 升级到新版本 {}(当前 {cur})", self.available_sha),
                ST_INSTALLING => {
                    if self.note.is_empty() {
                        "升级中…".into()
                    } else {
                        format!("升级中… · {}", self.note)
                    }
                }
                ST_FAILED => "上次升级失败(点击重查)".into(),
                _ => format!("版本 {cur} · 检查更新…"),
            });
            h.upgrade.set_enabled(
                self.busy == 0 && self.state != ST_CHECKING && self.state != ST_INSTALLING,
            );
        }
        // 升级中禁用退出:此刻 exit 会把升级线程杀在 停→换→起 之间,
        // 服务留下停机且再无托盘可救。
        h.quit.set_enabled(self.busy != 2);
    }
}

impl ApplicationHandler<Msg> for App {
    fn resumed(&mut self, _loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _loop: &ActiveEventLoop,
        _window: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn user_event(&mut self, _loop: &ActiveEventLoop, msg: Msg) {
        match msg {
            Msg::Status(up) => {
                self.running = up;
                if up {
                    // 版本串从 healthz 取——轮询线程已带;此处只做占位
                }
            }
            // 升级进行中(busy==2)不接受状态翻转:30 分钟检测线程可能在
            // 一次超长 cargo build 中把「升级中…」翻回「⬆️ 可升级」。
            Msg::UpdateState(s) => {
                if self.busy != 2 {
                    self.state = s;
                }
            }
            Msg::Available(sha) => self.available_sha = sha,
            Msg::Note(n) => self.note = n,
            Msg::Busy(b) => self.busy = b,
        }
        self.update_icon();
        self.rebuild_menu();
    }

    fn about_to_wait(&mut self, _loop: &ActiveEventLoop) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id.as_ref().to_string();
            let Some(proxy) = self.proxy.clone() else {
                continue;
            };
            match id.as_str() {
                "start" | "stop" => {
                    if self.busy == 0 {
                        self.busy = 1;
                        self.rebuild_menu();
                        std::thread::spawn(move || {
                            let r = if id == "start" {
                                service_start()
                            } else {
                                service_stop()
                            };
                            log_line(&format!("{id} -> {r:?}"));
                            let _ = proxy.send_event(Msg::Busy(0));
                            let _ = proxy.send_event(Msg::Status(healthz().is_some()));
                        });
                    }
                }
                "upgrade" => match self.state {
                    ST_IDLE | ST_LATEST | ST_FAILED => {
                        self.state = ST_CHECKING;
                        self.rebuild_menu();
                        std::thread::spawn(move || {
                            let repo = repo_dir();
                            let _ = run(Some(&repo), "git", &["fetch", "origin", "main"], &[]);
                            let (c1, lout) = run(Some(&repo), "git", &["rev-parse", "HEAD"], &[]);
                            let (c2, rout) =
                                run(Some(&repo), "git", &["rev-parse", "origin/main"], &[]);
                            let local = lout.trim().to_string();
                            let remote = rout.trim().to_string();
                            if c1 != 0 || c2 != 0 || local.is_empty() || remote.is_empty() {
                                let _ = proxy.send_event(Msg::UpdateState(ST_FAILED));
                            } else if local != remote {
                                let _ = proxy.send_event(Msg::UpdateState(ST_AVAILABLE));
                                let _ = proxy.send_event(Msg::Available(
                                    remote[..7.min(remote.len())].to_string(),
                                ));
                            } else {
                                let _ = proxy.send_event(Msg::UpdateState(ST_LATEST));
                            }
                            log_line(&format!("detect: local={:.7} remote={:.7}", local, remote));
                        });
                    }
                    ST_AVAILABLE => {
                        // 用户点击升级:这里才真正跑升级管线
                        self.state = ST_INSTALLING;
                        self.busy = 2;
                        self.rebuild_menu();
                        std::thread::spawn(move || {
                            let r = upgrade_pipeline(&|m| {
                                let _ = proxy.send_event(Msg::Note(m));
                            });
                            let _ = proxy.send_event(Msg::Busy(0));
                            let _ = proxy.send_event(Msg::Note(String::new()));
                            let _ = proxy.send_event(Msg::UpdateState(match &r {
                                Ok(_) => ST_LATEST,
                                Err(_) => ST_FAILED,
                            }));
                            let _ = proxy.send_event(Msg::Status(healthz().is_some()));
                            log_line(&format!("upgrade -> {r:?}"));
                        });
                    }
                    _ => {}
                },
                "web" => open_web(),
                "quit" => std::process::exit(0), // 仅退托盘;服务是独立进程
                _ => {}
            }
            self.rebuild_menu();
        }
    }
}

// ---------- 单实例(Windows) ----------

/// 命名互斥:已有托盘实例在跑则 false;创建失败(h==0,资源耗尽等)不阻断
/// 启动——没有单实例保护好过托盘起不来。句柄故意泄漏:进程生命周期即持有期。
#[cfg(target_os = "windows")]
fn single_instance_ok() -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = std::ffi::OsStr::new("Local\\walgit-tray")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // GetLastError 须紧跟 CreateMutexW 取,中间不能夹会改写它的调用
        let err = GetLastError();
        if h == 0 {
            log_line(&format!("single-instance mutex FAILED: os error {err}"));
            return true;
        }
        err != ERROR_ALREADY_EXISTS
    }
}

fn main() {
    // 单实例:双击多次不叠图标、不留幽灵进程。
    #[cfg(target_os = "windows")]
    if !single_instance_ok() {
        log_line("second instance — exiting");
        return;
    }
    // GUI 子系统下 panic 无控制台可见,落进 tray.log;链回默认 hook,
    // debug 构建的控制台输出与 RUST_BACKTRACE 照旧。
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_line(&format!("panic: {info}"));
        prev_hook(info);
    }));
    log_line("tray-rs launched");

    // 安装器自启标记(Windows 安装器勾选「开机自动启动」时写):自启的托盘
    // 把服务一并拉起——勾选框承诺的是「部署开机可用」,不是只把托盘拉起来。
    // 仅在服务未运行时尝试一次;失败不重试,留给菜单「启动服务」。
    if deploy_dir().join("service.autostart").exists() && healthz().is_none() {
        log_line("autostart marker: starting service");
        let _ = service_start();
    }
    let event_loop = EventLoop::<Msg>::with_user_event().build().unwrap();
    let proxy = Arc::new(event_loop.create_proxy());

    let icon = tray_icon::Icon::from_rgba(icon_rgba(32, state_color(false, 0)), 32, 32)
        .expect("icon rgba");
    let menu = Menu::new();
    let status = MenuItem::with_id("status", "walgit 服务:检查中…", false, None);
    let toggle = MenuItem::with_id("stop", "停止服务", true, None);
    let upgrade = MenuItem::with_id("upgrade", "版本 … · 检查更新…", true, None);
    let web = MenuItem::with_id("web", "打开 Web UI", true, None);
    let quit = MenuItem::with_id("quit", "退出托盘(服务保持运行)", true, None);
    let _ = menu.append_items(&[
        &status,
        &toggle,
        &PredefinedMenuItem::separator(),
        &upgrade,
        &PredefinedMenuItem::separator(),
        &web,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .with_tooltip("walgit — 仓库活在桶上")
        .build()
        .expect("tray build");

    let mut app = App {
        tray: Some(tray),
        items: Some(MenuHandles {
            status,
            toggle,
            upgrade,
            quit,
        }),
        proxy: Some(proxy.clone()),
        running: healthz().is_some(),
        busy: 0,
        state: ST_IDLE,
        version: String::new(),
        available_sha: String::new(),
        note: String::new(),
    };
    app.rebuild_menu();

    // 状态轮询线程(5s):status + version
    let poll_proxy = proxy;
    std::thread::spawn(move || loop {
        let body = healthz();
        let up = body.is_some();
        let _ = poll_proxy.send_event(Msg::Status(up));
        std::thread::sleep(Duration::from_secs(5));
    });
    // 自动检测线程(启动 30 秒一次,此后每 30 分钟;静默,失败不打扰)。
    // 本机没有源码仓库(安装器部署的机器)就整条停用:检测/升级都依赖
    // WALGIT_REPO 指向的 checkout + rustup/cargo,没有它只会每 30 分钟
    // 往 tray.log 写一条 skip 噪音。升级走新 setup.exe。
    if !repo_dir().join(".git").exists() {
        log_line(&format!(
            "detect: no repo at {} — update checks disabled (set WALGIT_REPO to enable)",
            repo_dir().display()
        ));
    } else {
        let detect_proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            loop {
                let repo = repo_dir();
                let _ = run(Some(&repo), "git", &["fetch", "origin", "main"], &[]);
                let (c1, lout) = run(Some(&repo), "git", &["rev-parse", "HEAD"], &[]);
                let (c2, rout) = run(Some(&repo), "git", &["rev-parse", "origin/main"], &[]);
                let local = lout.trim().to_string();
                let remote = rout.trim().to_string();
                if c1 == 0 && c2 == 0 && !local.is_empty() && !remote.is_empty() {
                    if local != remote {
                        let _ = detect_proxy.send_event(Msg::UpdateState(ST_AVAILABLE));
                        let _ = detect_proxy
                            .send_event(Msg::Available(remote[..7.min(remote.len())].to_string()));
                    }
                    log_line(&format!("detect: local={:.7} remote={:.7}", local, remote));
                } else {
                    log_line("detect: skip (git failed)");
                }
                std::thread::sleep(Duration::from_secs(1800));
            }
        });
    }

    event_loop.run_app(&mut app).unwrap();
}
