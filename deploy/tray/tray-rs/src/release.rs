//! Release 检测/升级的**纯逻辑**:版本比较、GitHub latest release 解析、
//! 菜单文本渲染。
//!
//! 这些判断决定「要不要下载并安装一个新二进制」——解析错一次就会让用户
//! 看到错误的升级提示(或把别的 tag 的 asset 当成新版本),所以全部做成
//! 无 IO 的纯函数,由 `cargo test` 覆盖。
//!
//! 来源:macOS Swift 托盘(`ReleaseLogic.swift` + `walgit-tray.swift` 的
//! `menuVersionLine`/`upgradeLine`);issue #183 把它迁到跨平台 tray-rs。
//!
//! 生产调用点只有 macOS 的 Release 通道;三平台都跑 `cargo test`,所以非
//! macOS 的 release 构建里"没人用"是预期,不是死代码。
#![cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]

use serde_json::Value;

// 升级状态机(三平台一致)
pub const ST_IDLE: u8 = 0; // 未查:检查更新…
pub const ST_CHECKING: u8 = 1; // 正在检查更新…
pub const ST_LATEST: u8 = 2; // 已是最新 ✓(点击重查)
pub const ST_AVAILABLE: u8 = 3; // ⬆️ 升级到新版本
pub const ST_INSTALLING: u8 = 4; // 升级中…
pub const ST_FAILED: u8 = 5; // 失败(点击重查)

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: String,
    pub asset: ReleaseAsset,
}

/// 当前架构的资产后缀。未知架构返回 `unknown`——此时永远匹配不到 asset,
/// 菜单停在「上次升级失败」而不是抓错包。
pub fn arch_slug() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        "unknown"
    }
}

/// 去掉 `v`/`V` 前缀并 trim。空串保持空串。
pub fn strip_version_prefix(value: &str) -> String {
    let trimmed = value.trim();
    match trimmed.strip_prefix(['v', 'V']) {
        Some(rest) => rest.to_string(),
        None => trimmed.to_string(),
    }
}

/// 解析 GitHub `releases/latest` 响应。
///
/// 只接受**与本 release 版本严格同名**、目标架构的 DMG:按后缀回退会把旧
/// 版本 asset 挂到新 tag 上(菜单误报升级、下载后才失败)。
pub fn parse_latest_release(json: &str, arch: &str) -> Result<ReleaseInfo, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| format!("malformed release JSON: {e}"))?;
    let tag = root
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| "malformed release JSON: missing tag_name".to_string())?;
    let version = strip_version_prefix(tag);
    if version.is_empty() {
        return Err("malformed release JSON: empty release version".into());
    }
    let assets = root
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| "malformed release JSON: missing assets array".to_string())?;
    let expected = format!("walgit-{version}-{arch}.dmg");
    let asset = assets
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(expected.as_str()))
        .ok_or_else(|| format!("release has no macOS asset {expected}"))?;
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| format!("release asset {expected} has no download url"))?;
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or_else(|| format!("release asset {expected} has no sha256 digest"))?;
    let sha = digest.to_lowercase();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("release asset {expected} has no sha256 digest"));
    }
    Ok(ReleaseInfo {
        tag: tag.to_string(),
        version,
        asset: ReleaseAsset {
            name: expected,
            url: url.to_string(),
            sha256: sha,
        },
    })
}

/// SemVer 子集:`+build` 元数据不参与优先级;数字段逐段比较;有 prerelease
/// 的小于同号正式版;prerelease 标识符按「数字段数值序、文本段自然序」比较。
fn version_parts(value: &str) -> (Vec<u64>, Vec<String>) {
    let stripped = strip_version_prefix(value);
    let without_build = stripped.split('+').next().unwrap_or("").to_string();
    let (numeric, prerelease) = match without_build.split_once('-') {
        Some((n, p)) => (n.to_string(), p.to_string()),
        None => (without_build, String::new()),
    };
    let numbers: Vec<u64> = numeric
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect();
    let pre: Vec<String> = if prerelease.is_empty() {
        Vec::new()
    } else {
        prerelease.split('.').map(|s| s.to_string()).collect()
    };
    (numbers, pre)
}

/// 自然序比较(`beta2 < beta10`):字母段大小写不敏感,数字段按数值。
fn natural_cmp(lhs: &str, rhs: &str) -> std::cmp::Ordering {
    let (l, r) = (lhs.to_lowercase(), rhs.to_lowercase());
    let (lb, rb) = (l.as_bytes(), r.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < lb.len() && j < rb.len() {
        let (lc, rc) = (lb[i], rb[j]);
        if lc.is_ascii_digit() && rc.is_ascii_digit() {
            let si = i;
            let sj = j;
            while i < lb.len() && lb[i].is_ascii_digit() {
                i += 1;
            }
            while j < rb.len() && rb[j].is_ascii_digit() {
                j += 1;
            }
            let ln: u64 = l[si..i].parse().unwrap_or(0);
            let rn: u64 = r[sj..j].parse().unwrap_or(0);
            if ln != rn {
                return ln.cmp(&rn);
            }
        } else {
            if lc != rc {
                return lc.cmp(&rc);
            }
            i += 1;
            j += 1;
        }
    }
    (lb.len() - i).cmp(&(rb.len() - j))
}

fn compare_prerelease(lhs: &[String], rhs: &[String]) -> std::cmp::Ordering {
    for index in 0..lhs.len().max(rhs.len()) {
        let (Some(l), Some(r)) = (lhs.get(index), rhs.get(index)) else {
            // 段数少的更小:a < a.b
            return if index >= lhs.len() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        };
        let (ln, rn) = (l.parse::<u64>(), r.parse::<u64>());
        let ord = match (ln, rn) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => natural_cmp(l, r),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

pub fn is_version_newer(candidate: &str, current: &str) -> bool {
    let (candidate_numbers, candidate_pre) = version_parts(candidate);
    let (current_numbers, current_pre) = version_parts(current);
    for index in 0..candidate_numbers.len().max(current_numbers.len()) {
        let left = candidate_numbers.get(index).copied().unwrap_or(0);
        let right = current_numbers.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    // 同号:有 prerelease 的更旧。
    if candidate_pre.is_empty() != current_pre.is_empty() {
        return !current_pre.is_empty();
    }
    if candidate_pre.is_empty() {
        return false;
    }
    compare_prerelease(&candidate_pre, &current_pre) == std::cmp::Ordering::Greater
}

/// 从 app 的 `Info.plist` 里取 `CFBundleShortVersionString`(已去 v 前缀)。
/// 只认 XML plist——我们打包的就是 XML,不引入 plist 解析依赖。
pub fn parse_bundle_version(plist: &str) -> Option<String> {
    const KEY: &str = "CFBundleShortVersionString";
    let at = plist.find(&format!("<key>{KEY}</key>"))?;
    let rest = &plist[at + KEY.len()..];
    let open = rest.find("<string>")? + "<string>".len();
    let close = rest[open..].find("</string>")? + open;
    let value = strip_version_prefix(&rest[open..close]);
    (!value.is_empty()).then_some(value)
}

/// 菜单里的版本语义:升级行判断的是**托盘 app 版本**,所以显示也用 app
/// 版本;服务进程版本另附,避免「已是最新」旁边印着更旧的服务版本(#170)。
pub fn menu_version_line(app_version: &str, service_version: &str) -> String {
    let app = strip_version_prefix(app_version);
    let service = strip_version_prefix(service_version);
    if service.is_empty() {
        format!("版本 {app}")
    } else {
        format!("版本 {app} · 服务 {service}")
    }
}

/// 升级菜单行(三平台共用的状态机文本)。
pub fn upgrade_line(
    state: u8,
    app_version: &str,
    service_version: &str,
    release: Option<&ReleaseInfo>,
    source_sha: &str,
    busy_note: &str,
) -> String {
    let version_text = menu_version_line(app_version, service_version);
    let app = strip_version_prefix(app_version);
    match state {
        ST_CHECKING => format!("{version_text} · 正在检查更新…"),
        ST_LATEST => format!("{version_text} · 已是最新 ✓(点击重查)"),
        ST_AVAILABLE => {
            if let Some(release) = release {
                format!(
                    "⬆️ 下载并升级到 v{}(当前 {app})",
                    strip_version_prefix(&release.version)
                )
            } else {
                format!("⬆️ 从源码升级到 {source_sha}(当前 {app})")
            }
        }
        ST_INSTALLING => {
            if busy_note.is_empty() {
                "升级中…".into()
            } else {
                format!("升级中… · {busy_note}")
            }
        }
        ST_FAILED => "上次升级失败(点击重查)".into(),
        _ => format!("{version_text} · 检查更新…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "tag_name": "v0.5.0",
      "assets": [
        {"name":"walgit-0.5.0-x86_64.dmg","browser_download_url":"https://example.invalid/x86.dmg","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        {"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/arm.dmg","digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
      ]
    }"#;

    #[test]
    fn arch_slug_is_known_or_unknown() {
        // 未知架构永远匹配不到 asset——宁可报"没有可用更新",也不装错包。
        assert!(
            matches!(arch_slug(), "arm64" | "x86_64" | "unknown"),
            "unexpected arch slug: {}",
            arch_slug()
        );
    }

    #[test]
    fn parses_arch_specific_asset() {
        let info = parse_latest_release(FIXTURE, "arm64").expect("parse");
        assert_eq!(info.tag, "v0.5.0");
        assert_eq!(info.version, "0.5.0");
        assert_eq!(info.asset.name, "walgit-0.5.0-arm64.dmg");
        assert_eq!(info.asset.url, "https://example.invalid/arm.dmg");
        assert_eq!(info.asset.sha256, "b".repeat(64));
    }

    #[test]
    fn rejects_malformed_release_json() {
        assert!(parse_latest_release("{}", "arm64").is_err(), "empty object");
        assert!(
            parse_latest_release(r#"{"tag_name":""}"#, "arm64").is_err(),
            "empty tag"
        );
        assert!(
            parse_latest_release(r#"{"tag_name":"v0.5.0"}"#, "arm64").is_err(),
            "missing assets"
        );
        assert!(parse_latest_release("not json", "arm64").is_err());
    }

    #[test]
    fn rejects_asset_without_usable_digest() {
        let missing = r#"{"tag_name":"v0.5.0","assets":[{"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/a.dmg"}]}"#;
        assert!(parse_latest_release(missing, "arm64").is_err());
        // GitHub 会在拿不到 digest 时给 null(而不是缺 key):同一个失败面。
        let null_digest = r#"{"tag_name":"v0.5.0","assets":[{"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/a.dmg","digest":null}]}"#;
        assert!(parse_latest_release(null_digest, "arm64").is_err());
        let short = r#"{"tag_name":"v0.5.0","assets":[{"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/a.dmg","digest":"sha256:deadbeef"}]}"#;
        assert!(parse_latest_release(short, "arm64").is_err());
        let non_hex = r#"{"tag_name":"v0.5.0","assets":[{"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/a.dmg","digest":"sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"}]}"#;
        assert!(parse_latest_release(non_hex, "arm64").is_err());
    }

    #[test]
    fn rejects_other_arch_and_stale_assets() {
        let x86_only = r#"{"tag_name":"v0.5.0","assets":[{"name":"walgit-0.5.0-x86_64.dmg","browser_download_url":"https://example.invalid/x.dmg","digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}]}"#;
        assert!(parse_latest_release(x86_only, "arm64").is_err());
        // 后缀回退会让新 tag 挂上旧版本 asset——必须拒绝。
        let stale = r#"{"tag_name":"v0.6.0","assets":[{"name":"walgit-0.5.0-arm64.dmg","browser_download_url":"https://example.invalid/old.dmg","digest":"sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"}]}"#;
        assert!(parse_latest_release(stale, "arm64").is_err());
    }

    #[test]
    fn version_ordering_matches_release_semantics() {
        assert!(is_version_newer("0.5.0", "0.4.0"), "newer patch line");
        assert!(!is_version_newer("0.4.0", "0.4.0"), "same version");
        assert!(!is_version_newer("0.4.9", "0.5.0"), "older version");
        assert!(
            is_version_newer("0.5.0", "0.5.0-beta.1"),
            "release beats prerelease"
        );
        assert!(
            is_version_newer("0.5.0-beta.2", "0.5.0-beta.1"),
            "newer prerelease"
        );
        assert!(is_version_newer("1.0.0", "0.99.99"), "major");
        assert!(
            is_version_newer("0.5.0-beta.10", "0.5.0-beta.2"),
            "numeric prerelease ordering"
        );
        assert!(!is_version_newer("0.5.0-rc.1", "0.5.0"), "release beats rc");
        assert!(
            is_version_newer("0.5.0", "0.5.0-rc.1"),
            "rc is older than release"
        );
        assert!(!is_version_newer("0.5", "0.5.0"), "trailing zero equal");
        assert!(
            !is_version_newer("0.5.0", "0.5"),
            "trailing zero equal reversed"
        );
        assert!(is_version_newer("V0.6.0", "v0.5.0"), "uppercase v prefix");
        assert!(!is_version_newer("", "0.5.0"), "empty version is not newer");
        assert!(
            !is_version_newer("1.0.0+build.2", "1.0.0+build.1"),
            "build metadata not precedence"
        );
        assert!(
            !is_version_newer("1.0.0+build.2", "1.0.0"),
            "build metadata equal to release"
        );
        assert!(
            is_version_newer("1.0.1+build.2", "1.0.0"),
            "past build metadata still ordered"
        );
        // 自然序:Swift 版用 .numeric 比较,文本段里的数字不能被字典序颠倒。
        assert!(
            is_version_newer("0.5.0-beta10", "0.5.0-beta2"),
            "natural text order"
        );
        assert!(
            !is_version_newer("0.5.0-alpha.1", "0.5.0-beta.1"),
            "alpha < beta"
        );
    }

    #[test]
    fn parses_bundle_version_from_plist() {
        let plist = r#"<?xml version="1.0"?><plist><dict>
            <key>CFBundleIdentifier</key><string>com.walgit.tray</string>
            <key>CFBundleShortVersionString</key><string>0.6.3</string>
            <key>CFBundleVersion</key><string>0.6.3</string>
        </dict></plist>"#;
        assert_eq!(parse_bundle_version(plist).as_deref(), Some("0.6.3"));
        let prefixed = r#"<key>CFBundleShortVersionString</key><string>v0.6.3</string>"#;
        assert_eq!(parse_bundle_version(prefixed).as_deref(), Some("0.6.3"));
        assert_eq!(parse_bundle_version("<plist></plist>"), None);
        assert_eq!(
            parse_bundle_version("<key>CFBundleShortVersionString</key><string></string>"),
            None
        );
    }

    #[test]
    fn menu_lines_keep_app_and_service_versions_apart() {
        assert_eq!(
            menu_version_line("0.5.1", "v0.5.0"),
            "版本 0.5.1 · 服务 0.5.0"
        );
        assert_eq!(menu_version_line("0.5.1", ""), "版本 0.5.1");

        let line = upgrade_line(ST_IDLE, "0.5.1", "v0.5.0", None, "", "");
        assert!(
            line.contains("版本 0.5.1") && line.contains("服务 0.5.0"),
            "{line}"
        );
        assert!(line.contains("检查更新…"), "{line}");

        let line = upgrade_line(ST_LATEST, "0.5.1", "v0.5.0", None, "", "");
        assert!(line.contains("已是最新"), "{line}");
        assert!(
            !line.contains("版本 0.5.0"),
            "service version used as app: {line}"
        );

        let release = parse_latest_release(FIXTURE, "arm64").expect("parse");
        let line = upgrade_line(ST_AVAILABLE, "0.5.0", "", Some(&release), "", "");
        assert_eq!(line, "⬆️ 下载并升级到 v0.5.0(当前 0.5.0)");

        let line = upgrade_line(ST_AVAILABLE, "0.5.0", "", None, "abcdef1", "");
        assert_eq!(line, "⬆️ 从源码升级到 abcdef1(当前 0.5.0)");

        let line = upgrade_line(ST_INSTALLING, "0.5.0", "", None, "", "下载中");
        assert_eq!(line, "升级中… · 下载中");
        assert_eq!(
            upgrade_line(ST_FAILED, "0.5.0", "", None, "", ""),
            "上次升级失败(点击重查)"
        );
    }
}
