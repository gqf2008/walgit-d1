//! 一次安装一个程序文件，`PATH` 上是指向它的入口（D65，全平台一致）。
//!
//! 托盘每次启动都跑这里：`walgit` 必须在**用户 shell 已经有的** `PATH` 目录里
//! 可解析，否则 agent 找不到命令——它的第一条指令永远是 `walgit …`。所以
//! 这里不写死一个位置，而是按「在 PATH 上 && 可写」挑目录：
//!
//! - macOS / Linux：在候选目录里建（或修复）指向安装目录程序的软链；
//! - Windows：没有软链车道，等价动作是把安装目录写进**用户** `PATH`
//!   （`HKCU\Environment`），与安装器 `user-path.iss` 同一判据、幂等。
//!
//! 为什么不是副本：副本是第二个程序文件，升级永远碰不到它——野外实测过一个
//! 手抄的 v0.5 二进制在服务已经是 v0.11 之后继续回应已删除的 `walgit mcp`。
//! 因此「占位的副本」会被改名挪开（不删，那是用户的文件）再换成软链。
//!
//! 为什么不动 shell 配置：本仓库任何地方都不改 `~/.zshrc`/`~/.profile`
//! （`grep -r zshenv` 为空是设计，不是遗漏）。找不到“在 PATH 上且可写”的目录
//! 时，只报告该导出的那一行，不替用户改启动脚本。

use std::path::{Path, PathBuf};

/// 一个入口最终变成了什么。三个平台的「建入口」共用一个词表，托盘日志与
/// `cli-entry-healed.json` 都按它说话。
#[derive(Debug, PartialEq, Eq)]
pub enum LinkOutcome {
    /// 指向安装目录程序的软链：新建、更新、或本来就好。
    Linked,
    /// 被一个 **walgit 副本**占着。已改名挪开（不删）并换成软链。
    Healed {
        moved_to: PathBuf,
        version: Option<String>,
    },
    /// 不是 walgit 的东西（别人的脚本、别的工具、不是我们建的软链）：不碰。
    Foreign,
    /// 建不出来（权限）。
    NotCreatable(String),
    /// Windows：用户 `PATH` 里补上了安装目录。
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "the Windows lane's outcome; unix creates a symlink")
    )]
    PathAdded,
}

/// 一次 `ensure` 的结果。
#[derive(Debug)]
pub struct Ensured {
    /// 被处理的入口（unix 是软链路径，windows 是安装目录）。
    pub entry: Option<PathBuf>,
    pub outcome: LinkOutcome,
    /// 这个入口所在目录本来就在用户的 `PATH` 上。false 表示「入口建好了，
    /// 但新开的 shell 仍解析不到」——那需要用户自己加一行 `PATH`。
    pub on_path: bool,
    /// 入口是 `WALGIT_CLI_LINK` 显式指定的：失败必须响亮（打包守卫依赖这句）。
    pub explicit: bool,
}

impl Ensured {
    pub fn missing(reason: &str) -> Self {
        Self {
            entry: None,
            outcome: LinkOutcome::NotCreatable(reason.to_string()),
            on_path: false,
            explicit: false,
        }
    }
}

/// 程序安装目录里的可执行文件。macOS 是 App Bundle 的
/// `Contents/Resources/walgit`，其它平台是托盘旁边的那一个。
pub fn program_in(dir: &Path) -> PathBuf {
    dir.join(exe_name())
}

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "walgit.exe"
    } else {
        "walgit"
    }
}

/// `PATH` 里的目录：进程自己的 + 登录 shell 的（unix）+ 用户变量
/// （windows）。托盘由 launchd / 资源管理器拉起，自带的 `PATH` 常常比终端
/// 里的窄很多；只信自己的会把「终端里有、托盘看不到」误判成没有。
pub fn path_dirs() -> Vec<PathBuf> {
    let mut dirs = split_path(&std::env::var("PATH").unwrap_or_default());
    #[cfg(unix)]
    if let Some(shell_path) = login_shell_path() {
        for dir in split_path(&shell_path) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    #[cfg(windows)]
    if let Some(user_path) = windows::read_user_path() {
        for dir in split_path(&user_path) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// 按平台的 `PATH` 分隔符切分，丢掉空项并展开 `~`。
pub fn split_path(value: &str) -> Vec<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    value
        .split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// 登录 shell 眼里的 `PATH`（`sh -lc`，和安装脚本判据一致）。
#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let out = std::process::Command::new(shell)
        .arg("-lc")
        .arg("printf %s \"$PATH\"")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// `dir` 是否在这个 `PATH` 目录表里。两个方向都做了 canonicalize（软链、
/// 尾斜杠、大小写不敏感的文件系统），退化为字面比较。
pub fn dir_on_path(dir: &Path, dirs: &[PathBuf]) -> bool {
    let want = canonical(dir);
    dirs.iter().any(|d| canonical(d) == want)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 候选入口，按「最可能已经在 `PATH` 上」的顺序：
///
/// - macOS：`/opt/homebrew/bin`（Apple Silicon 默认 PATH）、`/usr/local/bin`
///   （Intel 默认 PATH），最后才是用户目录。
/// - Linux：`~/.local/bin`（不需要 root），再 `/usr/local/bin`、`/usr/bin`
///   （deb 就装在那里；对同一路径的重复处理是幂等的）。
pub fn candidate_entries() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        // 先自己的目录（不打扰 Homebrew），再两个在默认 PATH 上的系统目录：
        // 新机器上 `~/.local/bin` 往往不在 PATH 里，那时才落到 Homebrew。
        out.push(home().join(".local/bin").join(exe_name()));
        out.push(PathBuf::from("/opt/homebrew/bin").join(exe_name()));
        out.push(PathBuf::from("/usr/local/bin").join(exe_name()));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        out.push(home().join(".local/bin").join(exe_name()));
        out.push(PathBuf::from("/usr/local/bin").join(exe_name()));
        out.push(PathBuf::from("/usr/bin").join(exe_name()));
    }
    #[cfg(unix)]
    out.push(home().join(".local/bin").join(exe_name()));
    out.dedup();
    out
}

/// 打包守卫注入的候选（`WALGIT_CLI_LINK_PRIMARY` / `_FALLBACK`），按序。
/// 显式注入时不再看 `PATH`，「是否在 PATH 上」只作为报告值。
pub fn explicit_entries() -> Option<Vec<PathBuf>> {
    let vars = ["WALGIT_CLI_LINK_PRIMARY", "WALGIT_CLI_LINK_FALLBACK"];
    let mut out: Vec<PathBuf> = Vec::new();
    for var in vars {
        if let Some(value) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
            let path = PathBuf::from(value);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(unix)]
fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// 候选排序：先「在 `PATH` 上且可写」，再其余可写的（`PATH` 上不可写的跳过）。
/// `ensure` 按这个顺序逐个尝试——一个候选建不出来（权限、父路径不是目录）
/// 不代表下一个也不行，旧实现正是在这里丢掉了回退。
pub fn order_candidates(
    candidates: &[PathBuf],
    path: &[PathBuf],
    writable: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut on_path: Vec<PathBuf> = Vec::new();
    let mut off_path: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let Some(dir) = candidate.parent() else {
            continue;
        };
        if !writable(dir) {
            // A path whose parent is not a directory still deserves an attempt:
            // the failure is what tells the packaging guard the injection worked.
            off_path.push(candidate.clone());
            continue;
        }
        if dir_on_path(dir, path) {
            on_path.push(candidate.clone());
        } else {
            off_path.push(candidate.clone());
        }
    }
    on_path.extend(off_path);
    on_path
}

/// 目录可写（存在且能建文件；不存在时看父目录）。
fn dir_writable(dir: &Path) -> bool {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => std::fs::metadata(dir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
            && probe_writable(dir),
        _ => dir
            .parent()
            .and_then(|parent| std::fs::metadata(parent).ok())
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false),
    }
}

#[cfg(unix)]
fn probe_writable(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok() && {
        let probe = dir.join(format!(".walgit-write-probe-{}", std::process::id()));
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }
}

#[cfg(windows)]
fn probe_writable(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok()
}

/// 程序目录里的 `walgit` 装好入口，返回它被放在哪。
///
/// 重入安全：每次托盘启动都跑，已经正确时只做一次 `readlink`/注册表读取。
pub fn ensure(program_dir: &Path) -> Ensured {
    let program = program_in(program_dir);
    if !program.is_file() {
        return Ensured::missing("安装目录里没有 walgit 程序");
    }
    #[cfg(unix)]
    {
        let path = path_dirs();
        let override_entry = std::env::var("WALGIT_CLI_LINK")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let explicit_candidates = explicit_entries();
        // `WALGIT_CLI_LINK_PRIMARY`/`_FALLBACK` are the packaging guard's
        // injection point (deploy/tray/macos/test.sh): they replace the
        // candidate list so a runner's own Homebrew directory cannot decide what
        // the fixture tests.
        let candidates = match &override_entry {
            Some(entry) => vec![entry.clone()],
            None => explicit_candidates
                .clone()
                .unwrap_or_else(candidate_entries),
        };
        if candidates.is_empty() {
            return Ensured::missing("没有可写且可用的候选入口目录");
        }
        let ordered = order_candidates(&candidates, &path, &dir_writable);
        let mut failure: Option<(PathBuf, String)> = None;
        for entry in ordered {
            // `deb` 把程序本身装在 `/usr/bin/walgit`：那个路径是入口候选，也是
            // 程序。对它做 heal 会把程序挪开再把软链指向自己。
            if canonical(&entry) == canonical(&program) {
                let on_path = entry.parent().is_some_and(|dir| dir_on_path(dir, &path));
                return Ensured {
                    entry: Some(entry),
                    outcome: LinkOutcome::Linked,
                    on_path,
                    explicit: override_entry.is_some(),
                };
            }
            let on_path = entry.parent().is_some_and(|dir| dir_on_path(dir, &path));
            match install_one_cli_link(&entry, &program) {
                LinkOutcome::NotCreatable(e) => failure = Some((entry, e)),
                // A candidate that is somebody else's file says nothing about the
                // next one, but we never touch it — stop here rather than write
                // to a second location the user did not ask for.
                outcome => {
                    return Ensured {
                        entry: Some(entry),
                        outcome,
                        on_path,
                        explicit: override_entry.is_some(),
                    };
                }
            }
        }
        match failure {
            Some((entry, e)) => Ensured {
                entry: Some(entry),
                outcome: LinkOutcome::NotCreatable(e),
                on_path: false,
                explicit: override_entry.is_some(),
            },
            None => Ensured::missing("没有可写且可用的候选入口目录"),
        }
    }
    #[cfg(windows)]
    {
        let dir = std::fs::canonicalize(program_dir).unwrap_or_else(|_| program_dir.to_path_buf());
        let user_path = windows::read_user_path().unwrap_or_default();
        let on_path = dir_on_path(&dir, &split_path(&user_path));
        if on_path {
            return Ensured {
                entry: Some(dir),
                outcome: LinkOutcome::PathAdded,
                on_path: true,
                explicit: false,
            };
        }
        let outcome = windows::add_to_user_path(&dir);
        Ensured {
            entry: Some(dir),
            outcome,
            on_path: false,
            explicit: false,
        }
    }
}

#[cfg(unix)]
/// 版本字符串，从 `--version` 的第一行取（必须是 walgit）。
fn walgit_version_of(path: &Path) -> Option<String> {
    let size = std::fs::metadata(path).ok()?.len();
    if size < (1 << 20) {
        return None;
    }
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or_default().trim().to_string();
    first.contains("walgit").then_some(first)
}

#[cfg(unix)]
fn symlink_to(link: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| format!("symlink {}: {e}", link.display()))?;
    Ok(())
}

#[cfg(unix)]
/// 装（或修）一个入口：永远是指向程序的软链，不是副本。
pub fn install_one_cli_link(link: &Path, target: &Path) -> LinkOutcome {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link).ok().as_deref() == Some(target) {
                return LinkOutcome::Linked;
            }
            if let Err(e) = std::fs::remove_file(link).map_err(|e| e.to_string()) {
                return LinkOutcome::NotCreatable(e);
            }
            match symlink_to(link, target) {
                Ok(()) => LinkOutcome::Linked,
                Err(e) => LinkOutcome::NotCreatable(e),
            }
        }
        Ok(_) => {
            // 普通文件。是 walgit 就是**副本**，副本会漂：改名挪开（不删）再
            // 换成软链。不是 walgit 就一个字节都不碰。
            let Some(version) = walgit_version_of(link) else {
                return LinkOutcome::Foreign;
            };
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut moved_to = link.with_extension(format!("copy-{stamp}"));
            if moved_to.exists() {
                moved_to = link.with_extension(format!("copy-{stamp}-1"));
            }
            if let Err(e) = std::fs::rename(link, &moved_to).map_err(|e| e.to_string()) {
                return LinkOutcome::NotCreatable(e);
            }
            if let Err(e) = symlink_to(link, target) {
                let _ = std::fs::rename(&moved_to, link);
                return LinkOutcome::NotCreatable(e);
            }
            LinkOutcome::Healed {
                moved_to,
                version: Some(version),
            }
        }
        Err(_) => match symlink_to(link, target) {
            Ok(()) => LinkOutcome::Linked,
            Err(e) => LinkOutcome::NotCreatable(e),
        },
    }
}

#[cfg(windows)]
mod windows {
    use std::path::Path;

    use super::LinkOutcome;

    /// 读用户 `PATH`（`HKCU\Environment`）。`reg query` 原样返回
    /// `REG_EXPAND_SZ`，不做展开——展开一次 `%USERPROFILE%` 再写回去会把
    /// 变量固化成当时的路径。
    pub fn read_user_path() -> Option<String> {
        let out = std::process::Command::new("reg")
            .args(["query", r"HKCU\Environment", "/v", "Path"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        text.lines()
            .find(|line| line.contains("Path"))
            .and_then(|line| line.split_once("REG_"))
            .and_then(|(_, rest)| rest.split_once(char::is_whitespace))
            .map(|(_, value)| value.trim().to_string())
    }

    /// 把安装目录追加进用户 `PATH`（幂等）。写入用 `REG_EXPAND_SZ`，与安装器
    /// `user-path.iss` 同一个类型；失败只报，不让托盘启动失败。
    pub fn add_to_user_path(dir: &Path) -> LinkOutcome {
        let current = read_user_path().unwrap_or_default();
        let Some(updated) = super::appended_path(&current, &dir.display().to_string()) else {
            return LinkOutcome::PathAdded;
        };
        let out = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Environment",
                "/v",
                "Path",
                "/t",
                "REG_EXPAND_SZ",
                "/d",
                &updated,
                "/f",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => LinkOutcome::PathAdded,
            Ok(o) => LinkOutcome::NotCreatable(
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ),
            Err(e) => LinkOutcome::NotCreatable(e.to_string()),
        }
    }
}

/// `PATH` 字面量加上一个目录；已经在里面（忽略大小写、尾斜杠）返回 `None`。
/// 空串 ⇒ 只有这一个目录，不留前导分隔符。
#[cfg(any(windows, test))]
pub fn appended_path(current: &str, dir: &str) -> Option<String> {
    let want = normalise_entry(dir);
    let present = split_path(current)
        .iter()
        .any(|d| normalise_entry(&d.display().to_string()) == want);
    if present {
        return None;
    }
    let trimmed = current.trim();
    if trimmed.is_empty() {
        Some(dir.to_string())
    } else {
        Some(format!("{trimmed}{}{dir}", if cfg!(windows) { ';' } else { ':' }))
    }
}

#[cfg(any(windows, test))]
fn normalise_entry(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按本平台的分隔符拼 `PATH`（测试不能写死 `:`）。
    fn join_path(items: &[&str]) -> String {
        items.join(if cfg!(windows) { ";" } else { ":" })
    }

    #[test]
    fn a_path_dir_is_matched_through_trailing_slashes() {
        let dirs = split_path(&join_path(&["/opt/homebrew/bin", "/usr/local/bin/", "/usr/bin"]));
        assert!(dir_on_path(Path::new("/usr/local/bin"), &dirs));
        assert!(dir_on_path(Path::new("/opt/homebrew/bin/"), &dirs));
        assert!(!dir_on_path(Path::new("/home/u/.local/bin"), &dirs));
    }

    #[test]
    fn the_first_candidate_that_is_on_the_path_and_writable_wins() {
        let candidates = vec![
            PathBuf::from("/opt/homebrew/bin/walgit"),
            PathBuf::from("/usr/local/bin/walgit"),
            PathBuf::from("/home/u/.local/bin/walgit"),
        ];
        let path = split_path(&join_path(&["/usr/bin", "/bin", "/home/u/.local/bin"]));
        // /opt/homebrew 不在 PATH 上；/usr/local 不在；~/.local/bin 在。
        let ordered = order_candidates(&candidates, &path, &|_| true);
        // 只有 `~/.local/bin` 在 PATH 上 → 它排第一，其余保持候选顺序。
        assert_eq!(ordered[0], PathBuf::from("/home/u/.local/bin/walgit"));
        assert_eq!(ordered[1], PathBuf::from("/opt/homebrew/bin/walgit"));
        assert!(dir_on_path(ordered[0].parent().unwrap(), &path));
    }

    /// 不可写的候选排到后面而不是被丢掉：一次建不出来（权限、父路径不是目录）
    /// 要落到下一个候选去试——旧实现正是在这里丢掉了回退。
    #[test]
    fn an_unwritable_candidate_is_tried_after_a_writable_one() {
        let candidates = vec![
            PathBuf::from("/usr/local/bin/walgit"),
            PathBuf::from("/home/u/.local/bin/walgit"),
        ];
        let path = split_path(&join_path(&["/usr/local/bin", "/home/u/.local/bin"]));
        let ordered = order_candidates(&candidates, &path, &|dir| {
            dir != Path::new("/usr/local/bin")
        });
        assert_eq!(ordered[0], PathBuf::from("/home/u/.local/bin/walgit"));
        assert_eq!(ordered[1], PathBuf::from("/usr/local/bin/walgit"));
    }

    /// 一个都不在 PATH 上时仍然建入口（免得下次启动重复找），但要如实报
    /// `on_path = false`，让日志能说出「新开的 shell 还看不到」。
    #[test]
    fn nothing_on_the_path_still_yields_a_fallback_and_says_so() {
        let candidates = vec![PathBuf::from("/home/u/.local/bin/walgit")];
        let path = split_path(&join_path(&["/usr/bin", "/bin"]));
        let ordered = order_candidates(&candidates, &path, &|_| true);
        assert_eq!(ordered, candidates);
        assert!(!dir_on_path(ordered[0].parent().unwrap(), &path));
    }

    /// 全不可写时仍然把候选交给建链那一层：显式注入的入口必须响亮地失败，
    /// 而不是被静默丢掉。
    #[test]
    fn nothing_writable_still_reaches_the_install_step() {
        let candidates = vec![PathBuf::from("/usr/local/bin/walgit")];
        let path = split_path(&join_path(&["/usr/bin"]));
        let ordered = order_candidates(&candidates, &path, &|_| false);
        assert_eq!(ordered, candidates);
    }

    #[test]
    fn appending_a_dir_is_idempotent_and_never_double_separates() {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let _ = sep;
        assert_eq!(
            appended_path("", "/opt/x/bin").as_deref(),
            Some("/opt/x/bin")
        );
        assert_eq!(appended_path("/opt/x/bin", "/opt/x/bin/"), None);
        assert_eq!(appended_path(&join_path(&["/a", "/b"]), "/b"), None);
        let current = join_path(&["/a", "/b"]);
        assert_eq!(
            appended_path(&current, "/c").as_deref(),
            Some(&format!("/a{sep}/b{sep}/c")[..])
        );
    }
}
