//! `walgit collab` — the D1 collaboration layer (`docs/D1_COLLAB_DESIGN.md`).
//!
//! Deterministic aggregation over `refs/collab/*` (§4.3): every client that
//! reads the same refs and verifies the same signatures computes the same
//! `thread` / `pr` / `merge_rule_eval` answer. The read commands run against a
//! local git checkout that has the collab refs (clone/fetch them), so no
//! server API is involved — this is the "anyone can verify locally" property.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use walgit_git::ObjectFormat;
use std::fmt::Write as _;
use std::io::Write as _;
use walgit_wal::collab::{
    BOARD_PATH, Board, BoardDef, Entry, EntryRef, EntryRefs, EntrySet, MergeRules, Report,
    SNAPSHOT_REF, SnapshotRecord, build_board, build_report, build_snapshot, default_board,
    merge_rule_eval, parse_board_def, parse_snapshot, pr_view, sign_entry, thread,
    verify_snapshot,
};

// ---- CLI commands --------------------------------------------------------------

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI 参数枚举,进程一次构建,无热路径
pub enum CollabAction {
    /// List distinct thread ids found in `refs/collab/inbox/*`.
    Ls {
        /// Local git checkout that has the collab refs.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Print one thread (parent-ordered entries, per-entry verification) as JSON.
    Thread {
        id: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Print the aggregated PR view + merge rule evaluation as JSON.
    Pr {
        id: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Merge-rules JSON file (`{"protect":["refs/heads/main"],...}`).
        #[arg(long)]
        rules: Option<PathBuf>,
    },
    /// Construct + sign + deliver a collab entry (§4.2). Writes the inbox ref
    /// locally; `--push <remote>` additionally pushes it to a walgit server.
    #[allow(clippy::large_enum_variant)] // CLI 参数结构,进程一次构建,无热路径
    Entry {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote to push the ref to (omit for local-only writes).
        #[arg(long)]
        push: Option<String>,
        #[arg(long)]
        kind: String,
        /// Thread id (shared by every entry of the thread).
        #[arg(long)]
        id: String,
        /// Principal whose inbox receives the entry (refname-safe).
        #[arg(long)]
        actor: String,
        /// Previous entry's oid in the thread, or empty for the root.
        #[arg(long, default_value = "")]
        parent: String,
        /// Entry body as JSON.
        #[arg(long)]
        body: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        head: Option<String>,
        /// Ed25519 signing key: 32 raw bytes as hex.
        #[arg(long)]
        key: PathBuf,
        /// Entry oid this entry relates to (repeatable; issue #75 ③).
        #[arg(long = "related")]
        related: Vec<String>,
        /// Entry oid this entry depends on (repeatable; issue #75 ③).
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        /// File to attach: `{filename, sha256, content_b64}` embedded in the
        /// body (repeatable; issue #75 ④). Hard cap 64 KiB per file.
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
    },
    /// First-use registration of a principal's public key at
    /// `refs/collab/meta/principals/<principal>` (D1 §5).
    PrincipalRegister {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        principal: String,
        /// The signing key seed; the public key is derived from it.
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        push: Option<String>,
    },
    /// Revoke a principal's key: delete the registry ref (tombstone).
    PrincipalRevoke {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        principal: String,
        #[arg(long)]
        push: Option<String>,
    },
    /// Fetch the host-global principal registry (issue #76) from the checkout's
    /// origin host and cache it under `refs/walgit/principals/*` locally — after
    /// this, repo B verifies a principal registered in repo A with no further
    /// network access (offline-verifiable cache).
    PrincipalFetch {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote whose URL names the walgit host (default `origin`).
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Bearer token for the host (default `$WALGIT_TOKEN`).
        #[arg(long)]
        token: Option<String>,
    },
    /// Read-only observability dashboard (D1 §8): aggregate all collab state
    /// into a summary — threads, PR status, verification health, activity.
    Report {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Output format: text (default), markdown, html.
        #[arg(long, default_value = "text")]
        format: String,
        /// Merge-rules JSON file (same shape as `pr --rules`).
        #[arg(long)]
        rules: Option<PathBuf>,
    },
    /// The work-unit board (D1 §8): the threads projected under the board
    /// definition at `.walgit/board.toml` (HEAD). Read-only: moving a card is
    /// an ordinary signed `status` entry (`collab entry --kind status`).
    Board {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Output format: text (default), markdown, json.
        #[arg(long, default_value = "text")]
        format: String,
        /// Board definition override — previews an uncommitted
        /// `.walgit/board.toml` (default: the one at HEAD, else the built-in
        /// default board).
        #[arg(long)]
        board: Option<PathBuf>,
        /// Merge-rules JSON file (same shape as `pr --rules`).
        #[arg(long)]
        rules: Option<PathBuf>,
    },
    /// Fold the append-only inbox into the signed aggregate snapshot
    /// (D45 / D1 §11.4): `refs/collab/meta/snapshot` moves first, then the
    /// folded inbox refs are deleted. Aggregation reads snapshot ∪ tail and
    /// is byte-identical across the fold. Idempotent; safe to re-run.
    Gc {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Principal recorded as the folder (its key signs the snapshot).
        #[arg(long)]
        actor: String,
        /// Ed25519 signing key of the folder: 32 raw bytes as hex.
        #[arg(long)]
        key: PathBuf,
        /// Remote to push the fold to (omit for a local-only fold).
        #[arg(long)]
        push: Option<String>,
    },
    /// Resident watcher: fetch `refs/collab/*` from a remote, report new or
    /// changed refs, and invoke `--exec` for each with the entry JSON on
    /// stdin (the agent's decision logic; walgit only does notify+sync).
    Watch {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote to fetch collab refs from.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Seconds between passes (default 10).
        #[arg(long, default_value_t = 10)]
        interval: u64,
        /// Run a single pass and exit (tests/CI).
        #[arg(long)]
        once: bool,
        /// Command run for each new/changed ref (`sh -c`); entry JSON on stdin.
        #[arg(long)]
        exec: Option<String>,
        /// State file override (default `<gitdir>/collab-watch.json`).
        #[arg(long)]
        state: Option<PathBuf>,
    },
}

pub async fn run(action: CollabAction) -> Result<()> {
    match action {
        CollabAction::Ls { repo } => {
            let reader = CollabReader::new(&repo);
            let (entries, _) = reader.load()?;
            let mut ids: Vec<&str> = entries.iter().map(|e| e.entry.id.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            for id in ids {
                println!("{id}");
            }
        }
        CollabAction::Thread { id, repo } => {
            let reader = CollabReader::new(&repo);
            let (entries, principals) = reader.load()?;
            let filtered: Vec<&EntryRef> = entries.iter().filter(|e| e.entry.id == id).collect();
            if filtered.is_empty() {
                bail!("no entries for thread {id}");
            }
            let ordered = thread(&filtered);
            let out: Vec<serde_json::Value> = ordered
                .iter()
                .map(|r| {
                    let verified = r.is_verified(&principals);
                    serde_json::json!({ "oid": r.oid, "principal": r.principal, "verified": verified, "entry": r.entry })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        CollabAction::Entry {
            repo,
            push,
            kind,
            id,
            actor,
            parent,
            body,
            base,
            head,
            key,
            related,
            depends_on,
            attach,
        } => run_entry(&EntryArgs {
            repo,
            push,
            kind,
            id,
            actor,
            parent,
            body,
            base,
            head,
            key,
            related,
            depends_on,
            attach,
        })?,
        CollabAction::PrincipalRegister {
            repo,
            principal,
            key,
            push,
        } => run_principal_register(&repo, &principal, &key, push.as_deref())?,
        CollabAction::PrincipalRevoke {
            repo,
            principal,
            push,
        } => run_principal_revoke(&repo, &principal, push.as_deref())?,
        CollabAction::Gc { repo, actor, key, push } => {
            run_gc(&repo, &actor, &key, push.as_deref())?;
        }
        CollabAction::PrincipalFetch {
            repo,
            remote,
            token,
        } => run_principal_fetch(&repo, &remote, token.as_deref()).await?,
        CollabAction::Report {
            repo,
            format,
            rules,
        } => run_report(&repo, &format, rules.as_deref())?,
        CollabAction::Board {
            repo,
            format,
            board,
            rules,
        } => run_board(&repo, &format, board.as_deref(), rules.as_deref())?,
        CollabAction::Watch {
            repo,
            remote,
            interval,
            once,
            exec,
            state,
        } => run_watch(
            &repo,
            &remote,
            interval,
            once,
            exec.as_deref(),
            state.as_deref(),
        )?,
        CollabAction::Pr { id, repo, rules } => {
            let reader = CollabReader::new(&repo);
            let (entries, principals) = reader.load()?;
            let filtered: Vec<&EntryRef> = entries.iter().filter(|e| e.entry.id == id).collect();
            let pr = pr_view(&filtered, &principals);
            let rules: MergeRules = match rules {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(&p)?)?,
                None => MergeRules::default(),
            };
            let eval = merge_rule_eval(&rules, &pr);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "pr": pr, "merge": eval }))?
            );
        }
    }
    Ok(())
}

pub(crate) fn ref_segment(label: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 255
        && !s.contains("..") // git forbids `..` inside a component
        && s != "."
        && s != ".."
        && !s.to_ascii_lowercase().ends_with(".lock")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        bail!("collab.{label}: {s:?} is not a refname-safe segment ([A-Za-z0-9._@-]+)")
    }
}

pub(crate) fn read_signing_key(path: &std::path::Path) -> Result<SigningKey> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read key {}", path.display()))?;
    let bytes = hex::decode(raw.trim())
        .with_context(|| format!("key {} must be 32 raw bytes as hex", path.display()))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("key {} must be 32 bytes", path.display()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub(crate) fn entry_uuid() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut b = [0u8; 16];
    rng.fill(&mut b);
    hex::encode(b)
}

/// Write a blob via `git hash-object -w --stdin`; returns the oid.
pub(crate) fn git_write_blob(repo: &std::path::Path, content: &str) -> Result<String> {
    git_write_blob_bytes(repo, content.as_bytes())
}

/// Binary-safe variant (D1-CI §8.2 artifact/log bytes are not text).
pub(crate) fn git_write_blob_bytes(repo: &std::path::Path, content: &[u8]) -> Result<String> {
    let mut child = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("spawn git hash-object")?;
    child
        .stdin
        .take()
        .context("git hash-object stdin")?
        .write_all(content)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn git_update_ref(repo: &std::path::Path, name: &str, oid: Option<&str>) -> Result<()> {
    let args: Vec<&str> = if let Some(oid) = oid {
        vec!["update-ref", name, oid]
    } else {
        vec!["update-ref", "-d", name]
    };
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(&args)
        .output()
        .context("git update-ref")?;
    if !out.status.success() {
        bail!(
            "git update-ref {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn git_push(repo: &std::path::Path, remote: &str, name: &str) -> Result<()> {
    git_push_many(repo, remote, &[name])
}

/// One push for several refs (D1-CI §8.2: a result's artifact batch travels
/// as a single receive-pack round trip).
pub(crate) fn git_push_many(repo: &std::path::Path, remote: &str, names: &[&str]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .arg("push")
        .arg(remote)
        .args(names)
        .output()
        .context("git push")?;
    if !out.status.success() {
        bail!(
            "git push {remote} ({} refs) failed: {}",
            names.len(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

struct EntryArgs {
    repo: std::path::PathBuf,
    push: Option<String>,
    kind: String,
    id: String,
    actor: String,
    parent: String,
    body: String,
    base: Option<String>,
    head: Option<String>,
    key: std::path::PathBuf,
    /// Entry oids this entry relates to (`--related`, repeatable).
    related: Vec<String>,
    /// Entry oids this entry depends on (`--depends-on`, repeatable).
    depends_on: Vec<String>,
    /// Files to attach: sha256 + base64 content embedded in `body.attachments`
    /// (`--attach`, repeatable; issue #75 ④).
    attach: Vec<std::path::PathBuf>,
}

/// The client-side transition gate (issue #102/#104): identical to the
/// server's — `status=done` requires the thread (parent-chain ordered, never
/// refs-collection order) to be at needs-review with a verified approve
/// review. Extractable so the rejection path is unit-testable without a repo.
fn check_status_transition(
    entry: &Entry,
    thread_entries: &[EntryRef],
    principals: &HashMap<String, String>,
) -> Result<()> {
    if entry.kind == "status" && entry.body.get("status").and_then(|v| v.as_str()) == Some("done") {
        let thread_refs: Vec<&EntryRef> = thread_entries
            .iter()
            .filter(|e| e.entry.id == entry.id)
            .collect();
        // 线程序先行(issue #104 观察 A):card_status 按给定顺序重放。
        let thread_refs = walgit_wal::collab::thread(&thread_refs);
        walgit_wal::collab::validate_status_transition(&thread_refs, principals).map_err(|e| {
            // 观察 B:仅当缺 verified approve(可能只在 host registry)时才指路
            // fetch——「当前状态不是 needs-review」fetch 也救不了,不误导。
            if e.contains("verified approve") {
                anyhow::anyhow!(
                    "status transition rejected: {e}\n提示:approve reviewer 的 key 若仅在 host registry,先 `walgit collab principal-fetch` 再试"
                )
            } else {
                anyhow::anyhow!("status transition rejected: {e}")
            }
        })?;
    }
    Ok(())
}

fn run_entry(args: &EntryArgs) -> Result<()> {
    ref_segment("entry.actor", &args.actor)?;
    let mut body: serde_json::Value = serde_json::from_str(&args.body)
        .with_context(|| format!("--body must be JSON: {}", args.body))?;
    // Structured cross-thread references (issue #75 ③): validated against the
    // collab state at aggregation time (thread view reports broken oids).
    let obj = body.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("--body must be a JSON object to attach related/depends-on")
    })?;
    if !args.related.is_empty() {
        obj.insert(
            "related".into(),
            serde_json::Value::Array(
                args.related.iter().map(|o| serde_json::Value::String(o.clone())).collect(),
            ),
        );
    }
    if !args.depends_on.is_empty() {
        obj.insert(
            "depends_on".into(),
            serde_json::Value::Array(
                args.depends_on.iter().map(|o| serde_json::Value::String(o.clone())).collect(),
            ),
        );
    }
    // Attachments (issue #75 ④): `{filename, sha256, content_b64}` — the
    // thread is self-contained and the reader verifies the digest.
    if !args.attach.is_empty() {
        use base64::Engine;
        use sha2::Digest;
        const MAX_ATTACH_BYTES: u64 = 64 * 1024;
        let mut attachments = Vec::new();
        for path in &args.attach {
            let bytes = std::fs::read(path)
                .with_context(|| format!("--attach {}: read failed", path.display()))?;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_ATTACH_BYTES,
                "--attach {}: {} bytes exceeds the {} KiB per-file cap",
                path.display(),
                bytes.len(),
                MAX_ATTACH_BYTES / 1024
            );
            let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
            attachments.push(serde_json::json!({
                "filename": path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
                "sha256": digest,
                "content_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            }));
        }
        obj.insert("attachments".into(), serde_json::Value::Array(attachments));
    }
    let mut entry = Entry {
        version: 1,
        kind: args.kind.clone(),
        id: args.id.clone(),
        actor: args.actor.clone(),
        ts: chrono::Utc::now().timestamp(),
        parent: args.parent.clone(),
        refs: match (&args.base, &args.head) {
            (None, None) => None,
            (b, h) => Some(EntryRefs {
                base: b.clone(),
                head: h.clone(),
            }),
        },
        body,
        sig: String::new(),
    };
    // Transition 门禁（issue #102/#104）：与服务端一致,可测 helper。
    // load 只在 done 分支执行——收件箱里他人坏条目/缺对象不该阻断
    // issue/comment 等普通写(#114 审查回归修正),也避免每写一次 O(N)
    // git cat-file。
    if entry.kind == "status"
        && entry.body.get("status").and_then(|v| v.as_str()) == Some("done")
    {
        let (thread_entries, principals) = CollabReader::new(&args.repo).load()?;
        check_status_transition(&entry, &thread_entries, &principals)?;
    }
    let key = read_signing_key(&args.key)?;
    entry.sig = sign_entry(&mut entry, &key);
    let content = serde_json::to_string_pretty(&entry)?;
    let oid = git_write_blob(&args.repo, &content)?;
    let ref_name = format!("refs/collab/inbox/{}/{}", args.actor, entry_uuid());
    git_update_ref(&args.repo, &ref_name, Some(&oid))?;
    if let Some(remote) = &args.push {
        git_push(&args.repo, remote, &ref_name)?;
    }
    println!("{ref_name} {oid}");
    Ok(())
}

fn run_principal_register(
    repo: &Path,
    principal: &str,
    key_path: &Path,
    push: Option<&str>,
) -> Result<()> {
    ref_segment("principal", principal)?;
    let key = read_signing_key(key_path)?;
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
    let content = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "principal": principal,
        "public_key": public_key,
        "registered_at": chrono::Utc::now().timestamp(),
    }))?;
    let oid = git_write_blob(repo, &content)?;
    let ref_name = format!("refs/collab/meta/principals/{principal}");
    git_update_ref(repo, &ref_name, Some(&oid))?;
    if let Some(remote) = push {
        git_push(repo, remote, &ref_name)?;
    }
    println!("{ref_name} {oid}");
    Ok(())
}

/// `collab principal-fetch`: pull the host-global registry and cache it as local
/// refs under `refs/walgit/principals/*` (read by `CollabReader::principals`).
async fn run_principal_fetch(repo: &Path, remote: &str, token: Option<&str>) -> Result<()> {
    let url = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["remote", "get-url", remote])
        .output()
        .context("git remote get-url")?;
    anyhow::ensure!(
        url.status.success(),
        "git remote get-url failed: {}",
        String::from_utf8_lossy(&url.stderr).trim()
    );
    let remote_url = String::from_utf8_lossy(&url.stdout).trim().to_string();
    let root = host_root(&remote_url)?;
    let token = token
        .map(str::to_string)
        .or_else(|| std::env::var("WALGIT_TOKEN").ok())
        .filter(|t| !t.trim().is_empty());
    let mut req = reqwest::Client::new()
        .get(format!("{root}/api/v1/principals"))
        .header("Accept", "application/json");
    if let Some(t) = &token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.context("GET host principals")?;
    let status = resp.status();
    let text = resp.text().await.context("reading host principals")?;
    anyhow::ensure!(
        status.is_success(),
        "GET {root}/api/v1/principals -> {status}: {text}"
    );
    let body = text;
    let map: HashMap<String, String> = serde_json::from_str(&body)?;

    // Purge cached principals that the host no longer lists (revoked/rotated
    // away) — otherwise the offline cache would keep trusting a revoked key.
    let existing = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/walgit/principals",
        ])
        .output()
        .context("git for-each-ref refs/walgit/principals")?;
    anyhow::ensure!(
        existing.status.success(),
        "git for-each-ref failed: {}",
        String::from_utf8_lossy(&existing.stderr).trim()
    );
    for name in String::from_utf8_lossy(&existing.stdout).lines() {
        let Some(principal) = name.strip_prefix("refs/walgit/principals/") else {
            continue;
        };
        if !map.contains_key(principal) {
            git_update_ref(repo, name, None)?;
        }
    }

    let mut n = 0;
    for (principal, public_key) in map {
        ref_segment("principal", &principal)?;
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "principal": principal,
            "public_key": public_key,
            "registered_at": 0,
        }))?;
        let oid = git_write_blob(repo, &content)?;
        let ref_name = format!("refs/walgit/principals/{principal}");
        git_update_ref(repo, &ref_name, Some(&oid))?;
        n += 1;
    }
    println!("cached {n} host principal(s) under refs/walgit/principals/*");
    Ok(())
}

/// `https://host[:port]/owner/repo[.git]` → `https://host[:port]`.
fn host_root(remote: &str) -> Result<String> {
    let without_git = remote.strip_suffix(".git").unwrap_or(remote);
    let (scheme, rest) = without_git
        .split_once("://")
        .with_context(|| format!("remote URL {remote:?} has no scheme"))?;
    let slash = rest.find('/').unwrap_or(rest.len());
    Ok(format!("{scheme}://{}", rest.get(..slash).unwrap_or(rest)))
}

fn run_principal_revoke(repo: &Path, principal: &str, push: Option<&str>) -> Result<()> {
    ref_segment("principal", principal)?;
    let ref_name = format!("refs/collab/meta/principals/{principal}");
    git_update_ref(repo, &ref_name, None)?;
    if let Some(remote) = push {
        git_push(repo, remote, &ref_name)?;
    }
    println!("{ref_name} revoked");
    Ok(())
}

// ---- gc: fold the inbox into the signed snapshot (D45 / D1 §11.4) -------------

/// Delete refspecs per push call — keeps argv far below `ARG_MAX` even for a
/// 20k-ref fold. Non-atomic batches: inbox refs never move, so a delete either
/// matches or was already done; a rejected batch is converged by re-running.
const GC_DELETE_CHUNK: usize = 500;

fn git_push_refspecs(
    repo: &Path,
    remote: &str,
    refspecs: &[String],
    atomic: bool,
    leases: &[String],
) -> Result<()> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["-C"]).arg(repo).arg("push");
    if atomic {
        cmd.arg("--atomic");
    }
    // A `+` refspec prefix would silently short-circuit the lease — a forced
    // update is expressed by the lease (or not at all), never by `+`.
    for lease in leases {
        cmd.arg(format!("--force-with-lease={lease}"));
    }
    cmd.arg(remote).args(refspecs);
    let out = cmd.output().context("git push")?;
    if !out.status.success() {
        bail!(
            "git push {remote} ({} refspec(s)) failed: {}",
            refspecs.len(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The refnames a remote currently advertises under the collab inbox — what
/// the gc's delete batches are filtered against. A concurrent gc may have
/// pruned some already, and stock git refuses a delete whose ref is not
/// advertised (`unable to delete …: remote ref does not exist`): that refusal
/// would break the fold's "re-run to converge" promise, so the gc never asks
/// git to delete a ref it has not just seen. The full advertisement is read
/// and filtered here rather than a `refs/collab/inbox/*` pattern: one round
/// trip either way (every push below re-fetches the same advertisement), and
/// the pruning must not depend on ls-remote's glob semantics.
fn git_ls_remote_inbox(repo: &Path, remote: &str) -> Result<HashMap<String, String>> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["ls-remote", remote])
        .output()
        .context("git ls-remote")?;
    if !out.status.success() {
        bail!(
            "git ls-remote {remote} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(oid, name)| (name.trim().to_string(), oid.to_string()))
        .filter(|(name, _)| name.starts_with("refs/collab/inbox/"))
        .collect())
}

/// Whether the seed about to sign a fold is the public key registered for its
/// actor. A mismatched pair would publish a snapshot whose actor cannot verify
/// it, stranding every reader with an apparently malicious fold.
fn signing_key_matches_registered(key: &SigningKey, registered_public_key: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(registered_public_key.trim())
        .is_ok_and(|bytes| bytes.as_slice() == key.verifying_key().as_bytes())
}

/// Record one entry in a snapshot fold, preferring the copy whose inbox
/// principal is the entry's own actor when the same oid appears under multiple
/// refs. Returns whether `records` changed and therefore needs a new snapshot;
/// a duplicate that adds no better copy is prune-only.
fn keep_fold_record(
    records: &mut Vec<SnapshotRecord>,
    seen: &mut HashMap<String, usize>,
    record: SnapshotRecord,
    entry_actor: &str,
    format: ObjectFormat,
) -> bool {
    if let Some(&index) = seen.get(&record.oid) {
        let current = records.get(index);
        let replace = current.is_some_and(|cur| {
            cur.entry_ref(format).is_none()
                || (cur.principal != entry_actor && record.principal == entry_actor)
        });
        if replace && let Some(slot) = records.get_mut(index) {
            *slot = record;
        }
        return replace;
    }
    seen.insert(record.oid.clone(), records.len());
    records.push(record);
    true
}

/// `collab gc`: fold every parseable inbox entry into the signed snapshot at
/// `refs/collab/meta/snapshot` and prune the folded refs. The new snapshot
/// composes the existing one (records carried verbatim — an oid addresses the
/// original bytes, so records are never re-serialized) with the current tail.
/// Unparseable inbox blobs are left in place (the read side skips them too).
/// A fold that records nothing new (every local inbox ref is a duplicate the
/// snapshot already carries — a crashed or raced gc's un-pruned tail) is
/// prune-only: the snapshot is never rebuilt, because a rebuild would differ
/// only in `ts` — pure ref churn.
///
/// Push order is the safety boundary: the snapshot lands first — a CAS from
/// the baseline this gc actually read (`--force-with-lease=ref:baseline`; a
/// concurrent fold that already moved the ref fails the lease and the gc
/// retries, never overwrites) — then the folded refs are deleted in batches,
/// and only the ones the remote still advertises (stock git refuses a delete
/// whose ref is missing; another gc may have pruned it mid-flight). A crash
/// or a reader mid-fold sees duplicates, never a loss; the read side dedups
/// by oid.
fn run_gc(repo: &Path, actor: &str, key_path: &Path, push: Option<&str>) -> Result<()> {
    ref_segment("gc.actor", actor)?;
    let reader = CollabReader::new(repo);
    let format = reader.object_format()?;
    // The snapshot's own signature is only verifiable against a *registered*
    // key (§4.2) — folding as an unregistered principal would strand every
    // reader with an unverifiable snapshot.
    let principals = reader.principals()?;
    let Some(registered_public_key) = principals.get(actor) else {
        bail!(
            "gc.actor {actor} has no registered collab key in this repository; \
             `walgit collab principal-register --principal {actor} --key <keyfile>` first"
        );
    };
    let key = read_signing_key(key_path)?;
    if !signing_key_matches_registered(&key, registered_public_key) {
        bail!(
            "gc.key does not match the public key registered for gc.actor {actor}; \
             use the key registered for this principal or rotate it explicitly"
        );
    }
    // The baseline the snapshot push leases against: the snapshot ref's value
    // as this gc read it (§11.4). `None` is `--force-with-lease=ref:` (an
    // explicitly empty <expect>), Git's portable "the ref must not exist";
    // the zero OID is not equivalent on current Git.
    let (baseline, mut records): (Option<String>, Vec<SnapshotRecord>) =
        match reader.snapshot_blob()? {
            Some((oid, bytes)) => (
                Some(oid),
                parse_snapshot(&bytes)
                    .map_err(|e| anyhow::anyhow!("{SNAPSHOT_REF}: {e}"))?
                    .entries,
            ),
            None => (None, Vec::new()),
        };
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        seen.entry(record.oid.clone()).or_insert(index);
    }
    let mut folded: Vec<String> = Vec::new(); // inbox ref names to prune
    let mut changed = false; // a new or better record ⇒ rebuild the snapshot
    let mut left = 0usize; // unparseable blobs left in place
    for (name, oid) in reader.inbox_refs()? {
        let blob = reader.git(&["cat-file", "blob", &oid])?;
        let Ok(json) = String::from_utf8(blob) else {
            left += 1;
            continue;
        };
        let Ok(entry) = serde_json::from_str::<Entry>(&json) else {
            left += 1;
            continue;
        };
        let Some(principal) = name
            .strip_prefix("refs/collab/inbox/")
            .and_then(|p| p.rsplit_once('/'))
            .map(|(p, _)| p.to_string())
        else {
            left += 1;
            continue;
        };
        // Already folded (an earlier gc crashed mid-prune, or a concurrent gc
        // raced this checkout's lagging fetch): prune the duplicate without
        // re-recording it, unless this copy is the actor-owned one and the
        // existing record names a planted inbox.
        changed |= keep_fold_record(
            &mut records,
            &mut seen,
            SnapshotRecord { oid, principal, json },
            &entry.actor,
            format,
        );
        folded.push(name);
    }
    if folded.is_empty() {
        println!("collab gc: nothing to fold ({left} unparseable inbox blob(s) left in place)");
        return Ok(());
    }
    // Nothing new to record ⇒ prune-only fold: the snapshot the baseline
    // names already carries every entry, so it is never rebuilt (a rebuild
    // would differ only in `ts` — pure ref churn).
    let snap_oid = if changed {
        let snap = build_snapshot(actor, chrono::Utc::now().timestamp(), records, &key);
        Some(git_write_blob(repo, &serde_json::to_string(&snap)?)?)
    } else {
        None
    };
    match push {
        None => {
            if let Some(oid) = &snap_oid {
                git_update_ref(repo, SNAPSHOT_REF, Some(oid))?;
            }
            for name in &folded {
                git_update_ref(repo, name, None)?;
            }
        }
        Some(remote) => {
            // Snapshot first (a CAS from the baseline this gc actually read),
            // then the deletes. A concurrent fold that moved the snapshot in
            // the meantime fails the lease — fetch and retry, never overwrite.
            // A prune-only fold still pushes the baseline snapshot so the
            // target is proven to hold the history before any inbox deletion;
            // when it already does this is a harmless no-op.
            let snapshot_to_push = snap_oid.as_deref().or(baseline.as_deref());
            let snapshot_to_push = snapshot_to_push.context(
                "cannot prune a remote without a snapshot: fetch the remote and retry",
            )?;
            let lease = format!(
                "{SNAPSHOT_REF}:{}",
                baseline.as_deref().unwrap_or_default()
            );
            git_push_refspecs(
                repo,
                remote,
                &[format!("{snapshot_to_push}:{SNAPSHOT_REF}")],
                false,
                std::slice::from_ref(&lease),
            )
            .context(
                "push the snapshot (the fold baseline moved — a concurrent gc? fetch and retry)",
            )?;
            // Prune only refs the remote still advertises: a concurrent gc
            // may have pruned some already, and stock git refuses a delete
            // whose ref is not advertised — that refusal is convergence, not
            // an error. Stale local copies are cleaned below either way.
            let live = git_ls_remote_inbox(repo, remote)?;
            let to_delete: Vec<(&String, &String)> = folded
                .iter()
                .filter_map(|name| live.get(name).map(|oid| (name, oid)))
                .collect();
            for chunk in to_delete.chunks(GC_DELETE_CHUNK) {
                let specs: Vec<String> = chunk.iter().map(|(name, _)| format!(":{name}")).collect();
                let leases: Vec<String> = chunk
                    .iter()
                    .map(|(name, oid)| format!("{name}:{oid}"))
                    .collect();
                git_push_refspecs(repo, remote, &specs, false, &leases).context(
                    "delete folded inbox refs; re-run `walgit collab gc` to converge (idempotent)",
                )?;
            }
            // Mirror the fold locally so this checkout aggregates the folded
            // shape immediately.
            if let Some(oid) = &snap_oid {
                git_update_ref(repo, SNAPSHOT_REF, Some(oid))?;
            }
            for name in &folded {
                git_update_ref(repo, name, None)?;
            }
        }
    }
    println!(
        "collab gc: folded {} inbox ref(s) into {SNAPSHOT_REF} ({}); {left} unparseable left in place",
        folded.len(),
        snap_oid
            .as_deref()
            .unwrap_or_else(|| baseline.as_deref().unwrap_or("new")),
    );
    Ok(())
}

/// What a work unit is called on a list: its title, falling back to the
/// thread id when it has none (issue #131 — the board cards and the report
/// lists label the same way).
fn unit_label<'a>(title: &'a str, id: &'a str) -> &'a str {
    if title.is_empty() {
        id
    } else {
        title
    }
}

fn render_report_text(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "collab report: {} threads, {} PRs, {}/{} entries verified\n",
        r.threads.len(),
        r.prs.len(),
        r.verified_entries,
        r.total_entries
    );
    let _ = writeln!(out, "threads");
    for t in &r.threads {
        let _ = writeln!(
            out,
            "  {}: {} entries ({} verified), kinds {}, last {}",
            unit_label(&t.title, &t.id),
            t.entries,
            t.verified,
            t.kinds.join("/"),
            t.last_ts
        );
    }
    let _ = writeln!(out, "\nprs");
    for p in &r.prs {
        let _ = writeln!(
            out,
            "  {} [{}] approvals={} merge_allowed={} ({})",
            unit_label(&p.title, &p.id),
            p.status, p.approvals, p.merge_allowed, p.merge_reason
        );
    }
    let _ = writeln!(out, "\nactivity");
    for (a, n) in &r.by_actor {
        let _ = writeln!(out, "  {a}: {n}");
    }
    out
}

fn render_report_markdown(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# collab report\n\n{} threads, {} PRs, **{}/{}** entries verified.\n",
        r.threads.len(),
        r.prs.len(),
        r.verified_entries,
        r.total_entries
    );
    let _ = writeln!(
        out,
        "## threads\n\n| title | entries | verified | kinds | last |\n|---|---|---|---|---|"
    );
    for t in &r.threads {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            esc(unit_label(&t.title, &t.id)),
            t.entries,
            t.verified,
            t.kinds.join("/"),
            t.last_ts
        );
    }
    let _ = writeln!(
        out,
        "\n## PRs\n\n| title | status | approvals | merge |\n|---|---|---|---|"
    );
    for p in &r.prs {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            esc(unit_label(&p.title, &p.id)),
            p.status, p.approvals, p.merge_allowed
        );
    }
    out
}

fn render_report_html(r: &Report) -> String {
    let mut rows = String::new();
    for t in &r.threads {
        let _ = writeln!(
            rows,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(unit_label(&t.title, &t.id)),
            t.entries,
            t.verified,
            esc(&t.kinds.join("/")),
            t.last_ts
        );
    }
    let mut prs = String::new();
    for p in &r.prs {
        let _ = writeln!(
            prs,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(unit_label(&p.title, &p.id)),
            esc(&p.status),
            p.approvals,
            p.merge_allowed
        );
    }
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>walgit collab</title>\
<style>body{{font-family:system-ui;margin:2rem;color:#111}}table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.3rem .6rem;text-align:left}}</style>\
</head><body><h1>collab report</h1>\
<p>{} threads, {} PRs, {}/{} entries verified</p>\
<h2>threads</h2><table><tr><th>title</th><th>entries</th><th>verified</th><th>kinds</th><th>last</th></tr>{}</table>\
<h2>PRs</h2><table><tr><th>title</th><th>status</th><th>approvals</th><th>merge</th></tr>{}</table>\
</body></html>",
        r.threads.len(),
        r.prs.len(),
        r.verified_entries,
        r.total_entries,
        rows,
        prs
    )
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn md_cell(s: &str) -> String {
    esc(s)
        .replace('|', "\\|")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "<br>")
}

fn run_report(repo: &Path, format: &str, rules_path: Option<&Path>) -> Result<()> {
    let reader = CollabReader::new(repo);
    let (entries, principals) = reader.load()?;
    let refs: Vec<&EntryRef> = entries.iter().collect();
    let rules: MergeRules = match rules_path {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => MergeRules::default(),
    };
    let report = build_report(&refs, &principals, &rules, chrono::Utc::now().timestamp());
    // The CI section rides the same loaded log and the same aggregation core
    // as `walgit ci status` (§8.3) — one answer, no second semantics.
    let ci = walgit_wal::ci::ci_entries(&refs);
    let runs = walgit_wal::ci::collect_runs(&ci, &principals, chrono::Utc::now().timestamp());
    match format {
        "text" => print!(
            "{}\nci runs\n{}",
            render_report_text(&report),
            crate::ci_cmd::ci_runs_text(&runs)
        ),
        "markdown" => print!(
            "{}\n## CI runs\n\n{}",
            render_report_markdown(&report),
            crate::ci_cmd::ci_runs_markdown(&runs)
        ),
        "html" => {
            let section = crate::ci_cmd::ci_runs_html(&runs);
            let html = render_report_html(&report);
            print!("{}", html.replace("</body>", &format!("{section}</body>")));
        }
        other => bail!("unknown report format {other} (text|markdown|html)"),
    }
    Ok(())
}

// ---- board: the threads projected under the versioned board definition -------

/// The board definition from the same source the server endpoint reads —
/// `.walgit/board.toml` at HEAD — so a pushed board renders identically
/// everywhere. `--board` previews an uncommitted definition; absent file means
/// the built-in default board, a present-but-invalid one is an error (never a
/// silently mis-folded board).
fn load_board_def(repo: &Path, override_path: Option<&Path>) -> Result<BoardDef> {
    if let Some(p) = override_path {
        let doc =
            std::fs::read_to_string(p).with_context(|| format!("read board {}", p.display()))?;
        return parse_board_def(&doc).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()));
    }
    let reader = CollabReader::new(repo);
    match reader.git(&["cat-file", "blob", &format!("HEAD:{BOARD_PATH}")]) {
        Ok(bytes) => parse_board_def(&String::from_utf8_lossy(&bytes))
            .map_err(|e| anyhow::anyhow!("{BOARD_PATH}: {e}")),
        Err(_) => Ok(default_board()),
    }
}

fn card_label(c: &walgit_wal::collab::BoardCard) -> &str {
    unit_label(&c.title, &c.id)
}

fn render_board_text(b: &Board) -> String {
    let mut out = String::new();
    for col in &b.columns {
        let _ = writeln!(out, "== {} ({}) ==", col.name, col.cards.len());
        for c in &col.cards {
            let _ = writeln!(
                out,
                "  {} [{}] {} entries ({} verified), by {}, last {}",
                card_label(c),
                c.status,
                c.entries,
                c.verified,
                c.actor,
                c.last_ts
            );
            let context = [
                (!c.owner.is_empty()).then(|| format!("owner={}", c.owner)),
                (!c.worktree.is_empty()).then(|| format!("worktree={}", c.worktree)),
                (!c.branch.is_empty()).then(|| format!("branch={}", c.branch)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            if !context.is_empty() {
                let _ = writeln!(out, "      {context}");
            }
            if !c.work.is_empty() {
                let _ = writeln!(out, "      work: {}", c.work);
            }
        }
        let _ = writeln!(out);
    }
    out
}

fn render_board_markdown(b: &Board) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# board\n");
    for col in &b.columns {
        let _ = writeln!(out, "## {} ({})\n", col.name, col.cards.len());
        if col.cards.is_empty() {
            let _ = writeln!(out, "_(empty)_\n");
            continue;
        }
        let _ = writeln!(
            out,
            "| card | status | owner | worktree | branch | work | entries | verified | last |\n|---|---|---|---|---|---|---|---|---|"
        );
        for c in &col.cards {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                md_cell(card_label(c)),
                md_cell(&c.status),
                md_cell(&c.owner),
                md_cell(&c.worktree),
                md_cell(&c.branch),
                md_cell(&c.work),
                c.entries,
                c.verified,
                c.last_ts
            );
        }
        let _ = writeln!(out);
    }
    out
}

fn run_board(
    repo: &Path,
    format: &str,
    board_path: Option<&Path>,
    rules_path: Option<&Path>,
) -> Result<()> {
    let reader = CollabReader::new(repo);
    let (entries, principals) = reader.load()?;
    let refs: Vec<&EntryRef> = entries.iter().collect();
    let rules: MergeRules = match rules_path {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => MergeRules::default(),
    };
    let board_def = load_board_def(repo, board_path)?;
    let board = build_board(&refs, &principals, &rules, &board_def);
    match format {
        "text" => print!("{}", render_board_text(&board)),
        "markdown" => print!("{}", render_board_markdown(&board)),
        // The wire form: exactly the bytes `GET /{o}/{r}/api/collab/board`
        // returns, so the two independent clients can be diffed byte-for-byte.
        "json" => std::io::stdout().write_all(&serde_json::to_vec(&board)?)?,
        other => bail!("unknown board format {other} (text|markdown|json)"),
    }
    Ok(())
}

// ---- watch: resident change detection + callback ------------------------------

/// Refs that are new or whose oid changed between two snapshots.
fn changed_refs(
    prev: &std::collections::HashMap<String, String>,
    cur: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = cur
        .iter()
        .filter(|(name, oid)| prev.get(*name) != Some(*oid))
        .map(|(n, o)| (n.clone(), o.clone()))
        .collect();
    out.sort();
    out
}

/// The checkout's git directory (absolute) — where per-checkout client state
/// lives (`collab-watch.json`, `ci-run.json`).
pub(crate) fn absolute_git_dir(repo: &Path) -> Result<PathBuf> {
    let git_dir = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .context("git rev-parse --absolute-git-dir")?;
    if !git_dir.status.success() {
        bail!("{} is not a git checkout", repo.display());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&git_dir.stdout).trim(),
    ))
}

pub(crate) fn state_path(repo: &Path, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    Ok(absolute_git_dir(repo)?.join("collab-watch.json"))
}

fn read_state(path: &Path) -> Result<std::collections::HashMap<String, String>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(std::collections::HashMap::new());
    };
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let mut map = std::collections::HashMap::new();
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            if let Some(oid) = val.as_str() {
                map.insert(k.clone(), oid.to_string());
            }
        }
    }
    Ok(map)
}

fn write_state(path: &Path, map: &std::collections::HashMap<String, String>) -> Result<()> {
    let mut obj = serde_json::Map::new();
    for (k, v) in map {
        obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::Value::Object(obj))?,
    )
    .with_context(|| format!("write state {}", path.display()))?;
    Ok(())
}

pub(crate) fn git_fetch_collab(repo: &Path, remote: &str) -> Result<()> {
    // Entries and meta only — deliberately NOT refs/collab/ci-artifacts/*
    // (D1-CI §8.2): those blobs are up to 16 MiB each and are pulled on
    // demand (`walgit ci log` / `walgit ci artifacts`), never into every
    // watcher checkout.
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args([
            "fetch",
            "-q",
            remote,
            "+refs/collab/inbox/*:refs/collab/inbox/*",
            "+refs/collab/meta/*:refs/collab/meta/*",
        ])
        .output()
        .context("git fetch collab refs")?;
    if !out.status.success() {
        bail!(
            "git fetch {remote} refs/collab/* failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// On-demand pull of one D1-CI object (§8.2). The result entry names the
/// publisher and content address, so fetching the whole artifact namespace
/// would turn one log download into an unbounded transfer.
pub(crate) fn git_fetch_ci_artifact(
    repo: &Path,
    remote: &str,
    actor: &str,
    sha256: &str,
) -> Result<bool> {
    ref_segment("ci.artifact.actor", actor)?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid CI artifact sha256 {sha256:?}");
    }
    let name = format!("{}{actor}/{sha256}", walgit_wal::ci::CI_ARTIFACT_REF_PREFIX);
    let probe = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["ls-remote", "--exit-code", "--refs", remote, &name])
        .output()
        .context("git ls-remote ci artifact ref")?;
    match probe.status.code() {
        Some(0) => {}
        Some(2) => return Ok(false), // no such ref: external/not published
        _ => bail!(
            "git ls-remote {remote} {name} failed: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        ),
    }
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["fetch", "-q", remote, &format!("+{name}:{name}")])
        .output()
        .context("git fetch ci artifact refs")?;
    if !out.status.success() {
        bail!(
            "git fetch {remote} {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(true)
}

fn refs_map(repo: &Path) -> Result<std::collections::HashMap<String, String>> {
    let reader = CollabReader::new(repo);
    let out = reader.git(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/collab",
    ])?;
    let mut map = std::collections::HashMap::new();
    for l in String::from_utf8_lossy(&out).lines() {
        let mut it = l.split_whitespace();
        if let (Some(n), Some(o)) = (it.next(), it.next()) {
            map.insert(n.to_string(), o.to_string());
        }
    }
    Ok(map)
}

/// One new/changed collab ref, described for the callback. The fields are
/// taken from the **parsed entry** — never re-parsed out of rendered text: the
/// blob is remote content, and a body containing a literal `\nkind=` line
/// must not be able to forge the signals an agent's `--exec` keys on.
struct RefEvent {
    kind: String,
    actor: String,
    thread: String,
    verified: bool,
    /// The raw blob text (the entry JSON on stdin).
    text: String,
}

fn describe_ref(repo: &Path, name: &str, oid: &str) -> Result<RefEvent> {
    let blob = CollabReader::new(repo).git(&["cat-file", "blob", oid])?;
    let principals = CollabReader::new(repo).principals()?;
    let text = String::from_utf8_lossy(&blob).to_string();
    // The fold (D45): a snapshot move is one watch event. `verified` is the
    // snapshot's own signature against the folder's registered key — provenance
    // of the fold; the contained entries still verify individually.
    if name == SNAPSHOT_REF {
        let (actor, verified) = match parse_snapshot(text.as_bytes()) {
            Ok(snap) => {
                let verified = principals
                    .get(&snap.actor)
                    .is_some_and(|k| verify_snapshot(&snap, k).is_ok());
                (snap.actor, verified)
            }
            Err(_) => (String::new(), false),
        };
        return Ok(RefEvent {
            kind: "snapshot".into(),
            actor,
            thread: String::new(),
            verified,
            text,
        });
    }
    if let Some(principal) = name.strip_prefix("refs/collab/meta/principals/") {
        return Ok(RefEvent {
            kind: "principal".into(),
            actor: principal.into(),
            thread: String::new(),
            verified: true,
            text,
        });
    }
    let entry: Entry = serde_json::from_str(&text).unwrap_or_else(|_| Entry {
        version: 0,
        kind: "unknown".into(),
        id: String::new(),
        actor: String::new(),
        ts: 0,
        parent: String::new(),
        refs: None,
        body: serde_json::Value::Null,
        sig: String::new(),
    });
    // The inbox path names the principal; verification includes the
    // inbox-consistency invariant (D1 §4.1, `EntryRef::is_verified`).
    let principal = name
        .strip_prefix("refs/collab/inbox/")
        .and_then(|p| p.rsplit_once('/'))
        .map(|(p, _)| p.to_string())
        .unwrap_or_default();
    let er = EntryRef {
        oid: oid.to_string(),
        principal,
        entry,
    };
    Ok(RefEvent {
        kind: er.entry.kind.clone(),
        actor: er.entry.actor.clone(),
        thread: er.entry.id.clone(),
        verified: er.is_verified(&principals),
        text,
    })
}

fn run_exec(cmd: &str, stdin: &str, env: &[(&str, &str)]) -> Result<()> {
    let mut out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("spawn --exec")?;
    out.stdin
        .take()
        .context("--exec stdin")?
        .write_all(stdin.as_bytes())?;
    let status = out.wait_with_output().context("--exec")?.status;
    if !status.success() {
        bail!("--exec failed with {status}");
    }
    Ok(())
}

fn run_watch(
    repo: &Path,
    remote: &str,
    interval: u64,
    once: bool,
    exec: Option<&str>,
    state_override: Option<&Path>,
) -> Result<()> {
    let state_file = state_path(repo, state_override)?;
    loop {
        git_fetch_collab(repo, remote)?;
        let cur = refs_map(repo)?;
        let prev = read_state(&state_file)?;
        let changed = changed_refs(&prev, &cur);
        for (name, oid) in &changed {
            let ev = describe_ref(repo, name, oid)?;
            if let Some(cmd) = exec {
                let env: Vec<(&str, &str)> = vec![
                    ("WALGIT_COLLAB_REF", name.as_str()),
                    ("WALGIT_COLLAB_KIND", ev.kind.as_str()),
                    ("WALGIT_COLLAB_THREAD", ev.thread.as_str()),
                    ("WALGIT_COLLAB_ACTOR", ev.actor.as_str()),
                    (
                        "WALGIT_COLLAB_VERIFIED",
                        if ev.verified { "true" } else { "false" },
                    ),
                ];
                run_exec(cmd, &ev.text, &env)?;
            }
            println!("{name} {oid}");
        }
        write_state(&state_file, &cur)?;
        if once {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

// ---- reading from a local git checkout --------------------------------------

pub struct CollabReader {
    repo: std::path::PathBuf,
}

impl CollabReader {
    pub fn new(repo: impl Into<std::path::PathBuf>) -> Self {
        Self { repo: repo.into() }
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = std::process::Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(args)
            .output()
            .with_context(|| format!("run git {args:?} in {}", self.repo.display()))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// `refs/collab/inbox/<principal>/<uuid>` -> (refname, oid).
    fn inbox_refs(&self) -> Result<Vec<(String, String)>> {
        let out = self.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/collab/inbox",
        ])?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                match (it.next(), it.next()) {
                    (Some(r), Some(o)) => Some((r.to_string(), o.to_string())),
                    _ => None,
                }
            })
            .collect())
    }

    /// The repository's object format; snapshot records carry either 40- or
    /// 64-hex oids and must be validated in the same hash domain.
    fn object_format(&self) -> Result<ObjectFormat> {
        let out = self.git(&["rev-parse", "--show-object-format"])?;
        match String::from_utf8_lossy(&out).trim() {
            "sha1" => Ok(ObjectFormat::Sha1),
            "sha256" => Ok(ObjectFormat::Sha256),
            other => bail!("unsupported git object format {other:?}"),
        }
    }

    /// `refs/collab/meta/principals/<principal>` (repo-local) and
    /// `refs/walgit/principals/<principal>` (host registry cached by
    /// `collab principal-fetch`, issue #76) → principal → public key b64.
    fn principals(&self) -> Result<HashMap<String, String>> {
        let out = self.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/collab/meta/principals",
            "refs/walgit/principals",
        ])?;
        let mut map = HashMap::new();
        for l in String::from_utf8_lossy(&out).lines() {
            let mut it = l.split_whitespace();
            let (Some(name), Some(oid)) = (it.next(), it.next()) else {
                continue;
            };
            let principal = name
                .strip_prefix("refs/collab/meta/principals/")
                .or_else(|| name.strip_prefix("refs/walgit/principals/"));
            let Some(principal) = principal else {
                continue;
            };
            let blob = self.git(&["cat-file", "blob", oid])?;
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&blob)
                && let Some(k) = v.get("public_key").and_then(|k| k.as_str())
            {
                // for-each-ref iterates refs/collab/* before refs/walgit/*, so
                // or_insert keeps the repo-local key authoritative (matches the
                // server's `principals.entry(..).or_insert(..)`).
                map.entry(principal.to_string()).or_insert_with(|| k.to_string());
            }
        }
        Ok(map)
    }

    /// The raw snapshot blob at `refs/collab/meta/snapshot`, when present
    /// (D45 fold), paired with the ref's current oid — the fold baseline the
    /// snapshot push leases against (§11.4: a gc must CAS the ref from the
    /// value it actually read, or a concurrent fold's snapshot can be lost).
    fn snapshot_blob(&self) -> Result<Option<(String, Vec<u8>)>> {
        let out = self.git(&["for-each-ref", "--format=%(objectname)", SNAPSHOT_REF])?;
        let oid = String::from_utf8_lossy(&out).trim().to_string();
        if oid.is_empty() {
            return Ok(None);
        }
        let blob = self.git(&["cat-file", "blob", &oid])?;
        Ok(Some((oid, blob)))
    }

    /// Load the aggregation input: the folded history at
    /// `refs/collab/meta/snapshot` ∪ the unfolded inbox tail, deduped by oid
    /// (D45 — the same set the server's `collab_load` builds), plus the
    /// principals registry.
    pub fn load(&self) -> Result<(Vec<EntryRef>, HashMap<String, String>)> {
        let principals = self.principals()?;
        let format = self.object_format()?;
        let mut set = EntrySet::new();
        if let Some((_, bytes)) = self.snapshot_blob()? {
            let snap = parse_snapshot(&bytes).map_err(|e| anyhow::anyhow!("{SNAPSHOT_REF}: {e}"))?;
            for rec in &snap.entries {
                if let Some(er) = rec.entry_ref(format) {
                    set.insert(er);
                }
            }
        }
        for (name, oid) in self.inbox_refs()? {
            let Some(principal) = name
                .strip_prefix("refs/collab/inbox/")
                .and_then(|p| p.rsplit_once('/'))
                .map(|(p, _)| p.to_string())
            else {
                continue;
            };
            // Match the server aggregation: one corrupt inbox entry must
            // not take the whole report down. Its ref stays in place for gc
            // to report and skip.
            let Ok(blob) = self.git(&["cat-file", "blob", &oid]) else {
                continue;
            };
            let Ok(entry) = serde_json::from_slice::<Entry>(&blob) else {
                continue;
            };
            set.insert(EntryRef {
                oid,
                principal,
                entry,
            });
        }
        Ok((set.into_entries(), principals))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use walgit_wal::collab::{Review, canonicalize, verify_entry};

    fn keypair() -> (ed25519_dalek::SigningKey, String) {
        // Deterministic test key (the CLI's write path takes the key from a
        // user file; generation is not part of the aggregation core).
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        (sk, base64::engine::general_purpose::STANDARD.encode(pk))
    }

    fn entry(
        id: &str,
        kind: &str,
        actor: &str,
        parent: &str,
        oid: &str,
        ts: i64,
        body: serde_json::Value,
    ) -> EntryRef {
        EntryRef {
            oid: oid.into(),
            principal: actor.into(),
            entry: Entry {
                version: 1,
                kind: kind.into(),
                id: id.into(),
                actor: actor.into(),
                ts,
                parent: parent.into(),
                refs: None,
                body,
                sig: String::new(),
            },
        }
    }

    #[test]
    fn canonicalize_is_sorted_and_compact() {
        let v: serde_json::Value = serde_json::json!({"b": 1, "a": {"d": [1, 2], "c": "x"}});
        assert_eq!(canonicalize(&v), r#"{"a":{"c":"x","d":[1,2]},"b":1}"#);
    }

    #[test]
    fn markdown_table_cells_escape_pipes_and_newlines() {
        assert_eq!(md_cell("a|b\nc\r\nd"), "a\\|b<br>c<br>d");
        assert_eq!(md_cell("<x>&|y"), "&lt;x&gt;&amp;\\|y");
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper() {
        let (sk, pk) = keypair();
        let mut e = entry(
            "1",
            "issue",
            "alice",
            "",
            "abc",
            1,
            serde_json::json!({"title": "t"}),
        );
        e.entry.sig = sign_entry(&mut e.entry, &sk);
        assert!(verify_entry(&e.entry, &pk).is_ok());
        e.entry.body = serde_json::json!({"title": "tampered"});
        assert!(verify_entry(&e.entry, &pk).is_err());
    }

    #[test]
    fn gc_signing_key_must_match_the_registered_public_key() {
        let (sk, pk) = keypair();
        assert!(signing_key_matches_registered(&sk, &pk));
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert!(!signing_key_matches_registered(&other, &pk));
    }

    #[test]
    fn fold_records_prefer_the_actor_owned_inbox_copy() {
        let e = entry("t", "issue", "alice", "", "unused", 1, serde_json::json!({}));
        let json = serde_json::to_string(&e.entry).unwrap();
        let oid = walgit_wal::collab::git_blob_oid(json.as_bytes(), ObjectFormat::Sha1);
        let planted = SnapshotRecord {
            oid: oid.clone(),
            principal: "mallory".to_string(),
            json: json.clone(),
        };
        let legitimate = SnapshotRecord {
            oid,
            principal: "alice".to_string(),
            json,
        };

        let mut records = Vec::new();
        let mut seen = HashMap::new();
        assert!(keep_fold_record(
            &mut records,
            &mut seen,
            planted.clone(),
            "alice",
            ObjectFormat::Sha1,
        ));
        assert!(keep_fold_record(
            &mut records,
            &mut seen,
            legitimate,
            "alice",
            ObjectFormat::Sha1,
        ));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].principal, "alice");
        assert!(!keep_fold_record(
            &mut records,
            &mut seen,
            planted,
            "alice",
            ObjectFormat::Sha1,
        ));
        assert_eq!(records[0].principal, "alice", "a later planted copy cannot displace it");
    }

    #[test]
    fn thread_orders_by_parent_chain() {
        let e1 = entry("t", "comment", "alice", "", "a", 1, serde_json::json!({}));
        let mut e2 = entry("t", "comment", "bob", "a", "b", 10, serde_json::json!({}));
        e2.entry.refs = None;
        let refs = vec![&e2, &e1]; // out of order input
        let ordered = thread(&refs);
        assert_eq!(ordered[0].oid, "a");
        assert_eq!(ordered[1].oid, "b");
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn thread_deterministic_with_dangling_parent() {
        let e1 = entry(
            "t",
            "comment",
            "alice",
            "missing",
            "a",
            1,
            serde_json::json!({}),
        );
        let e2 = entry("t", "comment", "bob", "", "b", 2, serde_json::json!({}));
        let refs = vec![&e1, &e2];
        let ordered = thread(&refs);
        // Dangling parent is treated as a root; order is (ts, actor, oid).
        assert_eq!(ordered[0].oid, "a");
        assert_eq!(ordered[1].oid, "b");
    }

    #[test]
    fn merge_rule_counts_only_human_approvals_on_protected_base() {
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 1,
        };
        let mut pr = pr_view(&[], &HashMap::new());
        pr.base = Some("refs/heads/main".into());
        assert!(!merge_rule_eval(&rules, &pr).allowed, "no approvals");

        pr.human_approvals.push(Review {
            actor: "alice".into(),
            decision: "approve".into(),
            ts: 1,
            oid: "x".into(),
        });
        let eval = merge_rule_eval(&rules, &pr);
        assert!(eval.allowed, "{eval:?}");
        assert_eq!(eval.satisfied_by, vec!["alice"]);

        let mut agent_only = pr.clone();
        agent_only.human_approvals = vec![Review {
            actor: "svc-reviewer".into(),
            decision: "approve".into(),
            ts: 1,
            oid: "y".into(),
        }];
        assert!(
            !merge_rule_eval(&rules, &agent_only).allowed,
            "agent approval does not count"
        );
    }

    #[test]
    fn report_is_deterministic_and_counts_verification() {
        let (sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);

        let mut issue = entry(
            "pr1",
            "issue",
            "alice",
            "",
            "a",
            1,
            serde_json::json!({"title": "t"}),
        );
        issue.entry.sig = sign_entry(&mut issue.entry, &sk);
        let unsigned = entry("pr1", "comment", "bob", "a", "b", 2, serde_json::json!({}));
        let mut patch = entry("pr1", "patch", "alice", "b", "c", 3, serde_json::json!({}));
        patch.entry.refs = Some(EntryRefs {
            base: Some("refs/heads/main".into()),
            head: Some("refs/heads/topic".into()),
        });
        patch.entry.sig = sign_entry(&mut patch.entry, &sk);
        let mut review = entry(
            "pr1",
            "review",
            "alice",
            "c",
            "d",
            4,
            serde_json::json!({"decision": "approve"}),
        );
        review.entry.sig = sign_entry(&mut review.entry, &sk);

        let refs = vec![&issue, &unsigned, &patch, &review];
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 1,
        };
        let r1 = build_report(&refs, &principals, &rules, 1_700_000_000);
        assert_eq!(r1.threads.len(), 1);
        assert_eq!(r1.threads[0].id, "pr1");
        assert_eq!(
            r1.threads[0].title, "t",
            "the root entry's title is the thread's (issue #131)"
        );
        assert_eq!(r1.threads[0].entries, 4);
        assert_eq!(
            r1.threads[0].verified, 3,
            "bob's unsigned entry not verified"
        );
        assert_eq!(r1.total_entries, 4);
        assert_eq!(r1.verified_entries, 3);
        assert_eq!(r1.unverified_entries, 1);
        assert_eq!(r1.missing_principals, 1, "bob has no key");
        assert_eq!(r1.prs.len(), 1);
        assert_eq!(r1.prs[0].title, "t", "the PR row carries the same title");
        assert_eq!(r1.prs[0].approvals, 1);
        assert!(r1.prs[0].merge_allowed);
        assert_eq!(
            r1.by_actor,
            vec![("alice".to_string(), 3), ("bob".to_string(), 1)]
        );

        // Determinism: identical input -> identical text render.
        let r2 = build_report(&refs, &principals, &rules, 1_700_000_000);
        assert_eq!(render_report_text(&r1), render_report_text(&r2));
        assert_eq!(render_report_markdown(&r1), render_report_markdown(&r2));
        assert_eq!(render_report_html(&r1), render_report_html(&r2));
        assert!(render_report_html(&r1).contains("<!doctype html>"));
        assert!(render_report_html(&r1).contains("</html>"));
        // Lists lead with the title, not the bare id (issue #131).
        let text = render_report_text(&r1);
        assert!(text.contains("  t: "), "thread row is titled: {text}");
        assert!(text.contains("  t [open]"), "PR row is titled: {text}");
        assert!(!text.contains("pr1"), "the id hides behind a title: {text}");
    }

    #[test]
    fn changed_refs_reports_new_and_updated_only() {
        let mut prev = std::collections::HashMap::new();
        prev.insert("refs/collab/inbox/a/1".to_string(), "aaa".to_string());
        prev.insert("refs/collab/inbox/a/2".to_string(), "bbb".to_string());
        let mut cur = std::collections::HashMap::new();
        cur.insert("refs/collab/inbox/a/1".to_string(), "aaa".to_string()); // unchanged
        cur.insert("refs/collab/inbox/a/2".to_string(), "ccc".to_string()); // updated
        cur.insert("refs/collab/inbox/b/3".to_string(), "ddd".to_string()); // new
        let changed = changed_refs(&prev, &cur);
        assert_eq!(
            changed,
            vec![
                ("refs/collab/inbox/a/2".to_string(), "ccc".to_string()),
                ("refs/collab/inbox/b/3".to_string(), "ddd".to_string()),
            ]
        );
    }

    #[test]
    fn report_projects_ci_runs_not_board_cards() {
        // §8.3: a pure-CI thread is no board card, but the report's CI section
        // (Report.runs) still finds it — the SPA guide and `collab report` read it.
        const RUN: &str = "ci-0123456789abcdef";
        const TS: i64 = 1_700_000_000;
        let (sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("ci-runner-a".to_string(), pk.clone());
        principals.insert("ci-runner-b".to_string(), pk.clone());

        let mut ca = entry(
            RUN,
            "ci_claim",
            "ci-runner-a",
            "",
            "ca",
            TS,
            serde_json::json!({"task":"test","ref":"refs/heads/main","commit":"c0ffee","ttl":300,"attempt":1}),
        );
        ca.entry.sig = sign_entry(&mut ca.entry, &sk);
        let mut cb = entry(
            RUN,
            "ci_claim",
            "ci-runner-b",
            "",
            "cb",
            TS + 1,
            serde_json::json!({"task":"test","ref":"refs/heads/main","commit":"c0ffee","ttl":300,"attempt":1}),
        );
        cb.entry.sig = sign_entry(&mut cb.entry, &sk);
        let mut ra = entry(
            RUN,
            "ci_result",
            "ci-runner-a",
            "ca",
            "ra",
            TS + 2,
            serde_json::json!({"task":"test","ref":"refs/heads/main","commit":"c0ffee","attempt":1,"claim":"ca","conclusion":"success","exit_code":0,"duration_ms":10,"log_summary":"ok","log_sha256":""}),
        );
        ra.entry.sig = sign_entry(&mut ra.entry, &sk);

        // A pure-CI thread: not a board/report thread card…
        let refs = vec![&ca, &cb, &ra];
        let rules = MergeRules::default();
        let r = build_report(&refs, &principals, &rules, TS + 3);
        assert!(r.threads.is_empty(), "pure-CI threads must not be cards");
        // …but it is the report's CI section, with the race visible.
        assert_eq!(r.runs.len(), 1, "runs: {:?}", r.runs);
        let run = &r.runs[0];
        assert_eq!(run.id, RUN);
        assert_eq!(run.task, "test");
        assert_eq!(run.claims, 2, "both claims are visible");
        assert_eq!(run.state, "done");
        assert_eq!(run.conclusion.as_deref(), Some("success"));
        // A claim with no result at all reads as claimed, not done.
        let mut pending = entry(
            "ci-ffffffffffffffff",
            "ci_claim",
            "ci-runner-b",
            "",
            "cp",
            TS,
            serde_json::json!({"task":"lint","ref":"refs/heads/main","commit":"c0ffee","ttl":300,"attempt":1}),
        );
        pending.entry.sig = sign_entry(&mut pending.entry, &sk);
        let refs = vec![&pending];
        let r = build_report(&refs, &principals, &rules, TS + 3);
        assert_eq!(r.runs.len(), 1);
        assert_eq!(r.runs[0].state, "claimed");
        // Expired claims read as stale — the TTL sight (§6.3).
        let r = build_report(&refs, &principals, &rules, TS + 301);
        assert_eq!(r.runs[0].state, "stale");
    }

    #[test]
    fn pr_view_marks_unverified_approvals() {
        let (sk, pk) = keypair();
        let mut e = entry(
            "pr1",
            "review",
            "alice",
            "",
            "r1",
            1,
            serde_json::json!({"decision": "approve"}),
        );
        e.entry.sig = sign_entry(&mut e.entry, &sk);
        let good = e.clone();
        let mut tampered = e.clone();
        tampered.entry.body = serde_json::json!({"decision": "request_changes"});

        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);
        let refs = vec![&good, &tampered];
        let pr = pr_view(&refs, &principals);
        assert_eq!(
            pr.human_approvals.len(),
            1,
            "only the verified approve counts"
        );
        assert_eq!(pr.unverified.len(), 1, "tampered entry listed unverified");
    }

    #[test]
    fn an_entry_in_the_wrong_inbox_is_not_verified() {
        let (sk, pk) = keypair();
        let mut e = entry(
            "pr1",
            "review",
            "alice", // the JSON names alice
            "",
            "r1",
            1,
            serde_json::json!({"decision": "approve"}),
        );
        e.entry.sig = sign_entry(&mut e.entry, &sk);
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);

        // Same actor, own inbox: verified.
        let mut own = e.clone();
        own.principal = "alice".into();
        assert!(own.is_verified(&principals));

        // The same signed bytes found in someone else's inbox: the signature is
        // alice's, but the inbox model (D1 §4.1) shards by principal — an entry
        // in bob's inbox naming alice as actor does not count.
        let mut smuggled = e.clone();
        smuggled.principal = "bob".into();
        assert!(!smuggled.is_verified(&principals));

        let refs = vec![&smuggled];
        let pr = pr_view(&refs, &principals);
        assert!(
            pr.human_approvals.is_empty(),
            "an approval smuggled into another inbox does not count"
        );
        assert_eq!(pr.unverified.len(), 1);
    }
}

#[cfg(test)]
mod entry_refs_tests {
    use super::*;
    use sha2::Digest;

    /// `--related` / `--depends-on` 注入后,body 里的数组可供聚合端
    /// `referenced_oids` 读取。
    #[test]
    fn refs_injection_lands_in_body() {
        let mut body = serde_json::json!({"title": "x"});
        let related = ["aaa".to_string()];
        let depends_on = ["bbb".to_string()];
        body["related"] = serde_json::Value::Array(
            related.iter().map(|o| serde_json::Value::String(o.clone())).collect(),
        );
        body["depends_on"] = serde_json::Value::Array(
            depends_on.iter().map(|o| serde_json::Value::String(o.clone())).collect(),
        );
        assert_eq!(body["related"][0], "aaa");
        assert_eq!(body["depends_on"][0], "bbb");
        let refs = walgit_wal::collab::referenced_oids(&body);
        assert_eq!(refs, vec!["aaa", "bbb"]);
    }

    /// `--attach`:嵌入 `{filename, sha256, content_b64}`,digest 与内容一致,
    /// 读取方可复算验真(issue #75 ④ 验收)。
    #[test]
    fn attachment_digest_roundtrip() {
        let content = b"attachment bytes";
        let digest = format!("{:x}", sha2::Sha256::digest(content));
        let b64 = base64::engine::general_purpose::STANDARD.encode(content);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
        let digest2 = format!("{:x}", sha2::Sha256::digest(&decoded));
        assert_eq!(digest, digest2);
        assert_eq!(decoded, content);
    }

    /// `run_entry` 的 transition 门禁 helper(issue #104):status=done 被拒的
    /// 机器可读错误 + host registry 提示;非 done 流转自由。拒绝路径不依赖
    /// 真实签名(无 verified approve 本身就是拒绝理由)。
    #[test]
    fn transition_gate_rejects_done_without_prerequisites() {
        let mk = |kind: &str, oid: &str, ts: i64, body: serde_json::Value| EntryRef {
            oid: oid.to_string(),
            principal: "alice".to_string(),
            entry: Entry {
                version: 1,
                kind: kind.to_string(),
                id: "t".to_string(),
                actor: "alice".to_string(),
                ts,
                parent: String::new(),
                refs: None,
                body,
                sig: String::new(),
            },
        };
        let thread_entries = vec![
            mk("issue", "e1", 1, serde_json::json!({"title": "x"})),
            mk("status", "e2", 2, serde_json::json!({"status": "needs-review"})),
        ];
        let principals = HashMap::new();
        let done = mk("status", "e3", 3, serde_json::json!({"status": "done"}));
        let err = check_status_transition(&done.entry, &thread_entries, &principals).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("status transition rejected"), "got: {msg}");
        assert!(msg.contains("principal-fetch"), "缺 host registry 提示: {msg}");
        // 非 done 状态自由流转。
        let open = mk("status", "e4", 4, serde_json::json!({"status": "open"}));
        assert!(check_status_transition(&open.entry, &thread_entries, &principals).is_ok());
    }
}

#[cfg(test)]
mod gc_tests {
    //! D45: a local fold (`collab gc` without `--push`) must not move the
    //! aggregation — same entries, same verification states — and must leave
    //! unparseable inbox blobs in place.
    use super::*;

    fn keypair_file(dir: &Path, seed: u8) -> (PathBuf, String, SigningKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pk = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        let path = dir.join(format!("key{seed}"));
        std::fs::write(&path, hex::encode([seed; 32])).unwrap();
        (path, pk, sk)
    }

    fn mk_entry(kind: &str, id: &str, actor: &str, parent: &str, ts: i64, body: serde_json::Value) -> Entry {
        Entry {
            version: 1,
            kind: kind.into(),
            id: id.into(),
            actor: actor.into(),
            ts,
            parent: parent.into(),
            refs: None,
            body,
            sig: String::new(),
        }
    }

    /// Write an entry blob + inbox ref the way `collab entry` does; returns
    /// the oid for chaining.
    fn push_entry(repo: &Path, e: &Entry) -> String {
        let oid = git_write_blob(repo, &serde_json::to_string_pretty(e).unwrap()).unwrap();
        git_update_ref(
            repo,
            &format!("refs/collab/inbox/{}/{}", e.actor, entry_uuid()),
            Some(&oid),
        )
        .unwrap();
        oid
    }

    fn register(repo: &Path, principal: &str, pk: &str) {
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "principal": principal,
            "public_key": pk,
            "registered_at": 0,
        }))
        .unwrap();
        let oid = git_write_blob(repo, &content).unwrap();
        git_update_ref(repo, &format!("refs/collab/meta/principals/{principal}"), Some(&oid)).unwrap();
    }

    /// The aggregation fingerprint: the report's bytes plus every entry's
    /// (oid, principal, verified, canonical entry) — the fold must not move it.
    fn fingerprint(repo: &Path) -> Vec<u8> {
        let (entries, principals) = CollabReader::new(repo).load().unwrap();
        let refs: Vec<&EntryRef> = entries.iter().collect();
        let mut out =
            serde_json::to_vec(&build_report(&refs, &principals, &MergeRules::default(), i64::MAX))
                .unwrap();
        let mut rows: Vec<String> = entries
            .iter()
            .map(|e| {
                format!(
                    "{} {} {} {}",
                    e.oid,
                    e.principal,
                    e.is_verified(&principals),
                    serde_json::to_string(&e.entry).unwrap()
                )
            })
            .collect();
        rows.sort();
        out.extend_from_slice(rows.join("\n").as_bytes());
        out
    }

    fn inbox_ref_count(repo: &Path) -> usize {
        let out = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["for-each-ref", "--format=%(refname)", "refs/collab/inbox"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    #[test]
    fn push_prune_only_never_deletes_without_a_snapshot_on_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let repo = tmp.path().join("r");
        std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .status()
            .unwrap();
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["remote", "add", "origin"])
            .arg(&remote)
            .status()
            .unwrap();

        let (alice_key, alice_pk, alice_sk) = keypair_file(tmp.path(), 7);
        register(&repo, "alice", &alice_pk);
        let mut issue = mk_entry("issue", "t1", "alice", "", 1, serde_json::json!({"title": "x"}));
        issue.sig = sign_entry(&mut issue, &alice_sk);
        let oid = push_entry(&repo, &issue);
        let inbox = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C"])
                .arg(&repo)
                .args(["for-each-ref", "--format=%(refname)", "refs/collab/inbox"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        git_push(&repo, "origin", &inbox).unwrap();

        // Local-only fold first: snapshot exists locally, but the remote has
        // only the inbox and no snapshot.
        run_gc(&repo, "alice", &alice_key, None).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["fetch", "-q", "origin", "+refs/collab/inbox/*:refs/collab/inbox/*"])
            .status()
            .unwrap();

        let pushed = run_gc(&repo, "alice", &alice_key, Some("origin"));
        let remote_snapshot = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C"])
                .arg(&repo)
                .args(["ls-remote", "origin", SNAPSHOT_REF])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        if pushed.is_ok() {
            assert!(!remote_snapshot.is_empty(), "a successful prune must publish a snapshot first");
            let snap_oid = remote_snapshot.split_whitespace().next().unwrap();
            let snap = std::process::Command::new("git")
                .args(["-C"])
                .arg(&repo)
                .args(["cat-file", "blob", snap_oid])
                .output()
                .unwrap();
            let snap = parse_snapshot(&snap.stdout).unwrap();
            assert!(snap.entries.iter().any(|record| record.oid == oid));
        } else {
            assert_eq!(inbox_ref_count(&repo), 1, "a failed prune leaves the remote inbox intact");
        }
    }

    #[test]
    fn inbox_delete_uses_the_oid_seen_by_ls_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let repo = tmp.path().join("r");
        std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .status()
            .unwrap();
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["remote", "add", "origin"])
            .arg(&remote)
            .status()
            .unwrap();

        let name = "refs/collab/inbox/alice/item";
        let first = git_write_blob(&repo, "first").unwrap();
        let second = git_write_blob(&repo, "second").unwrap();
        git_update_ref(&repo, name, Some(&first)).unwrap();
        git_push(&repo, "origin", name).unwrap();
        let live = git_ls_remote_inbox(&repo, "origin").unwrap();
        assert_eq!(live.get(name), Some(&first));

        git_update_ref(&repo, name, Some(&second)).unwrap();
        let forced = std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["push", "-q", "-f", "origin", name])
            .status()
            .unwrap();
        assert!(forced.success());

        let specs = vec![format!(":{name}")];
        let leases = vec![format!("{name}:{first}")];
        assert!(
            git_push_refspecs(&repo, "origin", &specs, false, &leases).is_err(),
            "a moved inbox ref must fail the delete lease"
        );
        let after = git_ls_remote_inbox(&repo, "origin").unwrap();
        assert_eq!(after.get(name), Some(&second), "the moved entry survives");
    }

    #[test]
    fn load_skips_unparseable_inbox_blobs_like_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();

        let entry = mk_entry("issue", "t1", "alice", "", 1, serde_json::json!({"title": "ok"}));
        push_entry(&repo, &entry);
        let garbage = git_write_blob(&repo, "not an entry").unwrap();
        git_update_ref(&repo, "refs/collab/inbox/alice/garbage0", Some(&garbage)).unwrap();

        let (entries, principals) = CollabReader::new(&repo).load().unwrap();
        assert_eq!(entries.len(), 1, "the valid entry survives");
        assert_eq!(entries[0].entry.id, "t1");
        assert!(principals.is_empty());
    }

    #[test]
    fn gc_local_fold_preserves_the_aggregation_and_the_tail_stays_live() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        let (alice_key, alice_pk, alice_sk) = keypair_file(tmp.path(), 7);
        let (_bob_key, bob_pk, bob_sk) = keypair_file(tmp.path(), 8);
        register(&repo, "alice", &alice_pk);
        register(&repo, "bob", &bob_pk);

        let mut issue = mk_entry("issue", "t1", "alice", "", 1, serde_json::json!({"title": "fold me"}));
        issue.sig = sign_entry(&mut issue, &alice_sk);
        let o1 = push_entry(&repo, &issue);
        let mut comment = mk_entry("comment", "t1", "bob", &o1, 2, serde_json::json!({"text": " chained"}));
        comment.sig = sign_entry(&mut comment, &bob_sk);
        let o2 = push_entry(&repo, &comment);
        // carol is unregistered: her entry is and stays unverified.
        let o3 = push_entry(&repo, &mk_entry("comment", "t1", "carol", &o2, 3, serde_json::json!({"text": "drive-by"})));
        // An unparseable inbox blob: gc must leave its ref alone.
        let garbage = git_write_blob(&repo, "not an entry").unwrap();
        git_update_ref(&repo, "refs/collab/inbox/carol/garbage0", Some(&garbage)).unwrap();

        // The garbage blob fails the CLI's strict inbox parse (pre-existing
        // behavior); gc must still fold around it, so fold first.
        run_gc(&repo, "alice", &alice_key, None).unwrap();
        assert_eq!(inbox_ref_count(&repo), 1, "only the unparseable blob's ref survives");
        let out = std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["for-each-ref", "--format=%(refname) %(objectname)", "refs/collab/inbox"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("garbage0"));
        git_update_ref(&repo, "refs/collab/inbox/carol/garbage0", None).unwrap();

        let before = {
            // Reconstruct the pre-fold fingerprint from the snapshot: it IS
            // the fold — so compare against a fresh aggregation instead.
            let snap_oid = String::from_utf8_lossy(
                &CollabReader::new(&repo)
                    .git(&["rev-parse", SNAPSHOT_REF])
                    .unwrap(),
            )
            .trim()
            .to_string();
            assert!(!snap_oid.is_empty());
            fingerprint(&repo)
        };

        // The fold carried every entry: 3 records, carol's unverified.
        let (entries, principals) = CollabReader::new(&repo).load().unwrap();
        assert_eq!(entries.len(), 3);
        let carol = entries.iter().find(|e| e.entry.actor == "carol").unwrap();
        assert!(!carol.is_verified(&principals));
        assert_eq!(entries.iter().filter(|e| e.is_verified(&principals)).count(), 2);

        // A second gc with an empty tail is a no-op...
        run_gc(&repo, "alice", &alice_key, None).unwrap();
        assert_eq!(fingerprint(&repo), before, "no-op gc moves nothing");

        // ...and the tail stays live: a new entry chains onto a folded tip
        // (its parent oid lives only inside the snapshot now).
        let mut follow = mk_entry("status", "t1", "alice", &o3, 4, serde_json::json!({"status": "in-progress"}));
        follow.sig = sign_entry(&mut follow, &alice_sk);
        push_entry(&repo, &follow);
        let (entries, principals) = CollabReader::new(&repo).load().unwrap();
        assert_eq!(entries.len(), 4);
        let refs: Vec<&EntryRef> = entries.iter().filter(|e| e.entry.id == "t1").collect();
        let ordered = thread(&refs);
        assert_eq!(ordered.len(), 4, "the chain resolves across the fold boundary");
        assert_eq!(ordered[3].entry.kind, "status");
        assert!(ordered.iter().all(|e| e.entry.actor != "carol" || !e.is_verified(&principals)));

        // A second real fold composes with the existing snapshot.
        run_gc(&repo, "alice", &alice_key, None).unwrap();
        assert_eq!(inbox_ref_count(&repo), 0);
        let (entries, _) = CollabReader::new(&repo).load().unwrap();
        assert_eq!(entries.len(), 4, "second fold loses nothing");
    }

    #[test]
    fn watch_describes_the_snapshot_ref_with_fold_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
        let (alice_key, alice_pk, _) = keypair_file(tmp.path(), 7);
        register(&repo, "alice", &alice_pk);
        let mut e = mk_entry("issue", "t1", "alice", "", 1, serde_json::json!({"title": "x"}));
        e.sig = sign_entry(&mut e, &SigningKey::from_bytes(&[7u8; 32]));
        push_entry(&repo, &e);
        run_gc(&repo, "alice", &alice_key, None).unwrap();
        let snap_oid = String::from_utf8_lossy(
            &CollabReader::new(&repo)
                .git(&["rev-parse", SNAPSHOT_REF])
                .unwrap(),
        )
        .trim()
        .to_string();
        let ev = describe_ref(&repo, SNAPSHOT_REF, &snap_oid).unwrap();
        assert_eq!(ev.kind, "snapshot");
        assert_eq!(ev.actor, "alice");
        assert!(ev.verified, "snapshot verifies against the folder's registered key");
        assert!(ev.thread.is_empty());
    }
}

#[cfg(test)]
mod host_root_tests {
    use super::host_root;

    #[test]
    fn parses_repo_remote_to_host_root() {
        assert_eq!(
            host_root("http://walgit.localhost:8081/gqf2008/vox-seat.git").unwrap(),
            "http://walgit.localhost:8081"
        );
        assert_eq!(
            host_root("https://git.example.com/acme/monorepo").unwrap(),
            "https://git.example.com"
        );
    }

    #[test]
    fn rejects_schemeless() {
        assert!(host_root("walgit.localhost:8081/o/r.git").is_err());
    }
}

#[cfg(test)]
mod principal_cache_tests {
    use super::*;

    #[test]
    fn principals_repo_local_overrides_host_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["init", "-q"])
            .status()
            .unwrap();

        let local_oid = git_write_blob(
            repo,
            &serde_json::to_string(&serde_json::json!({
                "version": 1,
                "principal": "p",
                "public_key": "LOCAL",
            }))
            .unwrap(),
        )
        .unwrap();
        let host_oid = git_write_blob(
            repo,
            &serde_json::to_string(&serde_json::json!({
                "version": 1,
                "principal": "p",
                "public_key": "HOST",
            }))
            .unwrap(),
        )
        .unwrap();
        git_update_ref(repo, "refs/collab/meta/principals/p", Some(&local_oid)).unwrap();
        git_update_ref(repo, "refs/walgit/principals/p", Some(&host_oid)).unwrap();

        let map = CollabReader::new(repo).principals().unwrap();
        assert_eq!(map.get("p").map(String::as_str), Some("LOCAL"));
    }
}
