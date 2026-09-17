//! The walgit ops skill, shipped with the binary. The in-repo
//! `skills/walgit/SKILL.md` is served verbatim on the **public lane** (the same
//! data-free prefix as `/services/public/install.sh`), next to a manifest
//! (`name` / `version` / `sha256` / `bytes`) and a one-line installer.
//!
//! An agent that reads the host's `/SKILL.md` (D42) can run
//! `curl -fsSL <host>/services/public/skill/install.sh | sh` to drop the skill
//! matching that host's build into `${WALGIT_SKILL_DIR:-$HOME/.agents/skills/walgit}`.
//!
//! Everything here is data-free: like the rest of `/services/public/*`, nothing
//! credential-bearing or repo-scoped may ever be added.

use serde::Serialize;
use sha2::{Digest, Sha256};
use walgit_config::{Config, TlsMode};

/// The skill source, compiled into the binary — one home: the repo file.
pub const SKILL_MD: &str = include_str!("../../../skills/walgit/SKILL.md");

/// Skill name (also the directory under `~/.agents/skills/`).
pub const NAME: &str = "walgit";

/// Where the skill is served (relative to the request's base URL).
pub const SKILL_PATH: &str = "/services/public/skill/SKILL.md";
/// Where the installer is served (relative to the request's base URL).
pub const INSTALL_PATH: &str = "/services/public/skill/install.sh";

/// Lower-case hex sha256 of `bytes` — the manifest's and installer's integrity value.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// The manifest (`/services/public/skill/manifest.json`): what an agent needs to
/// verify and re-fetch the skill this host serves.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub name: &'static str,
    /// The server build (`instance::build_version()`): the skill matches the binary that served it.
    pub version: String,
    /// sha256 of the exact bytes served at `skill_url` (lower-case hex).
    pub sha256: String,
    /// Byte length of the served skill.
    pub bytes: usize,
    pub skill_url: String,
    pub install_url: String,
}

pub fn manifest(base_url: &str) -> Manifest {
    let base = base_url.trim_end_matches('/');
    Manifest {
        name: NAME,
        version: crate::instance::build_version(),
        sha256: sha256_hex(SKILL_MD.as_bytes()),
        bytes: SKILL_MD.len(),
        skill_url: format!("{base}{SKILL_PATH}"),
        install_url: format!("{base}{INSTALL_PATH}"),
    }
}

/// The installer served at `/services/public/skill/install.sh`: pure POSIX sh,
/// **idempotent** (re-running after a host upgrade refreshes the skill), with the
/// expected sha256 and base URL baked in so the downloaded bytes are verified.
///
/// A self-signed origin cannot be verified by curl before the *client* installer
/// pinned its certificate, so this bootstrap fetch falls back to `-k` — the same
/// one-time concession `/services/public/install.sh` makes. The `sha256` check
/// still pins the content.
pub fn install_script(cfg: &Config, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let sha = sha256_hex(SKILL_MD.as_bytes());
    let version = crate::instance::build_version();
    let curl = if cfg.server.tls.mode == TlsMode::SelfSigned {
        "curl -fsSLk"
    } else {
        "curl -fsSL"
    };
    format!(
        r#"#!/bin/sh
# {NAME} ops skill — {version}, served by {base}
# Idempotent: re-run any time (e.g. after upgrading the host) to refresh to that build.
set -eu

sha256_of() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{{print $1}}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{{print $1}}'
  else
    echo "{NAME}: need sha256sum or shasum on PATH" >&2
    return 1
  fi
}}

DIR="${{WALGIT_SKILL_DIR:-$HOME/.agents/skills/{NAME}}}"
DEST="$DIR/SKILL.md"
SHA="{sha}"
URL="{base}{SKILL_PATH}"

if [ -f "$DEST" ] && [ "$(sha256_of "$DEST")" = "$SHA" ]; then
  echo "{NAME}: already up to date ({version}) -> $DEST"
  exit 0
fi

mkdir -p "$DIR"
TMP="$DEST.tmp.$$"
trap 'rm -f "$TMP"' EXIT HUP INT TERM
{curl} "$URL" -o "$TMP"
GOT="$(sha256_of "$TMP")"
if [ "$GOT" != "$SHA" ]; then
  echo "{NAME}: checksum mismatch: expected $SHA, got $GOT" >&2
  exit 1
fi
mv "$TMP" "$DEST"
trap - EXIT HUP INT TERM
echo "{NAME}: installed {version} -> $DEST"
"#,
    )
}

#[cfg(test)]
mod tests {
    use super::{SKILL_MD, manifest, sha256_hex};
    use walgit_config::Config;

    #[test]
    fn sha256_hex_matches_the_known_vector() {
        // FIPS 180-2 vector: sha256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifest_hashes_the_served_bytes() {
        let m = manifest("https://git.example.com/");
        assert_eq!(m.name, "walgit");
        assert_eq!(m.sha256, sha256_hex(SKILL_MD.as_bytes()));
        assert_eq!(m.bytes, SKILL_MD.len());
        assert_eq!(m.skill_url, "https://git.example.com/services/public/skill/SKILL.md");
        assert_eq!(m.install_url, "https://git.example.com/services/public/skill/install.sh");
        assert!(!m.version.is_empty());
    }

    #[test]
    fn install_script_pins_the_sha_and_writes_the_skill_dir() {
        let script = super::install_script(&Config::default(), "https://git.example.com");
        assert!(script.starts_with("#!/bin/sh"), "{script}");
        assert!(script.contains(&sha256_hex(SKILL_MD.as_bytes())), "{script}");
        assert!(script.contains("WALGIT_SKILL_DIR:-$HOME/.agents/skills/walgit"), "{script}");
        assert!(script.contains("/services/public/skill/SKILL.md"), "{script}");
        // A public-CA host must not downgrade verification.
        assert!(!script.contains("curl -fsSLk"), "{script}");
    }

    // The installer is the documented one-liner in `web/SKILL.md`; run it for
    // real. `curl` speaks `file://`, so the whole path — download, sha256 check,
    // write, idempotent re-run — is exercised without a network or a server.
    #[cfg(unix)]
    #[test]
    fn install_script_lays_down_the_skill_end_to_end() {
        use std::process::Command;

        let root = std::env::temp_dir().join(format!("walgit-skill-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let served = root.join("services/public/skill/SKILL.md");
        std::fs::create_dir_all(served.parent().unwrap()).unwrap();
        std::fs::write(&served, SKILL_MD).unwrap();
        let dest = root.join("installed");

        let script = super::install_script(
            &Config::default(),
            &format!("file://{}", root.display()),
        );
        let run = || {
            Command::new("sh")
                .args(["-c", &script])
                .env("WALGIT_SKILL_DIR", &dest)
                .output()
                .expect("run installer")
        };

        let first = run();
        assert!(
            first.status.success(),
            "installer failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            SKILL_MD
        );

        // Idempotent: a second run is a verified no-op, not a re-download.
        let second = run();
        assert!(second.status.success(), "{second:?}");
        assert!(
            String::from_utf8_lossy(&second.stdout).contains("already up to date"),
            "{}",
            String::from_utf8_lossy(&second.stdout)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn self_signed_host_bootstraps_insecurely_but_still_pins_the_sha() {
        let mut cfg = Config::default();
        cfg.server.tls.mode = walgit_config::TlsMode::SelfSigned;
        let script = super::install_script(&cfg, "https://git.example.com");
        assert!(script.contains("curl -fsSLk"), "{script}");
        assert!(script.contains(&sha256_hex(SKILL_MD.as_bytes())), "{script}");
    }
}
