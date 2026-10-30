//! Server-held collab signing keys (D64).
//!
//! Read tools need nothing here; the two write tools do. A collab entry is only
//! counted by every consumer of the log when its signature verifies against the
//! public key registered for the entry's **own** actor (D1 §4.3/§5), so a
//! server that signs on a caller's behalf must hold *that caller's* key — there
//! is no "server identity" to sign with, and inventing one would mint entries
//! that never verify.
//!
//! The custody rule is therefore narrow on purpose: `mcp.key_dir` names a
//! directory the operator controls, one `<principal>.ed25519` per principal,
//! and the server loads exactly the file named after the `actor` argument. The
//! file format is the one `walgit collab join` writes (D58) — the raw 32-byte
//! Ed25519 seed as hex, mode 0600 — so an operator moves a worktree key there
//! as-is. Absent `key_dir` there are no write tools at all: an empty surface
//! instead of a silent read-only signer.
//!
//! Two guards run before any entry is published, and both refuse rather than
//! warn:
//!  1. `actor` must be the authenticated caller (the API enforces this too; here
//!     it fails before a signature is even produced);
//!  2. the loaded key must be the key registered for `actor` — the check
//!     `ensure_actor_key_binding` applies agent-side, ported so a misconfigured
//!     key directory cannot fill the append-only log with unverifiable entries.

use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;

/// A collab ref segment (actor / principal): the same rule the CLI applies, so
/// a name that would be mangled in `refs/collab/inbox/<actor>/…` is refused
/// here rather than quietly creating a ref nobody can find.
pub fn ref_segment(label: &str, s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 255
        && !s.contains("..")
        && s != "."
        && s != ".."
        && !s.to_ascii_lowercase().ends_with(".lock")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "collab.{label}: {s:?} is not a refname-safe segment ([A-Za-z0-9._@-]+, starting with a letter or digit)"
        ))
    }
}

/// The seed path for `principal` under `key_dir`.
pub fn key_path(cfg: &walgit_config::McpConfig, principal: &str) -> Result<PathBuf, String> {
    ref_segment("actor", principal)?;
    let dir = cfg.key_dir.as_ref().ok_or_else(|| {
        "this instance has no `mcp.key_dir`, so its MCP surface is read-only \
         (`collab_entry`/`collab_claim` are not registered). To let agents write, put \
         `<principal>.ed25519` seeds — the hex 32-byte seed `walgit collab join` writes, \
         mode 0600 — in a directory and set `[mcp] key_dir` to it"
            .to_string()
    })?;
    Ok(dir.join(format!("{principal}.ed25519")))
}

/// Load the signing key the server may use for `principal`.
pub async fn key_for(cfg: &walgit_config::McpConfig, principal: &str) -> Result<SigningKey, String> {
    let path = key_path(cfg, principal)?;
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("reading the signing key {}: {e}", path.display()))?;
    let bytes = hex::decode(raw.trim())
        .map_err(|e| format!("{} must be the 32-byte seed as hex: {e}", path.display()))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| format!("{} must be 32 bytes of seed (hex)", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Refuse a write whose key is not the one registered for `actor`.
///
/// `registered` is the principal registry as verification reads it (the repo's
/// `refs/collab/meta/principals/*` overlaid with the host registry): repo-local
/// wins, exactly as `is_verified` resolves it. The signature itself is the
/// check — sign, then verify against the registered key — so this cannot drift
/// from the verifier.
pub fn check_binding(
    actor: &str,
    registered: Option<&str>,
    entry: &walgit_wal::collab::Entry,
    key_path: &Path,
) -> Result<(), String> {
    let Some(registered) = registered else {
        return Err(format!(
            "cannot sign as actor {actor} with {}: this host has no collab public key \
             registered for {actor}, so the signature cannot be checked. Register the \
             principal first (`walgit collab join --principal {actor} --key <seed> \
             --push origin` from the project), or point `mcp.key_dir` at the key of a \
             registered principal",
            key_path.display()
        ));
    };
    match walgit_wal::collab::verify_entry(entry, registered) {
        Ok(()) => Ok(()),
        Err(e) => Err(format!(
            "refusing to publish: the seed at {} is not the key registered for {actor} \
             ({e}). Verification uses only the key registered for an entry's own actor, \
             and the log is append-only — the entry could not be deleted or edited \
             afterwards. Use the seed registered for {actor}, or sign as the principal \
             this seed belongs to",
            key_path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_segments_match_git_rules() {
        assert!(ref_segment("actor", "coordinator").is_ok());
        assert!(ref_segment("actor", "worker-1.a_b@c").is_ok());
        assert!(ref_segment("actor", "").is_err());
        assert!(ref_segment("actor", "..").is_err());
        assert!(ref_segment("actor", "a/b").is_err());
        assert!(ref_segment("actor", "-lead").is_err());
        assert!(ref_segment("actor", "x.lock").is_err());
        assert!(ref_segment("actor", "sp ace").is_err());
    }

    #[test]
    fn key_path_requires_a_configured_directory() {
        let cfg = walgit_config::McpConfig::default();
        assert!(key_path(&cfg, "someone").unwrap_err().contains("read-only"));
        let cfg = walgit_config::McpConfig {
            key_dir: Some(PathBuf::from("/tmp/keys")),
            ..walgit_config::McpConfig::default()
        };
        assert_eq!(
            key_path(&cfg, "someone").unwrap(),
            PathBuf::from("/tmp/keys/someone.ed25519")
        );
        assert!(key_path(&cfg, "../escape").is_err());
    }
}
