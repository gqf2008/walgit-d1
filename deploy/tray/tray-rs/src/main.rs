//! walgit-tray — walgit 服务的系统托盘(macOS / Windows / Linux 一套代码)。
//!
//! 菜单:状态行 · 启动/停止 · 版本升级状态行(abb 同款状态机) ·
//!      打开 Web UI · 退出托盘(服务保持运行)。
//! 升级语义:自动的只有「检测」(每 30 分钟 fetch 比对,启动 30 秒先查一次);
//!      发现新版本只把菜单行变成「⬆️ 升级到新版本」,**由用户点击才升级**:
//!      ff-merge main → cargo 构建 → 备份 → 停 → 热换 → 健康验证,失败回滚。
//!
//! 服务控制:直接调用安装目录/App Bundle 里的 `walgit service`；程序文件
//!          不复制进 ~/.walgit，状态、配置和日志才属于那里。
//! 健康检查:内置裸 HTTP(loopback),零额外依赖。
//! 打开 Web UI:直接开新页面(三平台一致)。
//! macOS 升级:Release 感知(检测 GitHub latest release → 下载 DMG →
//!          校验 sha256/签名/公证 → 交给 release-install.sh 换装回滚)。
//!
//! 版本比较、release 解析与菜单文本在 `release.rs`(纯函数 + 单测)。

// Windows release 不带控制台:双击静默驻留托盘(debug 构建保留控制台便于排查)。
// 注意 start 引号:cmd 只认双引号,`start "" "url"` 的 "" 是占位标题。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;
mod release;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use release::{
    upgrade_line, ReleaseInfo, ST_AVAILABLE, ST_CHECKING, ST_FAILED, ST_IDLE, ST_INSTALLING,
    ST_LATEST,
};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};

const DEFAULT_LISTEN: &str = "127.0.0.1:8081";
#[cfg(target_os = "macos")]
const DEFAULT_RELEASE_API: &str = "https://api.github.com/repos/gqf2008/walgit-d1/releases/latest";

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

fn state_dir() -> PathBuf {
    // macOS:WALGIT_STATE_DIR / WALGIT_DEPLOY_DIR 优先(升级 helper 用
    // `open --env WALGIT_DEPLOY_DIR=…` 把新 app 指回同一份状态)。
    #[cfg(target_os = "macos")]
    let dir = bootstrap::state_dir();
    #[cfg(not(target_os = "macos"))]
    let dir = {
        let home = home();
        if home.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            home.join(".walgit")
        }
    };
    dir
}

/// Program location, never the state directory. In a macOS .app this is
/// Contents/Resources/walgit; Windows/Linux installers put it next to the tray.
fn walgit_binary() -> PathBuf {
    if let Ok(path) = std::env::var("WALGIT_BIN") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            #[cfg(target_os = "macos")]
            if let Some(contents) = dir.parent() {
                let bundled = contents.join("Resources").join(exe_name());
                if bundled.is_file() {
                    return bundled;
                }
            }
            let flat = dir.join(exe_name());
            if flat.is_file() {
                return flat;
            }
        }
    }
    home().join(".local/bin").join(exe_name())
}

fn repo_dir() -> PathBuf {
    std::env::var("WALGIT_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            #[cfg(target_os = "macos")]
            {
                PathBuf::from("/Volumes/DataExt/GitHub/walgit")
            }
            #[cfg(not(target_os = "macos"))]
            {
                home().join("walgit-repo")
            }
        })
}

/// 运行中的 App Bundle(`…/walgit-tray.app`)。非 bundle 形态(开发机直接跑
/// target/release/walgit-tray、Windows/Linux 安装目录)返回 None——Release
/// 升级要替换的正是 bundle 本身。
#[cfg(target_os = "macos")]
fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.extension()? != "app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

/// 托盘/App 版本。macOS 取 bundle 的 `CFBundleShortVersionString`(升级判断
/// 与菜单显示都以它为准,与服务进程版本分开);其他平台允许安装器用
/// `WALGIT_APP_VERSION` 注入,未注入时回退服务版本(见 `App::current_version`)。
fn app_version() -> String {
    #[cfg(target_os = "macos")]
    {
        use release::parse_bundle_version;

        if let Some(bundle) = app_bundle() {
            if let Ok(plist) = std::fs::read_to_string(bundle.join("Contents/Info.plist")) {
                if let Some(version) = parse_bundle_version(&plist) {
                    return version;
                }
            }
        }
        String::new()
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("WALGIT_APP_VERSION").unwrap_or_default()
    }
}

/// GitHub latest release(或 `WALGIT_RELEASE_FIXTURE` 指向的本地 JSON)。
/// 只信 API 里的 sha256 digest:拿不到就当作「没有可用更新」而不是安装
/// 一个未校验的包。只有 macOS 的 Release 通道会用它。
#[cfg(target_os = "macos")]
fn latest_release() -> Option<ReleaseInfo> {
    use release::{arch_slug, parse_latest_release};

    if let Ok(fixture) = std::env::var("WALGIT_RELEASE_FIXTURE") {
        if !fixture.is_empty() {
            let body = std::fs::read_to_string(&fixture).ok()?;
            return parse_latest_release(&body, arch_slug()).ok();
        }
    }
    let endpoint =
        std::env::var("WALGIT_RELEASE_API").unwrap_or_else(|_| DEFAULT_RELEASE_API.to_string());
    let (code, body) = run(
        None,
        "curl",
        &[
            "-fsSL",
            "--retry",
            "2",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: walgit-tray",
            &endpoint,
        ],
        &[],
    );
    if code != 0 {
        log_line(&format!(
            "release: latest lookup failed {}",
            body.trim().chars().take(160).collect::<String>()
        ));
        return None;
    }
    match parse_latest_release(&body, arch_slug()) {
        Ok(info) => Some(info),
        Err(e) => {
            log_line(&format!("release: {e}"));
            None
        }
    }
}

/// 检测结果:菜单状态机据此选「下载并升级(Release)」还是「从源码升级」。
enum Detected {
    Nothing,
    Source(String),
    #[cfg(target_os = "macos")]
    Release(ReleaseInfo),
}

/// 源码仓库是否可做源码升级。
fn has_source_repo() -> bool {
    repo_dir().join(".git").exists()
}

/// 是否跑在 DMG 装出来的 App Bundle 里(Release 升级通道)。
fn release_channel() -> bool {
    #[cfg(target_os = "macos")]
    {
        app_bundle().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// 检测可用更新。macOS 优先 Release(装好的 DMG 机器不需要源码仓库);
/// Release 通道不可用(无网/无 bundle/资产缺失)且本机有源码仓库时退回源码检测。
fn detected_update() -> Detected {
    // 每条检测都留痕:菜单为什么写「可升级/已最新」要能从 tray.log 倒推,
    // 否则现场只能猜(原 Swift 托盘同样打 detect 行)。
    #[cfg(target_os = "macos")]
    {
        use release::is_version_newer;

        if app_bundle().is_some() {
            let current = app_version();
            match latest_release() {
                Some(info) => {
                    let newer = is_version_newer(&info.version, &current);
                    log_line(&format!(
                        "detect: app={current} release=v{} → {}",
                        info.version,
                        if newer { "available" } else { "up-to-date" }
                    ));
                    return if newer {
                        Detected::Release(info)
                    } else {
                        Detected::Nothing
                    };
                }
                None => log_line(&format!(
                    "detect: app={current} release=none(lookup 失败)→ 退回源码检测"
                )),
            }
        }
    }
    if !has_source_repo() {
        log_line(&format!(
            "detect: source=none(无仓库 {})",
            repo_dir().display()
        ));
        return Detected::Nothing;
    }
    let repo = repo_dir();
    let _ = run(Some(&repo), "git", &["fetch", "origin", "main"], &[]);
    let (c1, lout) = run(Some(&repo), "git", &["rev-parse", "HEAD"], &[]);
    let (c2, rout) = run(Some(&repo), "git", &["rev-parse", "origin/main"], &[]);
    let local = lout.trim().to_string();
    let remote = rout.trim().to_string();
    if c1 != 0 || c2 != 0 || local.is_empty() || remote.is_empty() {
        log_line("detect: source skip(git failed)");
        return Detected::Nothing;
    }
    let short = |sha: &str| sha[..7.min(sha.len())].to_string();
    log_line(&format!(
        "detect: local={} remote={} → {}",
        short(&local),
        short(&remote),
        if local == remote {
            "up-to-date"
        } else {
            "available"
        }
    ));
    if local == remote {
        return Detected::Nothing;
    }
    Detected::Source(short(&remote))
}

/// 把检测结果送进事件循环。
fn publish_detected(proxy: &EventLoopProxy<Msg>, detected: Detected) {
    match detected {
        Detected::Nothing => {
            let _ = proxy.send_event(Msg::Release(None));
            let _ = proxy.send_event(Msg::UpdateState(ST_LATEST));
        }
        Detected::Source(sha) => {
            let _ = proxy.send_event(Msg::Release(None));
            let _ = proxy.send_event(Msg::Available(sha));
            let _ = proxy.send_event(Msg::UpdateState(ST_AVAILABLE));
        }
        #[cfg(target_os = "macos")]
        Detected::Release(info) => {
            let _ = proxy.send_event(Msg::Available(String::new()));
            let _ = proxy.send_event(Msg::Release(Some(info.clone())));
            let _ = proxy.send_event(Msg::UpdateState(ST_AVAILABLE));
        }
    }
}

/// macOS Release 升级:下载 DMG → 校验 sha256 → 挂载 → 校验签名/公证/版本
/// → 交给 `release-install.sh` 换装(它等托盘退出,失败回滚)。
/// 成功返回后调用方立即退出托盘进程,把 bundle 让给辅助脚本。
#[cfg(target_os = "macos")]
fn release_upgrade(report: &dyn Fn(String), release: &ReleaseInfo) -> Result<String, String> {
    use release::parse_bundle_version;

    let bundle =
        app_bundle().ok_or_else(|| "不是 App Bundle 安装,无法走 Release 升级".to_string())?;
    let cache = home().join("Library/Caches/walgit");
    std::fs::create_dir_all(&cache).map_err(|e| format!("创建缓存目录失败: {e}"))?;
    let dmg = cache.join(&release.asset.name);
    let _ = std::fs::remove_file(&dmg);

    report("下载安装包…".into());
    let (code, out) = run(
        None,
        "curl",
        &[
            "-fL",
            "--retry",
            "2",
            "--connect-timeout",
            "10",
            "--max-time",
            "600",
            "-o",
            &dmg.to_string_lossy(),
            &release.asset.url,
        ],
        &[],
    );
    if code != 0 {
        return Err(format!("下载安装包失败: {}", tail(&out, 200)));
    }

    report("校验安装包…".into());
    let (code, out) = run(None, "shasum", &["-a", "256", &dmg.to_string_lossy()], &[]);
    let got = out
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if code != 0 || got != release.asset.sha256 {
        return Err("SHA-256 校验失败".into());
    }

    let mount = cache.join(format!("mount-{}-{}", release.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&mount);
    std::fs::create_dir_all(&mount).map_err(|e| format!("创建挂载点失败: {e}"))?;
    let mount_text = mount.to_string_lossy().to_string();
    let dmg_text = dmg.to_string_lossy().to_string();
    let (code, out) = run(
        None,
        "hdiutil",
        &[
            "attach",
            "-nobrowse",
            "-readonly",
            "-mountpoint",
            &mount_text,
            &dmg_text,
        ],
        &[],
    );
    if code != 0 {
        return Err(format!("挂载 DMG 失败: {}", tail(&out, 200)));
    }
    let mut mount_guard = MountGuard::new(mount_text.clone());

    let staged = mount.join("walgit-tray.app");
    let staged_text = staged.to_string_lossy().to_string();
    let verify = (|| -> Result<(), String> {
        let plist = std::fs::read_to_string(staged.join("Contents/Info.plist"))
            .map_err(|e| format!("DMG 内 app 版本不可读: {e}"))?;
        let version =
            parse_bundle_version(&plist).ok_or_else(|| "DMG 内 app 版本不可读".to_string())?;
        if version != release::strip_version_prefix(&release.version) {
            return Err(format!(
                "DMG 内 app 版本不匹配(包内 {version},release {})",
                release.version
            ));
        }
        let (code, out) = run(
            None,
            "codesign",
            &["--verify", "--deep", "--strict", &staged_text],
            &[],
        );
        if code != 0 {
            return Err(format!("DMG 内 app 签名校验失败: {}", tail(&out, 200)));
        }
        let (code, out) = run(
            None,
            "spctl",
            &["--assess", "--type", "execute", &staged_text],
            &[],
        );
        if code != 0 {
            return Err(format!("DMG 内 app 公证校验失败: {}", tail(&out, 200)));
        }
        Ok(())
    })();
    verify?;

    report("换装中…".into());
    let script = bundle.join("Contents/Resources/release-install.sh");
    if !script.is_file() {
        return Err("缺少 release-install.sh".into());
    }
    let _ = std::fs::create_dir_all(state_dir());
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("tray.log"))
        .map_err(|e| format!("打开升级日志失败: {e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("打开升级日志失败: {e}"))?;
    let mut cmd = std::process::Command::new("/bin/bash");
    cmd.arg(&script)
        .arg(&dmg)
        .arg(&mount)
        .arg(&bundle)
        .arg(&release.version)
        .arg(std::process::id().to_string())
        .env("WALGIT_DEPLOY_DIR", state_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    cmd.spawn()
        .map_err(|e| format!("启动升级辅助脚本失败: {e}"))?;
    // 换装脚本接管挂载点(它会 detach),从这里起不再由我们卸载。
    mount_guard.disarm();
    log_line(&format!(
        "release: updater spawned for v{}",
        release.version
    ));
    Ok(format!("v{}", release.version))
}

/// attach 成功后,任何返回路径都必须 detach——用 RAII 而不是在每个 `?`/`return`
/// 前手写一次(审查指出:`OpenOptions::open` / `spawn` 的失败分支漏了 detach)。
/// 交棒给 `release-install.sh` 前 `disarm()`:换装脚本自己负责卸载。
#[cfg(target_os = "macos")]
struct MountGuard {
    mount: String,
    armed: bool,
}

#[cfg(target_os = "macos")]
impl MountGuard {
    fn new(mount: String) -> Self {
        Self { mount, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = run(None, "hdiutil", &["detach", &self.mount], &[]);
        }
    }
}

/// 错误串只带尾部若干字符:命令输出可能很长,菜单/日志只需要线索。
#[cfg(target_os = "macos")]
fn tail(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    let skip = trimmed.chars().count().saturating_sub(max);
    trimmed.chars().skip(skip).collect()
}

fn exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "walgit.exe"
    } else {
        "walgit"
    }
}

fn log_line(s: &str) {
    let dir = state_dir();
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
    let path = state_dir().join("walgit.toml");
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

/// 从 healthz 响应体提取 `version` 字段(与 release-install.sh 的
/// `health_version` 同口径)。用 JSON 解析而不是字符串包含:v0.5.1 不能匹配
/// v0.5.10,排版带空格(手写/再序列化过的 JSON)也要能读。
fn version_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|v| {
            v.get("version")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

// ---------- shell ----------

// POSIX sh:open / xdg-open / kill 仍走字符串(真 sh
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

/// macOS uses the in-binary service command. Windows/Linux keep the tray-side
/// supervisor because `walgit serve` exits 75 after the setup wizard saves and
/// needs an immediate respawn. Either way, no walgit-ensure shell and no binary
/// copy under ~/.walgit.
#[cfg(target_os = "macos")]
fn service_cmd(verb: &str) -> Result<(), String> {
    let bin = walgit_binary();
    if !bin.is_file() {
        return Err(format!("missing walgit binary: {}", bin.display()));
    }
    let cfg = state_dir().join("walgit.toml");
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["service", verb, "--config"]).arg(&cfg);
    let out = cmd
        .output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(())
    } else {
        Err(text)
    }
}

#[cfg(target_os = "macos")]
fn service_start() -> Result<(), String> {
    service_cmd("start")
}

#[cfg(target_os = "macos")]
fn service_stop() -> Result<(), String> {
    service_cmd("stop")
}

#[cfg(not(target_os = "macos"))]
fn pid_file() -> PathBuf {
    state_dir().join("walgit.pid")
}

#[cfg(not(target_os = "macos"))]
fn service_start() -> Result<(), String> {
    let child = spawn_service()?;
    std::fs::write(pid_file(), child.id().to_string()).map_err(|e| format!("pidfile: {e}"))?;
    // D43: the setup wizard's save exits 75 ("restart me"). Supervise the
    // child so 保存并重启 is one click — respawn on 75, bounded (a real
    // restart loop means the written config does not hold).
    std::thread::spawn(move || supervise_service(child));
    Ok(())
}

/// Spawn `walgit serve --config <state>/walgit.toml` detached (windows: no
/// console window, new group; unix: own session).
#[cfg(not(target_os = "macos"))]
fn spawn_service() -> Result<std::process::Child, String> {
    let exe = walgit_binary();
    let cfg = state_dir().join("walgit.toml");
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

#[cfg(not(target_os = "macos"))]
fn service_stop() -> Result<(), String> {
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

/// 当前服务进程版本(healthz 的 version 字段);服务不在时为空。
fn service_version() -> String {
    healthz().map(|body| version_of(&body)).unwrap_or_default()
}

/// 升级入口:选择了 Release(macOS App Bundle)走 DMG 管线,否则走源码管线。
fn run_upgrade(report: &dyn Fn(String), release: Option<&ReleaseInfo>) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(release) = release {
            return release_upgrade(report, release);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = release;
    upgrade_pipeline(report)
}

/// 升级管线(仅由用户点击触发):fetch → ff-merge → 构建 → 备份 → 停 → 换 → 起 → 验证。
/// 换装阶段任何一步失败都走同一条回滚路径(还原备份 → 重启服务),并把回滚
/// 本身的结果如实写进错误串——不谎报「已回滚」。
fn upgrade_pipeline(report: &dyn Fn(String)) -> Result<String, String> {
    let repo = repo_dir();
    let bin = walgit_binary();
    let bin_text = bin.to_string_lossy();
    if bin_text.contains(".app/Contents/") || bin_text.starts_with("/usr/") {
        return Err("该安装由 App Bundle / 系统包管理，请使用 Release 或安装器升级".into());
    }

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
    let bak = bin.with_extension("bak-tray");
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
    Status(bool, String),
    UpdateState(u8),
    Available(String),
    Release(Option<ReleaseInfo>),
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
    busy: u8,                // 0 idle, 1 switching, 2 upgrading
    state: u8,               // 升级状态机(ST_*)
    version: String,         // 托盘/App 版本(#183:升级判断的唯一依据)
    service_version: String, // healthz 报的服务进程版本,只作展示
    available_sha: String,   // 源码升级目标
    release: Option<ReleaseInfo>,
    note: String,
}

impl App {
    /// 菜单里的「当前版本」:App 版本优先;没有 App 版本(Windows/Linux
    /// 安装目录形态)时用服务版本兜底,避免状态行印一个空版本。
    fn current_version(&self) -> String {
        let app = release::strip_version_prefix(&self.version);
        if !app.is_empty() {
            return app;
        }
        let service = release::strip_version_prefix(&self.service_version);
        if !service.is_empty() {
            return service;
        }
        "…".to_string()
    }

    /// 与服务版本不同才单独展示(#170:别在「已是最新」旁印一个更旧的服务版本)。
    fn service_line(&self) -> String {
        let service = release::strip_version_prefix(&self.service_version);
        if service.is_empty() || service == self.current_version() {
            String::new()
        } else {
            service
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
        h.toggle.set_text(if self.busy == 1 {
            "切换中…"
        } else if running {
            "停止服务"
        } else {
            "启动服务"
        });
        h.toggle.set_enabled(self.busy == 0);
        // 升级通道:macOS 装好的 DMG 走 Release(不需要源码仓库);开发机
        // 与 Windows/Linux 走源码仓库(#73:都没有时菜单禁点并指路)。
        let can_upgrade = release_channel() || has_source_repo() || self.release.is_some();
        if !can_upgrade {
            h.upgrade
                .set_text(format!("版本 {}(经安装器升级)", self.current_version()));
            h.upgrade.set_enabled(false);
        } else {
            h.upgrade.set_text(upgrade_line(
                self.state,
                &self.current_version(),
                &self.service_line(),
                self.release.as_ref(),
                &self.available_sha,
                &self.note,
            ));
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
            Msg::Status(up, service_version) => {
                self.running = up;
                if up {
                    self.service_version = service_version;
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
            Msg::Release(info) => self.release = info,
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
                            let _ = proxy
                                .send_event(Msg::Status(healthz().is_some(), service_version()));
                        });
                    }
                }
                "upgrade" => match self.state {
                    ST_IDLE | ST_LATEST | ST_FAILED => {
                        self.state = ST_CHECKING;
                        self.rebuild_menu();
                        std::thread::spawn(move || {
                            let detected = detected_update();
                            publish_detected(&proxy, detected);
                        });
                    }
                    ST_AVAILABLE => {
                        // 用户点击升级:这里才真正跑升级管线
                        self.state = ST_INSTALLING;
                        self.busy = 2;
                        self.rebuild_menu();
                        let release = self.release.clone();
                        std::thread::spawn(move || {
                            let r = run_upgrade(
                                &|m| {
                                    let _ = proxy.send_event(Msg::Note(m));
                                },
                                release.as_ref(),
                            );
                            // macOS Release 交棒成功:辅助脚本要替换正在运行的
                            // bundle(它先等本 pid 消失),这里直接退场——不再
                            // 做 healthz 探测/状态回灌,免得拖过它的等待窗口。
                            if r.is_ok() && release.is_some() {
                                log_line(&format!("upgrade -> {r:?}"));
                                std::process::exit(0);
                            }
                            let _ = proxy.send_event(Msg::Busy(0));
                            let _ = proxy.send_event(Msg::Note(String::new()));
                            let _ = proxy.send_event(Msg::UpdateState(if r.is_ok() {
                                ST_LATEST
                            } else {
                                ST_FAILED
                            }));
                            let _ = proxy
                                .send_event(Msg::Status(healthz().is_some(), service_version()));
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

    // macOS 首次启动:状态目录初始化、旧布局迁移、CLI 软链。测试用它单独
    // 跑一遍 bootstrap 后立即退出(WALGIT_BOOTSTRAP_ONLY=1)。
    bootstrap::bootstrap_deploy();
    if std::env::var("WALGIT_BOOTSTRAP_ONLY").as_deref() == Ok("1") {
        return;
    }

    // 检测一次就退出(WALGIT_DETECT_ONCE=1):CI 的 macOS leg 没有可靠窗口
    // 服务器,但"菜单说升级到哪个版本"是 Release 通道最该被守住的语义。
    // 与 fixture(WALGIT_RELEASE_FIXTURE)配合即可离线断言。
    if std::env::var("WALGIT_DETECT_ONCE").as_deref() == Ok("1") {
        let detected = detected_update();
        let (state, release, sha) = match detected {
            Detected::Nothing => (ST_LATEST, None, String::new()),
            Detected::Source(sha) => (ST_AVAILABLE, None, sha),
            #[cfg(target_os = "macos")]
            Detected::Release(info) => (ST_AVAILABLE, Some(info), String::new()),
        };
        println!(
            "{}",
            upgrade_line(
                state,
                &app_version(),
                &service_version(),
                release.as_ref(),
                &sha,
                ""
            )
        );
        return;
    }

    // 安装器自启标记(Windows 安装器勾选「开机自动启动」时写):自启的托盘
    // 把服务一并拉起——勾选框承诺的是「部署开机可用」,不是只把托盘拉起来。
    // 仅在服务未运行时尝试一次;失败不重试,留给菜单「启动服务」。
    if state_dir().join("service.autostart").exists() && healthz().is_none() {
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
        version: app_version(),
        service_version: service_version(),
        available_sha: String::new(),
        release: None,
        note: String::new(),
    };
    app.rebuild_menu();

    // 状态轮询线程(5s):status + 服务版本
    let poll_proxy = proxy;
    std::thread::spawn(move || loop {
        let body = healthz();
        let up = body.is_some();
        let version = body.as_deref().map(version_of).unwrap_or_default();
        let _ = poll_proxy.send_event(Msg::Status(up, version));
        std::thread::sleep(Duration::from_secs(5));
    });
    // 自动检测线程(启动 30 秒一次,此后每 30 分钟;静默,失败不打扰)。
    // 两条通道:macOS App Bundle 走 GitHub Release(机器上不需要源码仓库);
    // 开发机与 Windows/Linux 走 WALGIT_REPO 指向的 checkout + rustup/cargo。
    // 两条都没有时整条停用——否则只会每 30 分钟往 tray.log 写一条 skip 噪音。
    if !release_channel() && !has_source_repo() {
        log_line(&format!(
            "detect: no repo at {} — update checks disabled (set WALGIT_REPO to enable)",
            repo_dir().display()
        ));
    } else {
        let detect_proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            loop {
                publish_detected(&detect_proxy, detected_update());
                std::thread::sleep(Duration::from_secs(1800));
            }
        });
    }

    event_loop.run_app(&mut app).unwrap();
}
