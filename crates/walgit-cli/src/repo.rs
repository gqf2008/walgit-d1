//! `walgit repo create|list|info` — repository management.

use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::info;

use walgit_config::Config;
use walgit_git::ObjectFormat;
use walgit_store::open_store;
use walgit_wal::Registry;

use crate::cli::{parse_repo_id, println_kv};
use crate::{PolicyAction, RepoAction};

pub async fn run(action: RepoAction, cfg: &Arc<Config>) -> Result<()> {
    // Issue #61: the HTTP read surface runs against a running host — no
    // bucket credentials, no local cache dir. Everything else stays
    // store-direct (the maintaining-host shape).
    if let RepoAction::Refs { .. }
    | RepoAction::Ref { .. }
    | RepoAction::MergeBase { .. }
    | RepoAction::Diff { .. }
    | RepoAction::Blame { .. }
    | RepoAction::Archive { .. }
    | RepoAction::Ops { .. }
    | RepoAction::OpStart { .. }
    | RepoAction::Owners { .. }
    | RepoAction::Resolve { .. }
    | RepoAction::Tree { .. }
    | RepoAction::Blob { .. }
    | RepoAction::Commits { .. }
    | RepoAction::Commit { .. }
    | RepoAction::Overview { .. }
    | RepoAction::Tasks { .. } = action
    {
        return http::run(action).await;
    }
    let store = open_store(cfg).await?;
    std::fs::create_dir_all(&cfg.cache.dir).ok();
    let registry = Registry::new(store.clone(), cfg.clone());

    if let RepoAction::Settings { action } = action {
        return crate::settings_cmd::run(action, cfg).await;
    }
    match action {
        RepoAction::Create {
            repo,
            object_format,
        } => {
            let (owner, name) = parse_repo_id(&repo)?;
            let id = walgit_git::RepoId::new(owner, name)?;
            let format = match object_format.as_str() {
                "sha1" => ObjectFormat::Sha1,
                "sha256" => ObjectFormat::Sha256,
                other => bail!("unknown object format `{other}` (expected sha1 or sha256)"),
            };
            let handle = registry.create(&id, format).await?;
            let manifest = handle.manifest();
            println_kv("repo", &id);
            println_kv("object_format", &manifest.object_format);
            println_kv("head_seq", manifest.head_seq);
            info!(repo = %id, "repo created");
        }
        RepoAction::List => {
            let repos = registry.list().await?;
            if repos.is_empty() {
                println!("(no repositories)");
            } else {
                for id in repos {
                    println!("{id}");
                }
            }
        }
        RepoAction::Info { repo } => {
            let (owner, name) = parse_repo_id(&repo)?;
            let id = walgit_git::RepoId::new(owner, name)?;
            let handle = registry.open(&id).await?;
            handle.sync().await?;
            let manifest = handle.manifest();
            let version = handle
                .manifest_version()
                .map_or_else(|| "(none)".into(), |v| v.to_string());

            println_kv("repo", &id);
            println_kv("object_format", &manifest.object_format);
            println_kv("head_seq", manifest.head_seq);
            println_kv("min_seq", manifest.min_seq);
            println_kv("revision", manifest.revision);
            println_kv("manifest_version", &version);

            let packs = &manifest.packs;
            println_kv("packs", packs.len());
            let total_pack_bytes: u64 = packs.iter().map(|p| p.pack_size).sum();
            println_kv("pack_bytes", total_pack_bytes);

            if let Some(cp) = &manifest.checkpoint {
                println_kv("checkpoint_seq", cp.seq);
                println_kv("checkpoint_key", &cp.key);
            }

            let segments = &manifest.log_segments;
            println_kv("log_segments", segments.len());
            for seg in segments {
                println!(
                    "  {} [{},{}] {} bytes{}",
                    seg.key,
                    seg.first_seq,
                    seg.last_seq,
                    seg.size,
                    if seg.sealed { " (sealed)" } else { "" }
                );
            }
        }
        RepoAction::Policy { action } => policy(action, &store).await?,
        RepoAction::Settings { .. } => unreachable!(),
        // Host-backed reads never reach here: `repo::run` routes them to
        // `http::run` before the store is opened.
        RepoAction::Refs { .. }
        | RepoAction::Ref { .. }
        | RepoAction::MergeBase { .. }
        | RepoAction::Diff { .. }
        | RepoAction::Blame { .. }
        | RepoAction::Archive { .. }
        | RepoAction::Ops { .. }
        | RepoAction::OpStart { .. }
        | RepoAction::Owners { .. }
        | RepoAction::Resolve { .. }
        | RepoAction::Tree { .. }
        | RepoAction::Blob { .. }
        | RepoAction::Commits { .. }
        | RepoAction::Commit { .. }
        | RepoAction::Overview { .. }
        | RepoAction::Tasks { .. } => {
            bail!("internal: host-backed read reached the store path")
        }
    }
    Ok(())
}

async fn policy(action: PolicyAction, store: &walgit_store::DynStore) -> Result<()> {
    use walgit_server::policy::{self, RepoPolicy};
    match action {
        PolicyAction::Get { repo } => {
            let id = repo_id(&repo)?;
            let policy = policy::load(store, &id).await?;
            println!("{}", serde_json::to_string_pretty(&policy)?);
        }
        PolicyAction::Set { repo, file } => {
            let id = repo_id(&repo)?;
            let bytes = std::fs::read(&file)?;
            let doc: RepoPolicy = serde_json::from_slice(&bytes)?;
            policy::save(store, &id, &doc).await?;
            info!(repo = %id, "policy saved");
        }
        PolicyAction::Clear { repo } => {
            let id = repo_id(&repo)?;
            policy::clear(store, &id).await?;
            info!(repo = %id, "policy cleared");
        }
    }
    Ok(())
}

fn repo_id(repo: &str) -> Result<walgit_git::RepoId> {
    let (owner, name) = parse_repo_id(repo)?;
    Ok(walgit_git::RepoId::new(owner, name)?)
}

/// HTTP-backed `repo` reads (issue #61): typed requests against a running
/// walgit host — `--url`/`$WALGIT_URL`, bearer `--token`/`$WALGIT_TOKEN`.
/// The server holds the bucket; this process never touches it. Reads map
/// 1:1 onto the `/{owner}/{repo}/api-browser` surface (D15/D68) and print pretty JSON.
mod http {
    use std::fmt::Write as _;
    use std::io::Write as _;

    use anyhow::{Result, bail};
    use futures::StreamExt;
    use serde_json::Value;

    use crate::cli::parse_repo_id;
    use crate::{Conn, RepoAction};

    const DEFAULT_URL: &str = "http://127.0.0.1:8080";

    fn base(conn: &Conn) -> String {
        let url = conn
            .url
            .clone()
            .or_else(|| std::env::var("WALGIT_URL").ok())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        url.trim_end_matches('/').to_string()
    }

    /// Percent-encode a path segment; `keep_slash` lets tree paths carry `/`.
    /// Unreserved bytes (RFC 3986: alphanumerics and `-._~`) pass through.
    fn enc(s: &str, keep_slash: bool) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_' | b'.' | b'~')
                || (b == b'/' && keep_slash)
            {
                out.push(b as char);
            } else {
                // write! into a String never fails.
                let _ = write!(out, "%{b:02X}");
            }
        }
        out
    }

    fn api_path(repo: &str, suffix: &str) -> Result<String> {
        let (owner, name) = parse_repo_id(repo)?;
        // D68: one HTTP lane. The CLI is a host operator's shell, not the API —
        // it rides `api-browser` exactly like the SDK and the bundled UI.
        Ok(format!("/{owner}/{name}/api-browser{suffix}"))
    }

    async fn request(conn: &Conn, url: &str, accept_sse: bool) -> Result<reqwest::Response> {
        let client = reqwest::Client::new();
        let mut req = client.get(url).header(
            "Accept",
            if accept_sse {
                "application/json, text/event-stream"
            } else {
                "application/json"
            },
        );
        if let Some(token) = &conn.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.as_u16() == 401 {
            bail!(
                "HTTP 401 from {url} — this host requires a token; pass --token or set WALGIT_TOKEN"
            );
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("HTTP {status} from {url}\n{body}");
        }
        Ok(resp)
    }

    async fn print_json(conn: &Conn, path: &str) -> Result<()> {
        let url = format!("{}{}", base(conn), path);
        let resp = request(conn, &url, false).await?;
        let body = resp.text().await?;
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("{url} returned non-JSON: {e}\n{body}"))?;
        println!("{}", serde_json::to_string_pretty(&v)?);
        Ok(())
    }

    /// Follow one task's packet stream to its terminal `result`|`error`.
    async fn follow_task(conn: &Conn, repo: &str, id: &str) -> Result<()> {
        let path = api_path(repo, &format!("/tasks/{}", enc(id, false)))?;
        let url = format!("{}{}", base(conn), path);
        let resp = request(conn, &url, true).await?;
        drain_sse(resp).await
    }

    /// Read an SSE packet stream, pretty-printing each event; a terminal
    /// `error` event fails, `result` ends the stream cleanly.
    async fn drain_sse(resp: reqwest::Response) -> Result<()> {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let Some(chunk) = stream.next().await else {
                break;
            };
            buf.extend_from_slice(&chunk?);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(buf.get(..pos).unwrap_or_default()).to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                println!("{}", serde_json::to_string_pretty(&v)?);
                match v.get("kind").and_then(|k| k.as_str()) {
                    Some("error") => bail!("task failed:\n{data}"),
                    Some("result") => return Ok(()),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub async fn run(action: RepoAction) -> Result<()> {
        match action {
            RepoAction::Refs { repo, kind, conn } => match kind.as_deref() {
                None => print_json(&conn, &api_path(&repo, "/refs")?).await,
                Some(k @ ("branches" | "tags" | "all" | "collab")) => {
                    print_json(&conn, &api_path(&repo, &format!("/refs/{k}"))?).await
                }
                Some(other) => {
                    bail!("unknown ref kind `{other}` (expected branches, tags, all or collab)")
                }
            },
            RepoAction::Resolve { repo, rev, conn } => {
                print_json(
                    &conn,
                    &api_path(&repo, &format!("/resolve/{}", enc(&rev, true)))?,
                )
                .await
            }
            RepoAction::Tree {
                repo,
                rev,
                path,
                conn,
            } => {
                let suffix = if path.is_empty() {
                    format!("/tree/{}", enc(&rev, false))
                } else {
                    format!("/tree/{}/{}", enc(&rev, false), enc(&path, true))
                };
                print_json(&conn, &api_path(&repo, &suffix)?).await
            }
            RepoAction::Blob {
                repo,
                rev,
                path,
                raw,
                conn,
            } => {
                let suffix = format!(
                    "/blob/{}/{}{}",
                    enc(&rev, false),
                    enc(&path, true),
                    if raw { "?raw" } else { "" }
                );
                if raw {
                    let url = format!("{}{}", base(&conn), api_path(&repo, &suffix)?);
                    let bytes = request(&conn, &url, false).await?.bytes().await?;
                    std::io::stdout().write_all(&bytes)?;
                    Ok(())
                } else {
                    print_json(&conn, &api_path(&repo, &suffix)?).await
                }
            }
            RepoAction::Commits {
                repo,
                ref_,
                n,
                skip,
                path,
                conn,
            } => {
                let mut suffix = format!("/commits?n={}", n.unwrap_or(35));
                if let Some(r) = ref_ {
                    let _ = write!(suffix, "&ref={}", enc(&r, true));
                }
                if let Some(s) = skip {
                    let _ = write!(suffix, "&skip={s}");
                }
                if let Some(p) = path {
                    let _ = write!(suffix, "&path={}", enc(&p, true));
                }
                print_json(&conn, &api_path(&repo, &suffix)?).await
            }
            RepoAction::Commit { repo, sha, conn } => {
                print_json(
                    &conn,
                    &api_path(&repo, &format!("/commit/{}", enc(&sha, false)))?,
                )
                .await
            }
            RepoAction::Overview { repo, conn } => {
                print_json(&conn, &api_path(&repo, "/overview")?).await
            }
            RepoAction::Tasks { repo, follow, conn } => match follow {
                Some(id) => follow_task(&conn, &repo, &id).await,
                None => print_json(&conn, &api_path(&repo, "/tasks")?).await,
            },
            RepoAction::Ref { repo, name, conn } => {
                print_json(
                    &conn,
                    &api_path(&repo, &format!("/refs/name/{}", enc(&name, true)))?,
                )
                .await
            }
            RepoAction::MergeBase {
                repo,
                from,
                to,
                conn,
            } => {
                let suffix = format!(
                    "/merge-base?from={}&to={}",
                    enc(&from, true),
                    enc(&to, true)
                );
                print_json(&conn, &api_path(&repo, &suffix)?).await
            }
            RepoAction::Diff {
                repo,
                from,
                to,
                format,
                conn,
            } => {
                let mut suffix = format!("/diff?from={}&to={}", enc(&from, true), enc(&to, true));
                if let Some(f) = format {
                    let _ = write!(suffix, "&format={}", enc(&f, true));
                }
                print_json(&conn, &api_path(&repo, &suffix)?).await
            }
            RepoAction::Blame {
                repo,
                rev,
                path,
                conn,
            } => {
                let suffix = format!("/blame/{}/{}", enc(&rev, false), enc(&path, true));
                print_json(&conn, &api_path(&repo, &suffix)?).await
            }
            RepoAction::Archive {
                repo,
                rev,
                format,
                out,
                conn,
            } => {
                let mut suffix = format!("/archive/{}", enc(&rev, false));
                if let Some(f) = format {
                    let _ = write!(suffix, "?format={}", enc(&f, true));
                }
                let url = format!("{}{}", base(&conn), api_path(&repo, &suffix)?);
                let bytes = request(&conn, &url, false).await?.bytes().await?;
                match out {
                    Some(file) => std::fs::write(file, &bytes)?,
                    None => std::io::stdout().write_all(&bytes)?,
                }
                Ok(())
            }
            RepoAction::Ops { repo, conn } => print_json(&conn, &api_path(&repo, "/ops")?).await,
            RepoAction::OpStart {
                repo,
                op,
                args,
                conn,
            } => {
                let mut suffix = format!("/ops/{}", enc(&op, false));
                for (i, a) in args.iter().enumerate() {
                    let (k, v) = a
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--arg must be `k=v`, got `{a}`"))?;
                    let _ = write!(
                        suffix,
                        "{}{}={}",
                        if i == 0 { '?' } else { '&' },
                        enc(k, true),
                        enc(v, true)
                    );
                }
                let url = format!("{}{}", base(&conn), api_path(&repo, &suffix)?);
                let resp = request(&conn, &url, true).await?;
                // Ops answer like tasks: SSE packets, or plain JSON when fast.
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                if content_type.contains("text/event-stream") {
                    drain_sse(resp).await
                } else {
                    let body = resp.text().await?;
                    let v: Value = serde_json::from_str(&body)
                        .map_err(|e| anyhow::anyhow!("{url} returned non-JSON: {e}\n{body}"))?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(())
                }
            }
            RepoAction::Owners { owner, conn } => match owner {
                None => print_json(&conn, "/api/v1/owners").await,
                Some(o) => {
                    print_json(&conn, &format!("/api/v1/owners/{}/repos", enc(&o, false))).await
                }
            },
            // repo::run filters the store-backed actions out before calling
            // here; an anyhow message instead of a panic keeps the binary
            // honest even if that filter ever drifts.
            _ => bail!("internal: not a host-backed repo read"),
        }
    }
}
