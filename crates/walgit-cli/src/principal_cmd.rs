//! `walgit principal` — host-global principal registry commands (issue #76):
//! register / list / revoke / rotate against `/api/v1/principals` (D1 §5
//! cross-repo extension). HTTP-only, no bucket access.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::Subcommand;
use serde_json::json;

use crate::collab_cmd::{read_signing_key, ref_segment};

#[derive(Subcommand)]
pub enum PrincipalAction {
    /// Register (or re-register = rotate) the caller's public key in the host
    /// registry: one registration verifies in every repository of that host.
    Register {
        /// Host root URL, e.g. `http://walgit.localhost:8081`.
        #[arg(long)]
        url: String,
        /// Principal (refname-safe); must equal the authenticated principal.
        #[arg(long)]
        principal: String,
        /// Ed25519 signing key: 32 raw bytes as hex (same format as `collab`).
        #[arg(long)]
        key: PathBuf,
        /// Bearer token (default `$WALGIT_TOKEN`).
        #[arg(long)]
        token: Option<String>,
    },
    /// Rotate = re-register with a new key (overwrites; revocation history is
    /// not kept by the registry, only by the signed trail it verified).
    Rotate {
        #[arg(long)]
        url: String,
        #[arg(long)]
        principal: String,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        token: Option<String>,
    },
    /// List every host-registered principal (`principal -> public_key`).
    List {
        #[arg(long)]
        url: String,
        #[arg(long)]
        token: Option<String>,
    },
    /// Revoke the caller's own principal (DELETE); self only for now.
    Revoke {
        #[arg(long)]
        url: String,
        #[arg(long)]
        principal: String,
        #[arg(long)]
        token: Option<String>,
    },
}

pub async fn run(action: PrincipalAction) -> Result<()> {
    match action {
        PrincipalAction::Register {
            url,
            principal,
            key,
            token,
        } => put(&url, &principal, &key, token.as_deref(), "registered").await,
        PrincipalAction::Rotate {
            url,
            principal,
            key,
            token,
        } => put(&url, &principal, &key, token.as_deref(), "rotated").await,
        PrincipalAction::List { url, token } => list(&url, token.as_deref()).await,
        PrincipalAction::Revoke {
            url,
            principal,
            token,
        } => revoke(&url, &principal, token.as_deref()).await,
    }
}

/// Require a bearer for write commands; reads may be anonymous.
fn required_bearer(token: Option<&str>) -> Result<String> {
    token
        .map(str::to_string)
        .or_else(|| std::env::var("WALGIT_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
        .with_context(|| "no bearer token: pass --token or set WALGIT_TOKEN")
}

fn optional_bearer(token: Option<&str>) -> Option<String> {
    token
        .map(str::to_string)
        .or_else(|| std::env::var("WALGIT_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
}

async fn put(url: &str, principal: &str, key: &Path, token: Option<&str>, verb: &str) -> Result<()> {
    ref_segment("principal", principal)?;
    let token = required_bearer(token)?;
    let sk = read_signing_key(key)?;
    let public_key = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
    let resp = reqwest::Client::new()
        .put(format!("{url}/api/v1/principals/{principal}"))
        .bearer_auth(token)
        .json(&json!({ "public_key": public_key }))
        .send()
        .await
        .context("PUT host principal")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "PUT {url}/api/v1/principals/{principal} -> {status}: {text}"
    );
    println!("{verb} {principal} at {url}");
    Ok(())
}

async fn list(url: &str, token: Option<&str>) -> Result<()> {
    let token = optional_bearer(token);
    let mut req = reqwest::Client::new()
        .get(format!("{url}/api/v1/principals"))
        .header("Accept", "application/json");
    if let Some(t) = &token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("GET host principals")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "GET {url}/api/v1/principals -> {status}: {text}"
    );
    println!("{text}");
    Ok(())
}

async fn revoke(url: &str, principal: &str, token: Option<&str>) -> Result<()> {
    ref_segment("principal", principal)?;
    let token = required_bearer(token)?;
    let resp = reqwest::Client::new()
        .delete(format!("{url}/api/v1/principals/{principal}"))
        .bearer_auth(token)
        .send()
        .await
        .context("DELETE host principal")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "DELETE {url}/api/v1/principals/{principal} -> {status}: {text}"
    );
    println!("revoked {principal} at {url}");
    Ok(())
}
