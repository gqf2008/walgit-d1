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
//! Release 升级:macOS 下载 DMG → 校验 sha256/签名/公证 → 交给
//! release-install.sh 换装回滚;Windows 下载 Inno 安装器 → 交给独立
//! walgit-upgrade-helper 静默安装/健康检查/回滚。
//!
//! 版本比较、release 解析与菜单文本在 `release.rs`(纯函数 + 单测)。

// Windows release 不带控制台:双击静默驻留托盘(debug 构建保留控制台便于排查)。
// 注意 start 引号:cmd 只认双引号,`start "" "url"` 的 "" 是占位标题。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use walgit_tray::release::{
    self, upgrade_line, ReleaseInfo, ReleaseTarget, ST_AVAILABLE, ST_CHECKING, ST_CHECK_FAILED,
    ST_FAILED, ST_IDLE, ST_INSTALLING, ST_LATEST,
};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};

const DEFAULT_LISTEN: &str = "127.0.0.1:8081";
const DEFAULT_RELEASE_API: &str = "https://api.github.com/repos/gqf2008/walgit-d1/releases/latest";
#[cfg(target_os = "windows")]
const WINDOWS_HELPER_EXE: &str = "walgit-upgrade-helper.exe";
#[cfg(target_os = "windows")]
const WINDOWS_INSTALL_MARKER: &str = ".walgit-install";

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

/// 托盘/App 版本。macOS 取 bundle 的 `CFBundleShortVersionString`;Windows
/// 优先取测试注入，否则运行安装目录的 `walgit.exe --version`。菜单在没有
/// app 版本时才回退服务版本（见 `App::current_version`）。
fn app_version() -> String {
    #[cfg(target_os = "macos")]
    {
        use walgit_tray::release::parse_bundle_version;

        if let Some(bundle) = app_bundle() {
            if let Ok(plist) = std::fs::read_to_string(bundle.join("Contents/Info.plist")) {
                if let Some(version) = parse_bundle_version(&plist) {
                    return version;
                }
            }
        }
        String::new()
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(version) = std::env::var("WALGIT_APP_VERSION") {
            let version = release::strip_version_prefix(&version);
            if !version.is_empty() {
                return version;
            }
        }
        let bin = walgit_binary();
        let (code, out) = run(None, &bin.to_string_lossy(), &["--version"], &[]);
        if code == 0 {
            return release::parse_tool_version(&out).unwrap_or_default();
        }
        String::new()
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        std::env::var("WALGIT_APP_VERSION").unwrap_or_default()
    }
}

/// 当前平台对应的 Release 资产族。
fn release_target() -> Result<ReleaseTarget, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(ReleaseTarget::Macos)
    }
    #[cfg(target_os = "windows")]
    {
        Ok(ReleaseTarget::Windows)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("release updates are not enabled on this platform".into())
    }
}

/// Query one GitHub release endpoint. The asset name is chosen by target and
/// `sha256` is mandatory; a missing digest never falls back to an unverified
/// download.
fn fetch_release(endpoint: &str, label: &str) -> Result<ReleaseInfo, String> {
    use walgit_tray::release::{arch_slug, parse_latest_release};

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
            endpoint,
        ],
        &[],
    );
    if code != 0 {
        return Err(format!(
            "{label} lookup failed {}: {}",
            endpoint,
            body.trim().chars().take(160).collect::<String>()
        ));
    }
    parse_latest_release(&body, release_target()?, arch_slug())
}

/// GitHub latest release(或 `WALGIT_RELEASE_FIXTURE` 指向的本地 JSON)。
fn latest_release() -> Result<ReleaseInfo, String> {
    use walgit_tray::release::{arch_slug, parse_latest_release};

    if let Ok(fixture) = std::env::var("WALGIT_RELEASE_FIXTURE") {
        if !fixture.is_empty() {
            let body = std::fs::read_to_string(&fixture)
                .map_err(|e| format!("cannot read fixture {fixture}: {e}"))?;
            return parse_latest_release(&body, release_target()?, arch_slug());
        }
    }
    let endpoint =
        std::env::var("WALGIT_RELEASE_API").unwrap_or_else(|_| DEFAULT_RELEASE_API.to_string());
    fetch_release(&endpoint, "latest")
}

/// The installer for the currently installed version, used only for rollback.
/// This is deliberately a fresh API lookup: an installer kept inside the
/// version being replaced would already have lost the rollback copy.
#[cfg(target_os = "windows")]
fn release_for_version(version: &str) -> Result<ReleaseInfo, String> {
    use walgit_tray::release::{arch_slug, parse_latest_release};

    if let Ok(fixture) = std::env::var("WALGIT_ROLLBACK_RELEASE_FIXTURE") {
        if !fixture.is_empty() {
            let body = std::fs::read_to_string(&fixture)
                .map_err(|e| format!("cannot read rollback fixture {fixture}: {e}"))?;
            return parse_latest_release(&body, release_target()?, arch_slug());
        }
    }
    let version = release::strip_version_prefix(version);
    let endpoint = if let Ok(endpoint) = std::env::var("WALGIT_RELEASE_TAG_API") {
        endpoint
    } else {
        let base =
            std::env::var("WALGIT_RELEASE_API").unwrap_or_else(|_| DEFAULT_RELEASE_API.to_string());
        let prefix = base.strip_suffix("/latest").ok_or_else(|| {
            format!("cannot derive tag release endpoint from {base}; set WALGIT_RELEASE_TAG_API")
        })?;
        format!("{prefix}/tags/v{version}")
    };
    fetch_release(&endpoint, "release")
}

/// 检测结果:菜单状态机据此选「下载并升级(Release)」还是「从源码升级」。
#[derive(Debug, Clone)]
enum Detected {
    Nothing,
    Source(String),
    Failed,
    Release(ReleaseInfo),
}

/// 源码仓库是否可做源码升级。
fn has_source_repo() -> bool {
    repo_dir().join(".git").exists()
}

/// Normalize a Windows path for case-insensitive, separator-insensitive
/// comparison. Kept platform-neutral so the install-layout predicate is tested
/// on every CI leg.
#[cfg(any(target_os = "windows", test))]
fn normalize_windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Windows installer locations that are release-managed. The current installer
/// uses `%LOCALAPPDATA%\\Programs\\walgit`; the older `%USERPROFILE%\\walgit`
/// remains recognized because the installer explicitly migrates that layout.
#[cfg(any(target_os = "windows", test))]
fn windows_install_dir_matches(
    dir: &std::path::Path,
    local_app_data: Option<&str>,
    user_profile: Option<&str>,
) -> bool {
    let dir = normalize_windows_path(dir);
    let mut candidates = Vec::new();
    if let Some(base) = local_app_data.filter(|base| !base.is_empty()) {
        candidates.push(std::path::Path::new(base).join("Programs").join("walgit"));
    }
    if let Some(base) = user_profile.filter(|base| !base.is_empty()) {
        candidates.push(std::path::Path::new(base).join("walgit"));
    }
    candidates
        .iter()
        .any(|candidate| normalize_windows_path(candidate) == dir)
}

#[cfg(target_os = "windows")]
fn windows_install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let local = std::env::var("LOCALAPPDATA").ok();
    let profile = std::env::var("USERPROFILE").ok();
    let managed = windows_install_dir_matches(dir, local.as_deref(), profile.as_deref())
        || dir.join(WINDOWS_INSTALL_MARKER).is_file();
    managed.then(|| dir.to_path_buf())
}

/// Whether the running tray is a release-managed installation. macOS uses the
/// App Bundle shape; Windows uses the installer directory. A source checkout
/// therefore stays on the source-upgrade path on both platforms.
fn release_channel() -> bool {
    #[cfg(target_os = "macos")]
    {
        app_bundle().is_some()
    }
    #[cfg(target_os = "windows")]
    {
        windows_install_dir().is_some()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Detect an available update. Installed Release builds do not need a source
/// checkout; a release lookup failure falls back to source only when that
/// checkout exists.
fn detected_update() -> Detected {
    // 每条检测都留痕:菜单为什么写「可升级/已最新」要能从 tray.log 倒推,
    // 否则现场只能猜(原 Swift 托盘同样打 detect 行)。
    if release_channel() {
        let app = app_version();
        let current = if app.is_empty() {
            service_version()
        } else {
            app
        };
        if current.is_empty() {
            log_line("detect: current app version is unknown");
            if !has_source_repo() {
                return Detected::Failed;
            }
        } else {
            match latest_release() {
                Ok(info) => {
                    let newer = release::is_version_newer(&info.version, &current);
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
                Err(reason) => {
                    log_line(&format!(
                        "detect: app={current} release unavailable ({reason})"
                    ));
                    if !has_source_repo() {
                        return Detected::Failed;
                    }
                }
            }
        }
    }
    if !has_source_repo() {
        log_line(&format!(
            "detect: source=none(无仓库 {})",
            repo_dir().display()
        ));
        return Detected::Failed;
    }
    let repo = repo_dir();
    let (fc, fout) = run(Some(&repo), "git", &["fetch", "origin", "main"], &[]);
    if fc != 0 {
        log_line(&format!(
            "detect: source fetch failed {}",
            fout.trim().chars().take(160).collect::<String>()
        ));
        return Detected::Failed;
    }
    let (c1, lout) = run(Some(&repo), "git", &["rev-parse", "HEAD"], &[]);
    let (c2, rout) = run(Some(&repo), "git", &["rev-parse", "origin/main"], &[]);
    let local = lout.trim().to_string();
    let remote = rout.trim().to_string();
    if c1 != 0 || c2 != 0 || local.is_empty() || remote.is_empty() {
        log_line("detect: source skip(git failed)");
        return Detected::Failed;
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

/// 每次检测启动都分配递增 generation。自动检测和点击重试可能并发,
/// 事件循环只接受最新 generation 的结果,晚到的旧结果不得覆盖新结果。
#[derive(Default)]
struct DetectEpoch {
    next: u64,
    latest: u64,
}

impl DetectEpoch {
    fn start(&mut self) -> u64 {
        self.next += 1;
        self.latest = self.next;
        self.latest
    }

    /// 作废当前这一代检测。进入升级时调用,让升级前发出的结果永久失效。
    fn invalidate(&mut self) {
        self.next += 1;
        self.latest = self.next;
    }

    fn accepts(&self, generation: u64, busy: u8) -> bool {
        busy != 2 && generation == self.latest
    }
}

/// 自动检测只在空闲且没有需要用户处理的终止状态时触发。升级失败必须
/// 停在 ST_FAILED,不能被定时器悄悄翻回「可升级/已最新」。
fn auto_detect_allowed(busy: u8, state: u8) -> bool {
    busy == 0 && !matches!(state, ST_CHECKING | ST_INSTALLING | ST_FAILED)
}

/// macOS Release 升级:下载 DMG → 校验 sha256 → 挂载 → 校验签名/公证/版本
/// → 交给 `release-install.sh` 换装(它等托盘退出,失败回滚)。
/// 成功返回后调用方立即退出托盘进程,把 bundle 让给辅助脚本。
#[cfg(target_os = "macos")]
fn release_upgrade(report: &dyn Fn(String), release: &ReleaseInfo) -> Result<String, String> {
    use walgit_tray::release::parse_bundle_version;

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
    let got = walgit_tray::upgrade_helper::sha256_file(&dmg)?;
    if got != release.asset.sha256 {
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

/// Download and verify one release asset into the update staging directory.
#[cfg(target_os = "windows")]
fn download_release_asset(
    report: &dyn Fn(String),
    directory: &std::path::Path,
    release: &ReleaseInfo,
) -> Result<PathBuf, String> {
    let path = directory.join(&release.asset.name);
    report(format!("下载安装包 v{}…", release.version));
    let path_text = path.to_string_lossy().to_string();
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
            &path_text,
            &release.asset.url,
        ],
        &[],
    );
    if code != 0 {
        return Err(format!("下载安装包失败: {}", tail(&out, 200)));
    }
    report("校验安装包…".into());
    let got = walgit_tray::upgrade_helper::sha256_file(&path)?;
    if got != release.asset.sha256 {
        return Err(format!("SHA-256 校验失败: {}", release.asset.name));
    }
    Ok(path)
}

/// Windows Release upgrade: download the new and rollback installers, copy the
/// helper out of the installation directory, then hand the sequence to it. The
/// tray exits immediately after the helper is running so the installer can
/// replace both tray and service binaries.
#[cfg(target_os = "windows")]
fn release_upgrade_windows(
    report: &dyn Fn(String),
    release: &ReleaseInfo,
) -> Result<String, String> {
    let install =
        windows_install_dir().ok_or_else(|| "不是安装器布局,无法走 Release 升级".to_string())?;
    let app = app_version();
    let current = if app.is_empty() {
        service_version()
    } else {
        app
    };
    let current = release::strip_version_prefix(&current);
    if current.is_empty() {
        return Err("无法读取当前版本,拒绝在无法回滚时升级".into());
    }
    if !release::is_version_newer(&release.version, &current) {
        return Err(format!(
            "release v{} 不比当前 v{current} 新",
            release.version
        ));
    }

    let update = state_dir()
        .join("update")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&update).map_err(|e| format!("创建升级目录失败: {e}"))?;
    let new_installer = download_release_asset(report, &update, release)?;

    report("准备回滚包…".into());
    let rollback = release_for_version(&current)?;
    if release::strip_version_prefix(&rollback.version) != current {
        return Err(format!(
            "回滚资产版本不匹配(需要 {current},release 为 {})",
            rollback.version
        ));
    }
    let rollback_installer = download_release_asset(report, &update, &rollback)?;

    let helper_source = install.join(WINDOWS_HELPER_EXE);
    if !helper_source.is_file() {
        return Err(format!("缺少升级 helper: {}", helper_source.display()));
    }
    let helper = update.join(format!(
        "walgit-upgrade-helper-{}-{}.exe",
        release.version,
        std::process::id()
    ));
    std::fs::copy(&helper_source, &helper).map_err(|e| format!("复制升级 helper 失败: {e}"))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("tray.log"))
        .map_err(|e| format!("打开升级日志失败: {e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("打开升级日志失败: {e}"))?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--new-installer")
        .arg(&new_installer)
        .arg("--rollback-installer")
        .arg(&rollback_installer)
        .arg("--new-sha256")
        .arg(&release.asset.sha256)
        .arg("--rollback-sha256")
        .arg(&rollback.asset.sha256)
        .arg("--target-version")
        .arg(&release.version)
        .arg("--rollback-version")
        .arg(&current)
        .arg("--install-dir")
        .arg(&install)
        .arg("--state-dir")
        .arg(state_dir())
        .arg("--update-dir")
        .arg(&update)
        .arg("--log")
        .arg(state_dir().join("tray.log"))
        .arg("--tray-pid")
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
        .map_err(|e| format!("启动升级 helper 失败: {e}"))?;
    log_line(&format!(
        "release: windows updater spawned for v{}",
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
#[cfg(any(target_os = "macos", target_os = "windows"))]
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

/// Is something *listening* on the configured address? That is **liveness** —
/// "the service is started" — and it is what the start/stop rows follow.
/// `/healthz` answering is a different fact (see [`healthz`]): a started server
/// that is still warming up, or one that is wedged, must not be reported as
/// stopped, and nothing about the verbs may depend on it.
fn port_open() -> bool {
    let (host, _, _) = deploy_config();
    let target = match host.strip_prefix("0.0.0.0:") {
        Some(rest) => format!("127.0.0.1:{rest}"),
        None => host,
    };
    TcpStream::connect(&target).is_ok()
}

/// One probe of both facts — the poller and every service action use this, so the
/// two are always read together and never inferred from each other.
fn status_probe() -> Msg {
    let body = healthz();
    Msg::Status {
        running: port_open(),
        healthy: body.is_some(),
        version: body.as_deref().map(version_of).unwrap_or_default(),
    }
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

// ---------- macOS Dock reopen 钩子(#200) ----------

/// 点 Dock 图标（或 `open -a`）→ 打开 Web UI。
///
/// macOS 把这件事投递给**应用 delegate** 的
/// `applicationShouldHandleReopen:hasVisibleWindows:`；winit 持有 delegate 且不实现也不
/// 转发它，而替换 winit 的 delegate 会破坏事件循环。所以启动时（主线程、事件循环开始
/// 前）往 **winit 的 delegate 类**上注入这一个方法：
///
/// * 注入前先 `class_getInstanceMethod` 检查：已存在（例如未来的 winit 实现了它）就跳过，
///   **绝不覆盖**别人的实现；
/// * 注入的选择子 winit 未实现，因此不改变 winit 的任何行为；
/// * 类型编码按当前目标的 `BOOL` 生成（arm64 是 `B`、x86_64 是 `c` —— 硬编码任何一个都会
///   在另一个架构上伪造方法元数据）；
/// * 任何一步失败只记日志，不 panic：托盘主体功能不依赖它。
#[cfg(target_os = "macos")]
fn install_dock_reopen_hook() {
    use objc2::encode::Encode;
    use objc2::runtime::{AnyObject, Bool, Sel};
    use objc2::sel;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    /// The delegate callback: open the Web UI when the Dock icon (or `open -a`)
    /// asks the app to come back. `false` = "I handled it, there is no window to
    /// show" (this app has no windows of its own).
    unsafe extern "C" fn should_handle_reopen(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
        _has_visible_windows: Bool,
    ) -> Bool {
        log_line("dock: reopen — opening the Web UI");
        open_web();
        Bool::NO
    }

    let Some(mtm) = MainThreadMarker::new() else {
        log_line("dock: reopen hook skipped (not on the main thread)");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: main thread (MainThreadMarker above) and the app exists.
    let Some(delegate) = (unsafe { app.delegate() }) else {
        log_line("dock: reopen hook skipped (no NSApplication delegate)");
        return;
    };
    let delegate_ptr: *const AnyObject = std::ptr::from_ref(&*delegate).cast();
    let class_ptr = unsafe { objc2::ffi::object_getClass(delegate_ptr.cast()) };
    if class_ptr.is_null() {
        log_line("dock: reopen hook skipped (delegate has no class)");
        return;
    }
    let selector = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    if !unsafe { objc2::ffi::class_getInstanceMethod(class_ptr, selector.as_ptr()) }.is_null() {
        log_line("dock: reopen hook skipped (the delegate already implements it)");
        return;
    }
    // `BOOL` is `B` on aarch64 and `c` on x86_64: take it from the type itself
    // instead of hardcoding either.
    let bool_encoding = Bool::ENCODING.to_string();
    let Ok(types) = std::ffi::CString::new(format!("{bool_encoding}@:@{bool_encoding}")) else {
        log_line("dock: reopen hook skipped (bad type encoding)");
        return;
    };
    // SAFETY: the selector is absent (checked above), so this cannot shadow an
    // existing implementation; the signature matches the documented AppKit
    // method for this target's BOOL; and this runs on the main thread before the
    // event loop starts dispatching.
    let added = unsafe {
        let imp: objc2::ffi::IMP = Some(std::mem::transmute::<
            unsafe extern "C" fn(&AnyObject, Sel, *mut AnyObject, Bool) -> Bool,
            unsafe extern "C" fn(),
        >(should_handle_reopen));
        objc2::ffi::class_addMethod(class_ptr.cast_mut(), selector.as_ptr(), imp, types.as_ptr())
    };
    log_line(&format!(
        "dock: reopen hook installed={added} on the winit delegate class"
    ));

    // `WALGIT_REOPEN_SELFTEST=1`: prove the injected method is callable without a
    // window server (CI has no Dock to click). This invokes exactly the IMP
    // AppKit calls on a Dock click, then leaves before the event loop.
    if std::env::var("WALGIT_REOPEN_SELFTEST").as_deref() != Ok("1") {
        return;
    }
    // `ffi::BOOL` is `bool` on arm64 but `i8` on x86_64: go through `Bool`
    // instead of a bare `!` (which does not compile on the latter).
    if Bool::from_raw(added).is_false() {
        log_line("dock: selftest skipped (the method was not added)");
        return;
    }
    // SAFETY: the selector exists with this exact signature (we just added it,
    // verified above); no other thread is messaging the delegate yet.
    let raw: unsafe extern "C" fn() = objc2::ffi::objc_msgSend;
    let send: unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject, Bool) -> Bool =
        unsafe { std::mem::transmute(raw) };
    let handled = unsafe {
        send(
            delegate_ptr,
            selector,
            std::ptr::from_ref(&*app).cast(),
            Bool::NO,
        )
    };
    println!("reopen-selftest handled={}", handled.as_bool());
    std::process::exit(0);
}

// ---------- 服务控制 ----------

/// macOS **and Windows** use the in-binary service command (D48): on Windows the
/// process belongs to the Task Scheduler, so the tray is a client. Linux keeps the
/// tray-side supervisor — `walgit serve` exits 75 after the setup wizard saves and
/// needs an immediate respawn, and there is no desktop scheduler to hand it to.
/// Either way: no walgit-ensure shell, and no binary copy under ~/.walgit.
/// 服务生命周期统一走二进制里的 `walgit service`（macOS 与 Windows，D48）：进程
/// 不属于托盘，托盘只是客户端 —— 起停、pidfile/任务的记账都归二进制一处。
/// Windows 上还必须带 CREATE_NO_WINDOW（`run` 已带），否则每点一次菜单闪一次黑窗。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn service_cmd(verb: &str) -> Result<(), String> {
    let bin = walgit_binary();
    if !bin.is_file() {
        return Err(format!("missing walgit binary: {}", bin.display()));
    }
    let cfg = state_dir().join("walgit.toml");
    let bin_s = bin.display().to_string();
    let cfg_s = cfg.display().to_string();
    let (code, out) = run(None, &bin_s, &["service", verb, "--config", &cfg_s], &[]);
    if code == 0 {
        Ok(())
    } else {
        Err(out)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn service_start() -> Result<(), String> {
    service_cmd("start")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn service_stop() -> Result<(), String> {
    service_cmd("stop")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn pid_file() -> PathBuf {
    state_dir().join("walgit.pid")
}

#[cfg(all(unix, not(target_os = "macos")))]
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
#[cfg(all(unix, not(target_os = "macos")))]
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
#[cfg(all(unix, not(target_os = "macos")))]
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

/// Linux only: the tray is the supervisor there (no scheduler to hand the
/// process to), so it owns the pidfile and stops the child it forked. On Windows
/// `walgit service stop` ends the scheduled task instead — see D48.
#[cfg(all(unix, not(target_os = "macos")))]
fn service_stop() -> Result<(), String> {
    let pid: u32 = std::fs::read_to_string(pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| "no pidfile".to_string())?;
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

/// 升级入口:选择了 Release 走平台安装器管线,否则走源码管线。
fn run_upgrade(report: &dyn Fn(String), release: Option<&ReleaseInfo>) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(release) = release {
            return release_upgrade(report, release);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(release) = release {
            return release_upgrade_windows(report, release);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
    // Test seam (#200): with `WALGIT_OPEN_URL_FILE` set, record the URL instead
    // of launching a browser. The reopen regression asserts *this* — a log line
    // alone would stay green if the action were dropped — and CI never pops a
    // browser window.
    if let Ok(path) = std::env::var("WALGIT_OPEN_URL_FILE") {
        if !path.is_empty() {
            let _ = std::fs::write(path, format!("{url}\n"));
            return;
        }
    }
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
    /// **Two independent facts**: is the service started (`running`), and does it
    /// answer `/healthz` (`healthy`). The menu's verbs follow `running`; `healthy`
    /// is a diagnostic. Treating one as the other is what made a healthy-looking
    /// tray show the wrong verb.
    Status {
        running: bool,
        healthy: bool,
        version: String,
    },
    Note(String),
    /// Last service-action failure (empty clears it). A failed 启动 must not look
    /// like a no-op, which is exactly what "exit 0, nothing happens" looked like.
    ServiceNote(String),
    Busy(u8),
    UpgradeFinished { ok: bool },
    Detected { generation: u64, detected: Detected },
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
    /// Started or not: the task is `Running` / the process is alive. What the
    /// 启动服务 /停止服务 rows do is fixed; this only drives the status line.
    running: bool,
    /// `/healthz` answered: a diagnostic shown next to `running`, never a verb.
    healthy: bool,
    /// Last service-action failure, shown in the status line until the next one
    /// succeeds (a failed start must not look like a no-op).
    service_note: String,
    busy: u8,                // 0 idle, 1 service action in flight, 2 upgrading
    state: u8,               // 升级状态机(ST_*)
    version: String,         // 托盘/App 版本(#183:升级判断的唯一依据)
    service_version: String, // healthz 报的服务进程版本,只作展示
    available_sha: String,   // 源码升级目标
    release: Option<ReleaseInfo>,
    note: String,
    detect_epoch: DetectEpoch,
    next_auto_detect: Option<Instant>,
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

    /// 启动一次检测。generation 在事件循环线程分配,结果回来时只认最新一代。
    fn start_detection(&mut self, proxy: &Arc<EventLoopProxy<Msg>>, show_checking: bool) {
        let generation = self.detect_epoch.start();
        if show_checking {
            self.state = ST_CHECKING;
            self.rebuild_menu();
        }
        let proxy = Arc::clone(proxy);
        std::thread::spawn(move || {
            let detected = detected_update();
            let _ = proxy.send_event(Msg::Detected {
                generation,
                detected,
            });
        });
    }

    /// 事件循环驱动的 30 分钟自动检测。相比独立线程投递 tick,这里能在
    /// 升级/失败状态上直接拒绝,不存在排队 tick 穿透终止态的问题。
    fn maybe_auto_detect(&mut self) {
        if !auto_detect_allowed(self.busy, self.state) {
            return;
        }
        let Some(next) = self.next_auto_detect else {
            return;
        };
        let now = Instant::now();
        if now < next {
            return;
        }
        self.next_auto_detect = Some(now + Duration::from_secs(1800));
        if let Some(proxy) = self.proxy.clone() {
            self.start_detection(&proxy, false);
        }
    }

    /// 一次性应用检测结果:菜单状态与对应的 payload 必须同代写入。
    fn apply_detected(&mut self, detected: Detected) {
        match detected {
            Detected::Nothing => {
                self.release = None;
                self.available_sha.clear();
                self.state = ST_LATEST;
            }
            Detected::Source(sha) => {
                self.release = None;
                self.available_sha = sha;
                self.state = ST_AVAILABLE;
            }
            Detected::Failed => {
                self.release = None;
                self.available_sha.clear();
                self.state = ST_CHECK_FAILED;
            }
            Detected::Release(info) => {
                self.available_sha.clear();
                self.release = Some(info);
                self.state = ST_AVAILABLE;
            }
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
        // **运行态与健康分开说**: the verbs below follow `running` (is it
        // started), while `/healthz` answering is a diagnostic printed here — a
        // started-but-unresponsive service is 运行中 · 无响应, not 已停止.
        let state = if running && self.healthy {
            format!("运行中 · 健康 {}", self.current_version())
        } else if running {
            "运行中 · /healthz 无响应".to_string()
        } else {
            "已停止".to_string()
        };
        let note = if self.service_note.is_empty() {
            String::new()
        } else {
            format!(" · {}", self.service_note)
        };
        h.status
            .set_text(format!("walgit 服务:{state}{backend_note}{note}"));
        // One row, labelled with the **next action**. The verb is decided from live
        // liveness at click time (see the handler), never from this cached label
        // and never from the item's id, so label and action always agree and the
        // row cannot offer a no-op: a running service reads 停止服务 and stops, a
        // stopped one reads 启动服务 and starts. No 「切换中…」 text either — the row
        // keeps saying what it will do while a call is in flight.
        h.toggle.set_text(if running { "停止服务" } else { "启动服务" });
        h.toggle.set_enabled(self.busy != 2);
        // 升级通道:macOS/Windows 装好的 Release 不需要源码仓库;
        // 开发机与 Linux 走源码仓库(#73:都没有时菜单禁点并指路)。
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
            Msg::Status {
                running,
                healthy,
                version,
            } => {
                self.running = running;
                self.healthy = healthy;
                if healthy {
                    self.service_version = version;
                }
            }
            Msg::Note(n) => self.note = n,
            Msg::ServiceNote(n) => self.service_note = n,
            Msg::Busy(b) => self.busy = b,
            // 升级结束必须是一次原子状态转换:不能先把 busy 清掉、等下一
            // 条消息才落 ST_FAILED,否则在途检测会从窗口里穿过去。
            Msg::UpgradeFinished { ok } => {
                self.busy = 0;
                self.note.clear();
                self.state = if ok { ST_LATEST } else { ST_FAILED };
            }
            Msg::Detected {
                generation,
                detected,
            } => {
                if self.detect_epoch.accepts(generation, self.busy) {
                    self.apply_detected(detected);
                }
            }
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
                "toggle" => {
                    if self.busy == 0 {
                        // Read the state **now**, not the label's memory of it — that
                        // is the whole fix. The old code took the verb from the
                        // item's id, which was hardcoded `"stop"`, so every click ran
                        // `service stop`, including the clicks on 「启动服务」.
                        self.busy = 1;
                        self.rebuild_menu();
                        std::thread::spawn(move || {
                            let verb = if port_open() { "stop" } else { "start" };
                            let r = if verb == "start" {
                                service_start()
                            } else {
                                service_stop()
                            };
                            log_line(&format!("{verb} -> {r:?}"));
                            let _ = proxy.send_event(Msg::ServiceNote(match &r {
                                Ok(()) => String::new(),
                                Err(e) => format!("{verb} 失败: {e}"),
                            }));
                            let _ = proxy.send_event(Msg::Busy(0));
                            let _ = proxy.send_event(status_probe());
                        });
                    }
                }
                "upgrade" => match self.state {
                    ST_IDLE | ST_LATEST | ST_FAILED | ST_CHECK_FAILED => {
                        self.start_detection(&proxy, true);
                    }
                    ST_AVAILABLE => {
                        // 用户点击升级:这里才真正跑升级管线。先作废升级前
                        // 启动的检测,防止它在新状态落定后回灌。
                        self.detect_epoch.invalidate();
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
                            // Release 交棒成功:辅助进程会替换/安装新版本并等待
                            // 本 pid 退出;这里直接退场,不再做状态回灌。
                            if r.is_ok() && release.is_some() {
                                log_line(&format!("upgrade -> {r:?}"));
                                std::process::exit(0);
                            }
                            let _ = proxy.send_event(Msg::UpgradeFinished { ok: r.is_ok() });
                            let _ = proxy.send_event(status_probe());
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
        self.maybe_auto_detect();
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

    // 检测一次就退出(WALGIT_DETECT_ONCE=1):CI 没有可靠窗口服务器,但
    // “菜单说升级到哪个版本”是 Release 通道最该被守住的语义。与 fixture
    // (WALGIT_RELEASE_FIXTURE)配合即可离线断言;Windows 无控制台，可把
    // 菜单行同时写到 WALGIT_DETECT_ONCE_FILE。
    if std::env::var("WALGIT_DETECT_ONCE").as_deref() == Ok("1") {
        let detected = detected_update();
        let (state, release, sha) = match detected {
            Detected::Nothing => (ST_LATEST, None, String::new()),
            Detected::Source(sha) => (ST_AVAILABLE, None, sha),
            Detected::Failed => (ST_CHECK_FAILED, None, String::new()),
            Detected::Release(info) => (ST_AVAILABLE, Some(info), String::new()),
        };
        let line = upgrade_line(
            state,
            &app_version(),
            &service_version(),
            release.as_ref(),
            &sha,
            "",
        );
        println!("{line}");
        if let Ok(path) = std::env::var("WALGIT_DETECT_ONCE_FILE") {
            if !path.is_empty() {
                let _ = std::fs::write(path, format!("{line}\n"));
            }
        }
        return;
    }

    // 安装器自启标记(Windows 安装器勾选「开机自动启动」时写):自启的托盘
    // 把服务一并拉起——勾选框承诺的是「部署开机可用」,不是只把托盘拉起来。
    // 仅在服务未运行时尝试一次;失败不重试,留给菜单「启动服务」。
    if state_dir().join("service.autostart").exists() && healthz().is_none() {
        log_line("autostart marker: starting service");
        let _ = service_start();
    }
    // macOS: a *regular* app (Dock icon + Cmd+Tab), not an accessory one. The
    // status item can be occluded — a full menu bar, the notch, a full-screen
    // app — and an accessory app has no other entry point at all (#197).
    // The bundle's `LSUIElement=false` is what LaunchServices reads; setting the
    // policy here keeps a bare `cargo run` (no bundle) equally reachable.
    let mut loop_builder = EventLoop::<Msg>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        loop_builder.with_activation_policy(ActivationPolicy::Regular);
    }
    let event_loop = loop_builder.build().unwrap();
    let proxy = Arc::new(event_loop.create_proxy());

    let icon = tray_icon::Icon::from_rgba(icon_rgba(32, state_color(false, 0)), 32, 32)
        .expect("icon rgba");
    let menu = Menu::new();
    let status = MenuItem::with_id("status", "walgit 服务:检查中…", false, None);
    // One row; `rebuild_menu` rewrites its text from the live state, and the id is
    // deliberately verb-less so nothing can pick a verb out of it again.
    let toggle = MenuItem::with_id("toggle", "启动服务", true, None);
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

    let detect_enabled = release_channel() || has_source_repo();
    if !detect_enabled {
        log_line(&format!(
            "detect: no repo at {} — update checks disabled (set WALGIT_REPO to enable)",
            repo_dir().display()
        ));
    }
    let mut app = App {
        tray: Some(tray),
        items: Some(MenuHandles {
            status,
            toggle,
            upgrade,
            quit,
        }),
        proxy: Some(proxy.clone()),
        running: port_open(),
        healthy: healthz().is_some(),
        service_note: String::new(),
        busy: 0,
        state: ST_IDLE,
        version: app_version(),
        service_version: service_version(),
        available_sha: String::new(),
        release: None,
        note: String::new(),
        detect_epoch: DetectEpoch::default(),
        next_auto_detect: detect_enabled.then(|| Instant::now() + Duration::from_secs(30)),
    };
    app.rebuild_menu();

    // 状态轮询线程(5s):运行态(端口) + 健康(/healthz) + 服务版本
    let poll_proxy = proxy;
    std::thread::spawn(move || loop {
        let _ = poll_proxy.send_event(status_probe());
        std::thread::sleep(Duration::from_secs(5));
    });

    #[cfg(target_os = "macos")]
    install_dock_reopen_hook();

    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_release_channel_uses_installer_directories_only() {
        let current = std::path::Path::new(r"C:\Users\me\AppData\Local\Programs\walgit");
        assert!(windows_install_dir_matches(
            current,
            Some(r"C:\Users\me\AppData\Local"),
            Some(r"C:\Users\me")
        ));
        let legacy = std::path::Path::new(r"C:\Users\me\walgit");
        assert!(windows_install_dir_matches(
            legacy,
            Some(r"C:\Users\me\AppData\Local"),
            Some(r"C:\Users\me")
        ));
        let source = std::path::Path::new(r"C:\src\walgit\target\release");
        assert!(!windows_install_dir_matches(
            source,
            Some(r"C:\Users\me\AppData\Local"),
            Some(r"C:\Users\me")
        ));
    }

    #[test]
    fn stale_detection_cannot_overwrite_newer_result() {
        let mut epoch = DetectEpoch::default();
        let automatic = epoch.start();
        let retry = epoch.start();
        let mut state = ST_CHECKING;

        // 点击重试更新一代,先回来并写入结果……
        if epoch.accepts(retry, 0) {
            state = ST_LATEST;
        }
        // ……更慢的自动检测旧结果后到,不能把菜单覆盖回旧状态。
        if epoch.accepts(automatic, 0) {
            state = ST_AVAILABLE;
        }
        assert_eq!(state, ST_LATEST);
    }

    #[test]
    fn detection_result_is_ignored_during_upgrade() {
        let mut epoch = DetectEpoch::default();
        let generation = epoch.start();
        assert!(!epoch.accepts(generation, 2));
    }

    #[test]
    fn upgrade_failure_is_not_overwritten_by_pre_upgrade_detection() {
        let mut epoch = DetectEpoch::default();
        let in_flight = epoch.start();

        // 进入升级:在途检测作废。
        epoch.invalidate();
        assert!(!epoch.accepts(in_flight, 2));

        // 旧结果在升级终止后到达,仍不能落到已经恢复的空闲状态上。
        let busy = 0;
        let mut state = ST_FAILED;
        if epoch.accepts(in_flight, busy) {
            state = ST_LATEST;
        }
        assert_eq!(state, ST_FAILED);

        // 终止态也不会被自动 tick 立刻重新开启检测。
        assert!(!auto_detect_allowed(busy, state));
    }
}
