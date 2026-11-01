//! macOS 首次启动 bootstrap:状态目录初始化 + 旧布局迁移 + CLI 软链。
//!
//! 程序二进制只属于 App Bundle;`~/.walgit` 只放用户状态(配置、cache、
//! keys、日志、凭证)。旧版(0.5.x/0.6.0)把程序与状态混在 `~/walgit`,首次
//! 启动要把用户状态**复制**(不移动,旧 app 回滚后仍完整)到新目录,并在旧
//! 目录留一个只活 5 分钟的升级兼容桥(walgit symlink + walgit-ensure 转发壳)。
//!
//! 来源:macOS Swift 托盘的 `bootstrapDeploy()`;issue #183 随托盘一起迁到
//! 跨平台 tray-rs。Windows/Linux 保持空实现(那边是安装器托管布局)。

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};

    use crate::cli_entry::LinkOutcome;
    use crate::{log_line, sh};

    /// 旧 helper 只在升级窗口里调用到的转发壳(5 分钟后自清):它把
    /// `walgit-ensure start|stop|…` 转成新二进制的 `walgit service`。
    const LEGACY_ENSURE_SHIM: &str = "#!/bin/sh\n\
        set -eu\n\
        BASE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\n\
        STATE=\"${WALGIT_STATE_DIR:-$HOME/.walgit}\"\n\
        case \"${1:-start}\" in\n\
          ensure|start|\"\") exec \"$BASE/walgit\" service start --config \"$STATE/walgit.toml\" ;;\n\
          stop|status|restart) exec \"$BASE/walgit\" service \"$1\" --config \"$STATE/walgit.toml\" ;;\n\
          *) exit 2 ;;\n\
        esac\n";

    /// 测试/多部署覆盖:WALGIT_STATE_DIR 优先,其次 WALGIT_DEPLOY_DIR(旧
    /// 升级 helper 用 `open --env WALGIT_DEPLOY_DIR=…` 把新 app 指回同一份
    /// 状态),否则 `~/.walgit`。
    pub fn state_dir() -> PathBuf {
        if let Some(dir) = env_path("WALGIT_STATE_DIR") {
            return dir;
        }
        if let Some(dir) = env_path("WALGIT_DEPLOY_DIR") {
            return dir;
        }
        crate::home().join(".walgit")
    }

    fn env_path(key: &str) -> Option<PathBuf> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    fn legacy_dir() -> PathBuf {
        env_path("WALGIT_LEGACY_DIR").unwrap_or_else(|| crate::home().join("walgit"))
    }

    /// App Bundle 里的可执行文件所在目录(`Contents/Resources`)。
    fn resource_dir() -> Option<PathBuf> {
        crate::app_bundle().map(|bundle| bundle.join("Contents/Resources"))
    }

    /// 旧配置里的 `[server].listen`(用于停掉没有 pidfile 的旧 screen 服务)。
    fn listen_addr(config: &Path) -> String {
        let Ok(text) = std::fs::read_to_string(config) else {
            return crate::DEFAULT_LISTEN.to_string();
        };
        for line in text.lines() {
            let flat = line.trim().split('#').next().unwrap_or("").trim();
            if let Some(value) = flat
                .strip_prefix("listen = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                return value.to_string();
            }
        }
        crate::DEFAULT_LISTEN.to_string()
    }

    fn health_ok(url: &str) -> bool {
        let (code, _) = sh(&format!("curl -sf --max-time 2 '{url}' >/dev/null 2>&1"));
        code == 0
    }

    /// 只删受管的普通文件/软链:用户自己在同名位置放的目录永不删除。
    fn remove_managed_file_if_safe(path: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.file_type().is_dir() {
            log_line(&format!(
                "bootstrap: 保留非普通文件,不删除 {}",
                path.display()
            ));
            return;
        }
        match std::fs::remove_file(path) {
            Ok(()) => log_line(&format!("bootstrap: 已移除旧托管文件 {}", path.display())),
            Err(e) => log_line(&format!(
                "bootstrap: 移除旧托管文件失败 {}: {e}",
                path.display()
            )),
        }
    }

    /// 复制用户状态到新目录(**不移动**):旧 app 回滚后仍要完整可用。
    /// cache 是设计上的可丢弃物,不复制。
    fn migrate_legacy_state(from: &Path, to: &Path) {
        if let Err(e) = std::fs::create_dir_all(to) {
            log_line(&format!("bootstrap: 建状态目录失败 {}: {e}", to.display()));
            return;
        }
        for name in [
            "walgit.toml",
            ".r2-credentials",
            ".walgit_token",
            "keys",
            ".events-seen",
        ] {
            let src = from.join(name);
            let dst = to.join(name);
            if !src.exists() {
                continue;
            }
            if dst.exists() {
                log_line(&format!("bootstrap: 迁移跳过已存在项 {name}"));
                continue;
            }
            let result = if src.is_dir() {
                copy_dir(&src, &dst)
            } else {
                std::fs::copy(&src, &dst).map(|_| ())
            };
            match result {
                Ok(()) => log_line(&format!("bootstrap: 已复制旧状态 {name}")),
                Err(e) => log_line(&format!("bootstrap: 迁移旧状态 {name} 失败: {e}")),
            }
        }
    }

    fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_dir(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }

    /// 停掉 0.5.x 的 screen/无 pidfile 服务:只 kill 映像名匹配的监听进程,
    /// 绝不按端口误杀别的服务。
    fn stop_legacy_service(listen: &str) {
        let port = listen.rsplit(':').next().unwrap_or("8081").to_string();
        let session =
            std::env::var("WALGIT_SCREEN_SESSION").unwrap_or_else(|_| "walgit-server".to_string());
        let _ = sh(&format!(
            "screen -S {session} -X quit >/dev/null 2>&1 || true"
        ));
        let _ = sh(&format!(
            "pid=$(lsof -tiTCP:{port} -sTCP:LISTEN 2>/dev/null | head -1 || true); \
             if [ -n \"$pid\" ]; then \
               comm=$(ps -p \"$pid\" -o comm= 2>/dev/null || true); \
               case \"$comm\" in *walgit*) kill \"$pid\" 2>/dev/null || true;; esac; \
             fi"
        ));
        for _ in 0..20 {
            if sh(&format!("lsof -tiTCP:{port} -sTCP:LISTEN >/dev/null 2>&1")).0 != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// 旧 helper 的升级桥:旧目录里临时重建 `walgit` 软链 + `walgit-ensure`
    /// 转发壳 + 版本 marker,5 分钟后自清。
    fn install_legacy_bridge(bridge: &Path, resources: &Path, bundled_version: &str) {
        let legacy_bin = bridge.join("walgit");
        let legacy_ensure = bridge.join("walgit-ensure");
        let marker = bridge.join(".skeleton-version");
        remove_managed_file_if_safe(&legacy_bin);
        let target = resources.join("walgit");
        let mut bridged = true;
        if let Err(e) = std::os::unix::fs::symlink(&target, &legacy_bin) {
            log_line(&format!("bootstrap: 旧升级 walgit symlink 失败: {e}"));
            bridged = false;
        }
        if let Err(e) = std::fs::write(&marker, bundled_version) {
            log_line(&format!("bootstrap: 旧升级 marker 失败: {e}"));
            bridged = false;
        }
        remove_managed_file_if_safe(&legacy_ensure);
        let shim = LEGACY_ENSURE_SHIM;
        match std::fs::write(&legacy_ensure, shim) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &legacy_ensure,
                    std::fs::Permissions::from_mode(0o755),
                );
                log_line(&format!("bootstrap: v{bundled_version} 旧升级兼容桥已建立"));
            }
            Err(e) => log_line(&format!("bootstrap: 旧升级 walgit-ensure 写入失败: {e}")),
        }
        if bridged {
            let (bridge, resources) = (bridge.to_path_buf(), resources.to_path_buf());
            let version = bundled_version.to_string();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(300));
                let link = bridge.join("walgit");
                let points_at_bundle = std::fs::read_link(&link)
                    .map(|p| p == resources.join("walgit"))
                    .unwrap_or(false);
                if points_at_bundle {
                    remove_managed_file_if_safe(&link);
                }
                let marker_value = std::fs::read_to_string(bridge.join(".skeleton-version"))
                    .map(|v| v.trim().to_string())
                    .unwrap_or_default();
                if marker_value == version {
                    remove_managed_file_if_safe(&bridge.join(".skeleton-version"));
                }
                remove_managed_file_if_safe(&bridge.join("walgit-ensure"));
                remove_managed_file_if_safe(&bridge.join("run-walgit.sh"));
                log_line("bootstrap: 旧升级兼容桥已清理");
            });
        }
    }

    /// Where the "your PATH entry was a copy" note lives, next to the state.
    /// Written only when a copy was healed; removed the next time the entry is
    /// simply correct.
    pub fn healed_marker(state: &Path) -> PathBuf {
        state.join("cli-link-healed.json")
    }

    /// Minimal JSON string escaping — this file is a fixed-format note, not a
    /// serializer worth a dependency for.
    fn json_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push(' '),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// 终端入口：全平台同一条规则（`cli_entry::ensure`）——挑一个「用户 shell
    /// 的 `PATH` 上有、且可写」的目录放软链；占位的旧副本挪开；`PATH` 上没有
    /// 可用目录时如实报告。这里只负责 macOS 侧的日志与 healed 说明文件。
    fn install_cli_link(state: &Path, resources: &Path) {
        if !state.is_absolute() {
            log_line("bootstrap: deployDir 非绝对路径,跳过 CLI 软链");
            return;
        }
        let ensured = crate::cli_entry::ensure(resources);
        let heal = healed_marker(state);
        match &ensured.outcome {
            LinkOutcome::Linked | LinkOutcome::PathAdded => {
                let _ = std::fs::remove_file(&heal);
                if !ensured.on_path {
                    if let Some(dir) = ensured.entry.as_ref().and_then(|entry| entry.parent()) {
                        log_line(&format!(
                            "bootstrap: {} 已就位,但它不在 PATH 上——新开的 shell 仍看不到 `walgit`",
                            dir.display()
                        ));
                    }
                }
            }
            LinkOutcome::Healed { moved_to, version } => {
                // A healed copy is written down: the tray can tell the user their
                // `PATH` entry was a second program file, and the marker disappears
                // on the next start where there is nothing to heal.
                let note = format!(
                    "{{\"path\":{},\"moved_to\":{},\"version\":{},\"bundle\":{}}}",
                    json_string(
                        &ensured
                            .entry
                            .as_ref()
                            .map(|entry| entry.display().to_string())
                            .unwrap_or_default()
                    ),
                    json_string(&moved_to.display().to_string()),
                    json_string(version.as_deref().unwrap_or("")),
                    json_string(&resources.join("walgit").display().to_string()),
                );
                if let Err(e) = std::fs::write(&heal, note) {
                    log_line(&format!("bootstrap: 写 {} 失败: {e}", heal.display()));
                }
                log_line(&format!(
                    "bootstrap: PATH 上是旧副本({}),已移到 {} 并换成指向 bundle 的软链",
                    version.as_deref().unwrap_or("?"),
                    moved_to.display()
                ));
            }
            LinkOutcome::Foreign => log_line("bootstrap: PATH 入口被别人的文件占着,未触碰"),
            LinkOutcome::NotCreatable(e) => {
                log_line(&format!("bootstrap: CLI 软链未创建: {e}"));
            }
        }
    }

    /// 首次启动 bootstrap。可重入:重复运行只补齐缺失项。
    pub fn bootstrap_deploy() {
        let Some(resources) = resource_dir() else {
            log_line("bootstrap: 不是 App Bundle,跳过");
            return;
        };
        let Ok(metadata) = std::fs::metadata(resources.join("walgit")) else {
            log_line("bootstrap: bundle 无 walgit 资源(开发构建),跳过");
            return;
        };
        if !metadata.is_file() {
            log_line("bootstrap: bundle 无 walgit 资源(开发构建),跳过");
            return;
        }
        let state = state_dir();
        if let Err(e) = std::fs::create_dir_all(&state) {
            log_line(&format!("bootstrap: 建 {} 失败: {e}", state.display()));
            return;
        }
        let legacy = legacy_dir();
        let legacy_config = legacy.join("walgit.toml");
        let has_legacy = legacy_config.exists()
            || legacy.join("walgit").exists()
            || legacy.join("walgit-ensure").exists();

        let mut legacy_was_running = false;
        if has_legacy {
            let listen = listen_addr(&legacy_config);
            legacy_was_running = health_ok(&format!("http://{listen}/healthz"));
            if legacy_was_running {
                stop_legacy_service(&listen);
            }
            migrate_legacy_state(&legacy, &state);
        }

        // 用户配置:只在缺失时从 bundle 模板初始化,永不覆盖。
        let user_config = state.join("walgit.toml");
        if !user_config.exists() {
            let template = resources.join("walgit.toml");
            if template.is_file() {
                match std::fs::copy(&template, &user_config) {
                    Ok(_) => log_line("bootstrap: 写入 walgit.toml"),
                    Err(e) => log_line(&format!("bootstrap: walgit.toml 失败: {e}")),
                }
            }
        }

        let bridge = if has_legacy { legacy } else { state.clone() };
        let bundled_version = crate::app_version();
        let legacy_layout = has_legacy
            || bridge.join("walgit").exists()
            || bridge.join(".skeleton-version").exists();
        if legacy_layout && !bundled_version.is_empty() {
            install_legacy_bridge(&bridge, &resources, &bundled_version);
        } else {
            remove_managed_file_if_safe(&bridge.join("walgit-ensure"));
            remove_managed_file_if_safe(&bridge.join("run-walgit.sh"));
        }

        install_cli_link(&state, &resources);

        // 旧服务本来在跑 → 用新 bundle 带起来;否则若端口上跑的还是旧版本,
        // 重启到新版本(DMG 覆写安装、服务没停的场景)。
        if legacy_was_running {
            let result = crate::service_start();
            log_line(&format!("bootstrap: 旧服务迁移后启动 {result:?}"));
        } else if !bundled_version.is_empty() {
            let url = format!("http://{}/healthz", crate::deploy_config().0);
            let (code, body) = sh(&format!(
                "curl -sf --max-time 2 '{url}' 2>/dev/null || true"
            ));
            if code == 0 && !body.trim().is_empty() {
                let running = crate::version_of(&body);
                if walgit_tray::release::strip_version_prefix(&running)
                    != walgit_tray::release::strip_version_prefix(&bundled_version)
                {
                    let result = crate::service_cmd("restart");
                    log_line(&format!(
                        "bootstrap: 服务版本落后({running}) restart {result:?}"
                    ));
                }
            }
        }
        log_line(&format!("bootstrap: 状态目录就绪({})", state.display()));
    }
}

#[cfg(target_os = "macos")]
pub use macos::{bootstrap_deploy, state_dir};
/// The CLI-entry rule and its note are exercised by the tests below; the tray
/// reads the note when it grows a UI for it (`cli-link-healed.json`).
#[cfg(all(test, unix))]
pub use crate::cli_entry::{install_one_cli_link, LinkOutcome};

/// 非 macOS:没有 bundle 迁移语义,但「`walgit` 必须在 `PATH` 上」这条一样适用
/// (Linux 建软链,Windows 补用户 `PATH`),入口规则见 `cli_entry`。
#[cfg(not(target_os = "macos"))]
pub fn bootstrap_deploy() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let ensured = crate::cli_entry::ensure(dir);
    match &ensured.outcome {
        crate::cli_entry::LinkOutcome::Linked | crate::cli_entry::LinkOutcome::PathAdded => {
            if !ensured.on_path {
                crate::log_line(&format!(
                    "bootstrap: 入口已建 ({}),但它不在 PATH 上——新开的 shell 仍看不到 `walgit`",
                    ensured
                        .entry
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |p| p.display().to_string())
                ));
            }
        }
        crate::cli_entry::LinkOutcome::Healed { moved_to, version } => {
            crate::log_line(&format!(
                "bootstrap: PATH 上是旧副本({}),已移到 {} 并换成软链",
                version.as_deref().unwrap_or("?"),
                moved_to.display()
            ));
        }
        crate::cli_entry::LinkOutcome::Foreign => {
            crate::log_line("bootstrap: PATH 入口被别人的文件占着,未触碰");
        }
        crate::cli_entry::LinkOutcome::NotCreatable(e) => {
            crate::log_line(&format!("bootstrap: 建 CLI 入口失败: {e}"));
        }
    }
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::{install_one_cli_link, LinkOutcome};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A stand-in for the bundle's program: small enough to be any file, but
    /// the tests never run it through the version probe.
    fn bundle_program(dir: &Path) -> PathBuf {
        let target = dir.join("bundle-walgit");
        std::fs::write(&target, b"#!/bin/sh\necho 'walgit v9.9.9'\n").expect("write target");
        target
    }

    /// A hand-copied CLI: an executable above the size gate that answers
    /// `--version` with an older walgit. This is the shape found in the wild —
    /// a second program file on `PATH` that no upgrade can reach.
    fn old_copy(path: &Path) {
        let mut body = String::from("#!/bin/sh\necho 'walgit 0.5.0-old'\n");
        while body.len() < (1 << 20) + 1 {
            body.push_str("# padding\n");
        }
        std::fs::write(path, body).expect("write copy");
        let mut perms = std::fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    #[test]
    fn a_missing_entry_becomes_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = bundle_program(dir.path());
        let link = dir.path().join("bin").join("walgit");
        assert_eq!(install_one_cli_link(&link, &target), LinkOutcome::Linked);
        assert_eq!(std::fs::read_link(&link).expect("link"), target);
    }

    #[test]
    fn a_symlink_bound_elsewhere_is_repointed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = bundle_program(dir.path());
        let link = dir.path().join("walgit");
        let stale = dir.path().join("stale-walgit");
        std::fs::write(&stale, b"#!/bin/sh\n").expect("write stale");
        std::os::unix::fs::symlink(&stale, &link).expect("symlink");
        assert_eq!(install_one_cli_link(&link, &target), LinkOutcome::Linked);
        assert_eq!(std::fs::read_link(&link).expect("link"), target);
    }

    /// The failure this guards: a copy is not a version of the program, it is a
    /// second program file, so upgrading the app never touches it. It is moved
    /// aside (the user's file is never deleted) and the symlink takes its place.
    #[test]
    fn a_copy_of_a_walgit_binary_is_moved_aside_and_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = bundle_program(dir.path());
        let link = dir.path().join("walgit");
        old_copy(&link);
        let outcome = install_one_cli_link(&link, &target);
        let LinkOutcome::Healed { moved_to, version } = outcome else {
            panic!("expected a heal, got {outcome:?}");
        };
        assert_eq!(version.as_deref(), Some("walgit 0.5.0-old"));
        assert!(
            moved_to.exists(),
            "the copy is kept: {}",
            moved_to.display()
        );
        assert_eq!(
            std::fs::read_link(&link).expect("link"),
            target,
            "the entry now points into the bundle"
        );
        // Running the heal again is idempotent: a correct symlink, no second copy.
        assert_eq!(install_one_cli_link(&link, &target), LinkOutcome::Linked);
    }

    /// Anything that is not a walgit binary is the user's: a wrapper script, a
    /// second implementation, a symlink they made. Never touched.
    #[test]
    fn a_foreign_file_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = bundle_program(dir.path());
        let link = dir.path().join("walgit");
        std::fs::write(&link, b"#!/bin/sh\nexec /somewhere/else \"$@\"\n").expect("write wrapper");
        assert_eq!(install_one_cli_link(&link, &target), LinkOutcome::Foreign);
        assert_eq!(
            std::fs::read(&link).expect("read"),
            b"#!/bin/sh\nexec /somewhere/else \"$@\"\n"
        );
        assert!(std::fs::symlink_metadata(&link)
            .expect("meta")
            .file_type()
            .is_file());
    }
}
