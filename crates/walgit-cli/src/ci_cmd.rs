//! `walgit ci` — the decentralized CI protocol (`docs/D1_CI_PROTOCOL.md`).
//!
//! walgit runs no CI (principle X): the server is only the fact source (bucket)
//! and the event source (ref facts). A **runner** is a credentialed client that
//! subscribes to ref updates, claims a run with a signed `ci_claim` entry,
//! executes the task declared in the tested commit's `.walgit/ci.toml`, and
//! publishes a signed `ci_result` entry back into its collab inbox. Everything
//! this module writes, the aggregation core (`walgit-wal/src/ci.rs`) can
//! re-derive and verify offline — the protocol has no server-side logic.

use anyhow::bail;
use anyhow::{Context, Result};
use base64::Engine as _;
use clap::Subcommand;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Read;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use walgit_bundle::schedule::DueSlot;
use walgit_wal::ci::{
    CI_CLAIM_KIND, CI_RESULT_KIND, Conclusion, Decision, RunView, run_id, scheduled_run_id,
};
use walgit_wal::collab::{Entry, EntryRef, sign_entry};

use crate::proc_group::{kill_tree, spawn_in_own_group};

// ---- schema limits (docs/D1_CI_PROTOCOL.md §3.1, normative bounds) -------------

/// V9: the file itself.
const FILE_MAX_BYTES: usize = 64 * 1024;
/// V4: the command string.
const COMMAND_MAX_BYTES: usize = 4096;
/// V3: tasks per file.
const TASKS_MAX: usize = 64;
/// V3: task name length.
const NAME_MAX: usize = 64;
/// V5: ref patterns per task, and per-pattern length.
const REFS_PER_TASK_MAX: usize = 64;
const REF_PATTERN_MAX: usize = 255;
/// V6: timeout / `claim_ttl` bounds, in seconds.
const TIMEOUT_MIN_SECS: u64 = 1;
const TIMEOUT_MAX_SECS: u64 = 24 * 3600;
/// V7: retry bound.
const ATTEMPTS_MAX: u32 = 10;
/// V8: `env_allow` entries.
const ENV_ALLOW_MAX: usize = 64;

/// Pipeline-level defaults (§3), applied where a task omits the field.
const DEFAULT_REFS: [&str; 1] = ["refs/heads/*"];
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_CLAIM_TTL_SECS: u64 = 300;
const DEFAULT_ATTEMPTS: u32 = 1;

#[derive(Subcommand)]
pub enum CiAction {
    /// Parse and validate a `.walgit/ci.toml` (`docs/D1_CI_PROTOCOL.md` §3.1).
    Validate {
        /// Repository (or any directory) holding the file.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Explicit file path (default `<repo>/.walgit/ci.toml`).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Subscribe to ref tips, claim runs, execute the declared tasks and
    /// publish signed results (`docs/D1_CI_PROTOCOL.md` §4–§9).
    Run {
        /// The checkout to watch: its remote is polled, its collab refs read.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote to poll for tips and to push entries to.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Principal the entries are signed as (conventionally `ci-…`).
        #[arg(long)]
        actor: String,
        /// Ed25519 signing key: 32 raw bytes as hex.
        #[arg(long)]
        key: PathBuf,
        /// One pass over the remote's tips and exit (cron / agent friendly).
        #[arg(long)]
        once: bool,
        /// Seconds between passes in watch mode.
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// Override every task's claim TTL in seconds (ci.toml `claim_ttl`
        /// otherwise; tests shrink it to re-claim quickly).
        #[arg(long)]
        claim_ttl: Option<u64>,
        /// Only run this named task.
        #[arg(long)]
        task: Option<String>,
        /// State file (default `<gitdir>/ci-run.json`, §4).
        #[arg(long)]
        state: Option<PathBuf>,
    },
    /// Every run in the checkout's collab log, aggregated (§8.3).
    Status {
        /// Repository checkout whose collab refs are read.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// text | markdown | json
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// Print a run's full captured log (§8.2): the blob `log_sha256` names,
    /// fetched on demand; falls back to the entry's stored summary.
    Log {
        /// Repository checkout whose collab refs are read.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote to fetch artifact objects from when missing locally.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Run id (ci-…); default: the newest run in the log.
        run: Option<String>,
    },
    /// Download a run's artifacts into a directory (§8.2), sha256-verified.
    Artifacts {
        /// Repository checkout whose collab refs are read.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Remote to fetch artifact objects from when missing locally.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Run id (ci-…); default: the newest run in the log.
        run: Option<String>,
        /// Destination directory.
        #[arg(long, default_value = ".")]
        out: PathBuf,
    },
}

pub async fn run(action: CiAction) -> Result<()> {
    match action {
        CiAction::Validate { repo, file } => {
            let path = file.unwrap_or_else(|| repo.join(".walgit").join("ci.toml"));
            let raw = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let resolved =
                parse_and_validate(&raw).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            println!(
                "{}: ok — claim_ttl {}s, {} task(s)",
                path.display(),
                resolved.claim_ttl_secs,
                resolved.tasks.len()
            );
            for t in &resolved.tasks {
                println!(
                    "  {} refs [{}] timeout {}s attempts {} env_allow [{}]{}\n    {}",
                    t.name,
                    t.refs.join(", "),
                    t.timeout_secs,
                    t.max_attempts,
                    t.env_allow.join(", "),
                    t.schedule
                        .as_deref()
                        .map(|s| format!(" schedule \"{s}\""))
                        .unwrap_or_default(),
                    t.command
                );
            }
            Ok(())
        }
        CiAction::Run {
            repo,
            remote,
            actor,
            key,
            once,
            interval,
            claim_ttl,
            task,
            state,
        } => {
            crate::collab_cmd::ref_segment("ci.actor", &actor)?;
            let signing = crate::collab_cmd::read_signing_key(&key)?;
            let state_file = match state {
                Some(p) => p,
                None => crate::collab_cmd::absolute_git_dir(&repo)?.join("ci-run.json"),
            };
            let runner = Runner {
                repo,
                remote,
                actor,
                key: signing,
                state_file,
                task_filter: task,
            };
            runner.register()?;
            println!(
                "ci: watching {} via {} as {} (interval {}s, state {})",
                runner.repo.display(),
                runner.remote,
                runner.actor,
                interval,
                runner.state_file.display()
            );
            if once {
                let code = runner.run_pass(claim_ttl)?;
                if code != 0 {
                    std::process::exit(1);
                }
                return Ok(());
            }
            // Pull only: every pass re-reads the remote's tips, so the poll
            // interval is the worst-case notification latency.
            loop {
                let _ = runner.run_pass(claim_ttl)?;
                std::thread::sleep(Duration::from_secs(interval.max(1)));
            }
        }
        CiAction::Status { repo, format } => {
            let (entries, principals) = crate::collab_cmd::CollabReader::new(&repo).load()?;
            let refs: Vec<&EntryRef> = entries.iter().collect();
            let ci = walgit_wal::ci::ci_entries(&refs);
            let now = chrono::Utc::now().timestamp();
            let runs = walgit_wal::ci::collect_runs(&ci, &principals, now);
            match format.as_str() {
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "now": now,
                        "runs": runs.values().collect::<Vec<_>>(),
                    }))?
                ),
                "markdown" => print!(
                    "ci status: {} run(s)\n\n{}",
                    runs.len(),
                    ci_runs_markdown(&runs)
                ),
                _ => print!("ci status: {} run(s)\n{}", runs.len(), ci_runs_text(&runs)),
            }
            Ok(())
        }
        CiAction::Log { repo, remote, run } => {
            let (id, result) = latest_effective_result(&repo, &remote, run.as_deref())?;
            let empty_sha = sha256_hex(b"");
            if result.log_sha256.is_empty() || result.log_sha256 == empty_sha {
                // An empty capture was never uploaded; the summary is the log.
                print!("{}", result.log_summary);
                return Ok(());
            }
            if result.log_truncated {
                eprintln!(
                    "ci: run {id}: stored log exceeds the retention cap; showing the retained tail"
                );
            }
            if let Some(bytes) =
                fetch_ci_object(&repo, &remote, &result.actor, &result.log_sha256).await?
            {
                std::io::stdout().write_all(&bytes)?;
            } else {
                eprintln!(
                    "ci: run {id}: log object {} not published; the stored summary follows",
                    result.log_sha256
                );
                print!("{}", result.log_summary);
            }
            Ok(())
        }
        CiAction::Artifacts {
            repo,
            remote,
            run,
            out,
        } => {
            let (id, result) = latest_effective_result(&repo, &remote, run.as_deref())?;
            if result.artifacts.is_empty() {
                println!("ci: run {id} declares no artifacts");
                return Ok(());
            }
            std::fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
            let mut names = HashSet::new();
            for a in &result.artifacts {
                // §8.2 name is a base name; a hostile writer could smuggle a
                // path — never write outside `--out`.
                let Some(fname) = Path::new(&a.name)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                else {
                    eprintln!("ci: {}: unusable name, skipped", a.name);
                    continue;
                };
                if !names.insert(fname.clone()) {
                    eprintln!("ci: {fname}: duplicate artifact name, skipped");
                    continue;
                }
                let Some(bytes) =
                    fetch_ci_object(&repo, &remote, &result.actor, &a.sha256).await?
                else {
                    if let Some(url) = &a.url {
                        println!("ci: {fname}: external artifact, see {url}");
                    } else {
                        eprintln!("ci: {fname}: object {} not published, skipped", a.sha256);
                    }
                    continue;
                };
                let dest = out.join(&fname);
                write_artifact_file(&dest, &bytes)?;
                println!(
                    "ci: {fname} ({} bytes, sha256 verified) -> {}",
                    a.bytes,
                    dest.display()
                );
            }
            Ok(())
        }
    }
}

fn write_artifact_file(dest: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| {
            format!(
                "create {} (refusing to overwrite or follow an existing path)",
                dest.display()
            )
        })?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

/// The run selected by `--run` (or the newest) and its latest attempt's
/// effective result (§7.2) — what `ci log`/`ci artifacts` download from.
fn latest_effective_result(
    repo: &Path,
    remote: &str,
    run: Option<&str>,
) -> Result<(String, walgit_wal::ci::CiResult)> {
    if let Err(e) = crate::collab_cmd::git_fetch_collab(repo, remote) {
        eprintln!("ci: fetch collab refs failed ({e:#}); reading local state");
    }
    let (entries, principals) = crate::collab_cmd::CollabReader::new(repo).load()?;
    let refs: Vec<&EntryRef> = entries.iter().collect();
    let ci = walgit_wal::ci::ci_entries(&refs);
    let now = chrono::Utc::now().timestamp();
    let runs = walgit_wal::ci::collect_runs(&ci, &principals, now);
    let view = match run {
        Some(id) => runs
            .get(id)
            .with_context(|| format!("run {id} not found"))?,
        None => runs
            .values()
            .max_by_key(|v| v.last_ts)
            .context("no runs yet")?,
    };
    let result = view
        .attempts
        .last()
        .and_then(|a| a.effective.clone())
        .with_context(|| format!("run {} has no effective result", view.id))?;
    Ok((view.id.clone(), result))
}

/// The bytes of one artifact/log object (§8.2): a git blob under
/// `refs/collab/ci-artifacts/*/<sha256>`, fetched from the remote on demand
/// and sha256-verified before it is handed out. `None` = not published.
async fn fetch_ci_object(
    repo: &Path,
    remote: &str,
    actor: &str,
    sha256: &str,
) -> Result<Option<Vec<u8>>> {
    if let Some(bytes) = read_ci_object_local(repo, sha256)? {
        return Ok(Some(bytes));
    }
    if !remote_artifact_fits_cap(repo, remote, actor, sha256).await? {
        return Ok(None);
    }
    if !crate::collab_cmd::git_fetch_ci_artifact(repo, remote, actor, sha256)? {
        return Ok(None);
    }
    read_ci_object_local(repo, sha256)
}

/// Ask the walgit HTTP endpoint for the exact actor/sha object size before
/// `git fetch` can materialize it. The endpoint answers 413 for an oversized
/// payload without reading the blob body; a missing object is a normal
/// "not published" outcome. Pure Git/SSH remotes have no size metadata
/// channel, so on-demand artifact reads fail closed instead of downloading
/// an unbounded object just to reject it.
async fn remote_artifact_fits_cap(
    repo: &Path,
    remote: &str,
    actor: &str,
    sha256: &str,
) -> Result<bool> {
    let url = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["remote", "get-url", remote])
        .output()
        .context("git remote get-url")?;
    if !url.status.success() {
        bail!(
            "git remote get-url {remote} failed: {}",
            String::from_utf8_lossy(&url.stderr).trim()
        );
    }
    let remote_url = String::from_utf8_lossy(&url.stdout).trim().to_string();
    let mut endpoint = reqwest::Url::parse(remote_url.trim_end_matches(".git"))
        .with_context(|| format!("remote URL {remote_url:?} is not HTTP(S)"))?;
    anyhow::ensure!(
        matches!(endpoint.scheme(), "http" | "https"),
        "remote {remote_url:?} has no walgit artifact size endpoint; refused to fetch {sha256} before a size check"
    );
    let segments = format!("api/collab/ci-artifacts/{sha256}");
    endpoint
        .path_segments_mut()
        .map_err(|()| anyhow::anyhow!("remote URL {remote_url:?} cannot carry a path"))?
        .pop_if_empty()
        .extend(segments.split('/'));
    endpoint.query_pairs_mut().append_pair("actor", actor);
    let client = artifact_preflight_client(repo, &remote_url)?;
    let mut req = client
        .head(endpoint)
        .header("Accept", "application/octet-stream");
    if let Some(auth) = git_http_authorization(repo, &remote_url)? {
        req = req.header("Authorization", auth);
    }
    let resp = req.send().await.context("HEAD artifact size")?;
    match resp.status().as_u16() {
        200 => Ok(true),
        404 => Ok(false),
        413 => {
            eprintln!(
                "ci: artifact {sha256}: object exceeds the {}-byte cap; skipped before fetch",
                walgit_wal::ci::CI_ARTIFACT_MAX_BYTES
            );
            Ok(false)
        }
        401 | 403 => bail!(
            "HEAD artifact size: HTTP {}; configure the Git credential helper for this host",
            resp.status()
        ),
        status => bail!("HEAD artifact size: unexpected HTTP {status}"),
    }
}

/// Reuse Git's URL-scoped TLS policy for the out-of-band HEAD request: the
/// default standalone deployment pins a self-signed CA only in
/// `http.<url>.sslCAInfo`, which reqwest's system roots cannot see.
fn artifact_preflight_client(repo: &Path, remote_url: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(path) = git_config_urlmatch(repo, remote_url, "http.sslCAInfo")? {
        let path = expand_home(std::path::Path::new(&path));
        let pem = std::fs::read(&path)
            .with_context(|| format!("read Git sslCAInfo {}", path.display()))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("parse Git sslCAInfo {}", path.display()))?
        {
            builder = builder.add_root_certificate(cert);
        }
    }
    if git_config_urlmatch(repo, remote_url, "http.sslVerify")?
        .is_some_and(|v| v.eq_ignore_ascii_case("false"))
    {
        builder = builder
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true);
    }
    builder.build().context("build artifact preflight client")
}

fn git_config_urlmatch(repo: &Path, url: &str, key: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["config", "--get-urlmatch", key, url])
        .output()
        .with_context(|| format!("git config --get-urlmatch {key}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

fn expand_home(path: &Path) -> std::path::PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = raw.strip_prefix("~/") else {
        return path.to_path_buf();
    };
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map_or_else(|| path.to_path_buf(), |home| home.join(rest))
}

/// Ask Git for a credential scoped to this exact remote URL. Never forward a
/// process-global token: without a host-bound credential the preflight sends
/// no Authorization header and a 401 is a hard error.
fn git_http_authorization(repo: &Path, remote_url: &str) -> Result<Option<String>> {
    let url = reqwest::Url::parse(remote_url)
        .with_context(|| format!("remote URL {remote_url:?} cannot be parsed"))?;
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let mut child = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("git credential fill")?;
    if let Some(mut stdin) = child.stdin.take() {
        write!(
            stdin,
            "protocol={}\nhost={}\npath={}\n\n",
            url.scheme(),
            host,
            url.path()
        )
        .context("write git credential query")?;
    }
    let out = child.wait_with_output().context("wait git credential fill")?;
    if !out.status.success() {
        return Ok(None);
    }
    let credential = String::from_utf8_lossy(&out.stdout);
    let mut username = "";
    let mut password = "";
    for line in credential.lines() {
        if let Some(v) = line.strip_prefix("username=") {
            username = v;
        } else if let Some(v) = line.strip_prefix("password=") {
            password = v;
        }
    }
    if password.is_empty() {
        return Ok(None);
    }
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    Ok(Some(format!("Basic {basic}")))
}

fn read_ci_object_local(repo: &Path, sha256: &str) -> Result<Option<Vec<u8>>> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            walgit_wal::ci::CI_ARTIFACT_REF_PREFIX,
        ])
        .output()
        .context("git for-each-ref ci-artifacts")?;
    if !out.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let suffix = format!("/{sha256}");
    let candidates: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(name, _)| name.ends_with(&suffix))
        .map(|(_, oid)| oid.to_string())
        .collect();
    for oid in candidates {
        let size = Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["cat-file", "-s", &oid])
            .output()
            .context("git cat-file -s")?;
        if !size.status.success() {
            continue;
        }
        let size = std::str::from_utf8(&size.stdout)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        if size.is_none_or(|size| size > walgit_wal::ci::CI_ARTIFACT_MAX_BYTES) {
            eprintln!(
                "ci: artifact {sha256}: object {oid} is missing or exceeds the {}-byte cap; ignored",
                walgit_wal::ci::CI_ARTIFACT_MAX_BYTES
            );
            continue;
        }
        let out = Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["cat-file", "blob", &oid])
            .output()
            .context("git cat-file")?;
        if !out.status.success() {
            continue;
        }
        if out.stdout.len() as u64 > walgit_wal::ci::CI_ARTIFACT_MAX_BYTES {
            continue;
        }
        let actual = sha256_hex(&out.stdout);
        if actual != sha256 {
            eprintln!(
                "ci: artifact {sha256}: object {oid} hashes to {actual}; ignored"
            );
            continue;
        }
        return Ok(Some(out.stdout));
    }
    Ok(None)
}

// ---- .walgit/ci.toml schema (docs/D1_CI_PROTOCOL.md §3) ------------------------

/// The raw file. Unknown keys are a validation failure (V1) — a typo in a
/// task declaration must not silently disable a check.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CiConfig {
    version: u32,
    #[serde(default)]
    claim_ttl: Option<String>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    max_attempts: Option<u32>,
    #[serde(default)]
    task: Vec<CiTask>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CiTask {
    name: String,
    #[serde(default)]
    refs: Option<Vec<String>>,
    command: String,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    max_attempts: Option<u32>,
    #[serde(default)]
    env_allow: Option<Vec<String>>,
    /// §4.3: a 6/7-field UTC cron or `@hourly`/`@daily`/`weekly` alias —
    /// the task fires per slot even while its refs do not move.
    #[serde(default)]
    schedule: Option<String>,
    /// §8.2: relative paths (inside the task worktree) to upload as artifacts
    /// after the run — bytes into the repo's object store, references in the
    /// result entry.
    #[serde(default)]
    artifacts: Vec<String>,
}

/// A validated task with pipeline defaults applied — what the runner executes.
#[derive(Clone, Debug)]
pub struct CiResolvedTask {
    pub name: String,
    pub refs: Vec<String>,
    pub command: String,
    pub timeout_secs: u64,
    pub max_attempts: u32,
    pub env_allow: Vec<String>,
    /// The validated cron expression (V10), when the task is also periodic.
    pub schedule: Option<String>,
    /// The validated artifact paths (V11).
    pub artifacts: Vec<String>,
}

/// A validated `.walgit/ci.toml`.
#[derive(Clone, Debug)]
pub struct CiResolved {
    pub claim_ttl_secs: u64,
    pub tasks: Vec<CiResolvedTask>,
}

impl CiResolved {
    /// The tasks whose `refs` patterns match `ref_name`, in declaration order.
    pub fn matching(&self, ref_name: &str) -> Vec<&CiResolvedTask> {
        self.tasks
            .iter()
            .filter(|t| t.refs.iter().any(|p| glob_match(p, ref_name)))
            .collect()
    }
}

/// Parse + validate (§3.1 V1–V9). `Err` carries every violation found.
pub fn parse_and_validate(raw: &[u8]) -> Result<CiResolved, String> {
    let mut errors = Vec::new();
    if raw.len() > FILE_MAX_BYTES {
        return Err(format!(
            "V9: file is {} bytes, limit {FILE_MAX_BYTES}",
            raw.len()
        ));
    }
    let text = match std::str::from_utf8(raw) {
        Ok(t) => t,
        Err(e) => return Err(format!("file is not UTF-8: {e}")),
    };
    let cfg: CiConfig = match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => return Err(format!("V1: {e}")),
    };
    if cfg.version != 1 {
        errors.push(format!("V2: version must be 1, got {}", cfg.version));
    }
    if cfg.task.is_empty() {
        errors.push("V3: at least one [[task]] is required".to_string());
    }
    if cfg.task.len() > TASKS_MAX {
        errors.push(format!("V3: {} tasks, limit {TASKS_MAX}", cfg.task.len()));
    }
    let claim_ttl_secs = match cfg.claim_ttl.as_deref() {
        None => DEFAULT_CLAIM_TTL_SECS,
        Some(s) => match parse_duration_secs(s) {
            Ok(secs) if (TIMEOUT_MIN_SECS..=TIMEOUT_MAX_SECS).contains(&secs) => secs,
            Ok(secs) => {
                errors.push(format!(
                    "V6: claim_ttl {s:?} = {secs}s outside [{TIMEOUT_MIN_SECS}, {TIMEOUT_MAX_SECS}]"
                ));
                DEFAULT_CLAIM_TTL_SECS
            }
            Err(e) => {
                errors.push(format!("V6: claim_ttl {s:?}: {e}"));
                DEFAULT_CLAIM_TTL_SECS
            }
        },
    };
    // Pipeline-level defaults: a task that omits a field inherits these (§3).
    let pipeline_timeout_secs = match cfg.timeout.as_deref() {
        None => DEFAULT_TIMEOUT_SECS,
        Some(s) => match parse_duration_secs(s) {
            Ok(secs) if (TIMEOUT_MIN_SECS..=TIMEOUT_MAX_SECS).contains(&secs) => secs,
            Ok(_) | Err(_) => {
                errors.push(format!("V6: pipeline timeout {s:?} is not a valid duration in [{TIMEOUT_MIN_SECS}, {TIMEOUT_MAX_SECS}]"));
                DEFAULT_TIMEOUT_SECS
            }
        },
    };
    let pipeline_max_attempts = match cfg.max_attempts {
        None => DEFAULT_ATTEMPTS,
        Some(n) => {
            check(
                &mut errors,
                (1..=ATTEMPTS_MAX).contains(&n),
                format!("V7: pipeline max_attempts {n} outside [1, {ATTEMPTS_MAX}]"),
            );
            n
        }
    };
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut tasks = Vec::new();
    for t in &cfg.task {
        let name = &t.name;
        check(
            &mut errors,
            name.len() <= NAME_MAX
                && !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            format!("V3: task name {name:?} must match [A-Za-z0-9._-]{{1,{NAME_MAX}}}"),
        );
        check(
            &mut errors,
            seen.insert(name.as_str()),
            format!("V3: duplicate task name {name:?}"),
        );
        check(
            &mut errors,
            !t.command.is_empty() && t.command.len() <= COMMAND_MAX_BYTES,
            format!(
                "V4: task {name:?} command must be 1..{COMMAND_MAX_BYTES} bytes, got {}",
                t.command.len()
            ),
        );
        let refs = match &t.refs {
            None => DEFAULT_REFS.iter().map(|s| (*s).to_string()).collect(),
            Some(v) => v.clone(),
        };
        check(
            &mut errors,
            !refs.is_empty() && refs.len() <= REFS_PER_TASK_MAX,
            format!("V5: task {name:?} refs must hold 1..{REFS_PER_TASK_MAX} patterns"),
        );
        for r in &refs {
            check(
                &mut errors,
                !r.is_empty() && r.len() <= REF_PATTERN_MAX && r.starts_with("refs/"),
                format!(
                    "V5: task {name:?} ref pattern {r:?} must start with `refs/` and be ≤ {REF_PATTERN_MAX} bytes"
                ),
            );
        }
        let timeout_secs = match t.timeout.as_deref() {
            None => pipeline_timeout_secs,
            Some(s) => match parse_duration_secs(s) {
                Ok(secs) if (TIMEOUT_MIN_SECS..=TIMEOUT_MAX_SECS).contains(&secs) => secs,
                Ok(secs) => {
                    errors.push(format!(
                        "V6: task {name:?} timeout {s:?} = {secs}s outside [{TIMEOUT_MIN_SECS}, {TIMEOUT_MAX_SECS}]"
                    ));
                    pipeline_timeout_secs
                }
                Err(e) => {
                    errors.push(format!("V6: task {name:?} timeout {s:?}: {e}"));
                    pipeline_timeout_secs
                }
            },
        };
        let max_attempts = t.max_attempts.unwrap_or(pipeline_max_attempts);
        check(
            &mut errors,
            (1..=ATTEMPTS_MAX).contains(&max_attempts),
            format!("V7: task {name:?} max_attempts {max_attempts} outside [1, {ATTEMPTS_MAX}]"),
        );
        let env_allow = t.env_allow.clone().unwrap_or_default();
        check(
            &mut errors,
            env_allow.len() <= ENV_ALLOW_MAX,
            format!(
                "V8: task {name:?} env_allow has {} entries, limit {ENV_ALLOW_MAX}",
                env_allow.len()
            ),
        );
        for env_name in &env_allow {
            check(
                &mut errors,
                valid_env_name(env_name) && !env_name.starts_with("WALGIT_CI_"),
                format!(
                    "V8: task {name:?} env_allow {env_name:?} is not a valid env name (or reserves the WALGIT_CI_* namespace, which the runner injects)"
                ),
            );
        }
        // V10 (§4.3): the schedule must be a cron expression the runner can
        // evaluate — the same grammar as bundle strategies (walgit-config).
        if let Some(expr) = &t.schedule {
            check(
                &mut errors,
                !expr.is_empty()
                    && expr.len() <= 255
                    && walgit_bundle::schedule::parse_schedule(expr).is_ok(),
                format!(
                    "V10: task {name:?} schedule {expr:?} must be a 6/7-field UTC cron (sec min hour dom mon dow) or @hourly/@daily/@weekly"
                ),
            );
        }
        // V11 (§8.2): artifact declarations are relative paths inside the
        // task worktree — collectable by the runner, never absolute, never
        // escaping the worktree, `/`-separated on every platform.
        check(
            &mut errors,
            t.artifacts.len() <= walgit_wal::ci::CI_ARTIFACTS_PER_RESULT_MAX,
            format!(
                "V11: task {name:?} artifacts has {} entries, limit {}",
                t.artifacts.len(),
                walgit_wal::ci::CI_ARTIFACTS_PER_RESULT_MAX
            ),
        );
        for a in &t.artifacts {
            check(
                &mut errors,
                !a.is_empty()
                    && a.len() <= 255
                    && !a.starts_with('/')
                    && !a.contains('\\')
                    && !a.contains(':')
                    && a.split('/').all(|c| !c.is_empty() && c != "." && c != ".."),
                format!(
                    "V11: task {name:?} artifacts entry {a:?} must be a relative path (no leading /, no \\ or drive letter, no empty/./.. components)"
                ),
            );
        }
        tasks.push(CiResolvedTask {
            name: name.clone(),
            refs,
            command: t.command.clone(),
            timeout_secs,
            max_attempts,
            env_allow,
            schedule: t.schedule.clone(),
            artifacts: t.artifacts.clone(),
        });
    }
    if errors.is_empty() {
        Ok(CiResolved {
            claim_ttl_secs,
            tasks,
        })
    } else {
        Err(errors.join("\n"))
    }
}

/// Record one violation of §3.1.
fn check(errors: &mut Vec<String>, ok: bool, msg: String) {
    if !ok {
        errors.push(msg);
    }
}

/// `humantime` duration (`"30s"`, `"10m"`, `"1h"`) → whole seconds.
fn parse_duration_secs(s: &str) -> Result<u64, String> {
    humantime::parse_duration(s)
        .map_err(|e| format!("not a duration: {e}"))
        .map(|d| d.as_secs())
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Glob over ref names (§3.2): `*` matches any run of characters **including
/// `/`**, `?` one character, everything else literal; the whole name must
/// match. Iterative greedy backtracking, no allocations per candidate.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star = usize::MAX;
    let mut mark = 0usize;
    while let Some(&nc) = n.get(ni) {
        let pc = p.get(pi).copied();
        if pc.is_some_and(|c| c == '?' || c == nc) {
            pi += 1;
            ni += 1;
        } else if pc == Some('*') {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

// ---- read side (§8.3): one aggregation, three renderings ------------------------

/// The run table shared by `walgit ci status` and the `collab report`'s CI
/// section — they consume the same `RunView`s, never a second aggregation.
pub(crate) fn ci_runs_text(runs: &std::collections::BTreeMap<String, RunView>) -> String {
    let mut out = String::new();
    for r in sorted_runs(runs) {
        let _ = writeln!(
            out,
            "{:<19} {:<10} {:<24} @ {:<8} {:<8} {:<9} attempt {}{}",
            r.id,
            r.task,
            r.repo_ref,
            short_oid(&r.commit),
            r.state.as_str(),
            r.conclusion.as_ref().map_or("-", Conclusion::as_str),
            r.latest_attempt,
            r.runner
                .as_ref()
                .map_or(String::new(), |a| format!(" by {a}")),
        );
        if r.unverified > 0 {
            let _ = writeln!(
                out,
                "{:<19} {} unverified/malformed entry(ies) — visible red, not counted",
                "", r.unverified
            );
        }
    }
    out
}

pub(crate) fn ci_runs_markdown(runs: &std::collections::BTreeMap<String, RunView>) -> String {
    let mut out = String::from(
        "| run | task | ref | commit | state | conclusion | attempt | runner |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for r in sorted_runs(runs) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            r.id,
            r.task,
            r.repo_ref,
            short_oid(&r.commit),
            r.state.as_str(),
            r.conclusion.as_ref().map_or("-", Conclusion::as_str),
            r.latest_attempt,
            r.runner.as_deref().unwrap_or("-"),
        );
    }
    out
}

pub(crate) fn ci_runs_html(runs: &std::collections::BTreeMap<String, RunView>) -> String {
    let mut rows = String::new();
    for r in sorted_runs(runs) {
        let _ = writeln!(
            rows,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            crate::collab_cmd::esc(&r.id),
            crate::collab_cmd::esc(&r.task),
            crate::collab_cmd::esc(&r.repo_ref),
            crate::collab_cmd::esc(&short_oid(&r.commit)),
            r.state.as_str(),
            r.conclusion.as_ref().map_or("-", Conclusion::as_str),
            r.latest_attempt,
            crate::collab_cmd::esc(r.runner.as_deref().unwrap_or("-")),
        );
    }
    format!(
        "<h2>CI runs</h2><table><tr><th>run</th><th>task</th><th>ref</th><th>commit</th>\
         <th>state</th><th>conclusion</th><th>attempt</th><th>runner</th></tr>{rows}</table>"
    )
}

/// Newest first — the same order every surface shows.
fn sorted_runs(runs: &std::collections::BTreeMap<String, RunView>) -> Vec<&RunView> {
    let mut views: Vec<&RunView> = runs.values().collect();
    views.sort_by_key(|r| std::cmp::Reverse(r.last_ts));
    views
}

fn short_oid(commit: &str) -> String {
    commit.chars().take(8).collect()
}

// ---- runner (`walgit ci run`, docs/D1_CI_PROTOCOL.md §4–§9) ---------------------

/// Platform basics every task process gets after `env_clear` (§8.1) — enough
/// for tools to find themselves and temp files to work, nothing else. The
/// runner's own environment (secrets included) stays behind unless the task's
/// `env_allow` names it.
const BASE_ENV_ALLOW: [&str; 10] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
];
/// Captured output kept in memory: the retained tail of merged stdout+stderr.
/// It matches the artifact object cap, so a published log blob is always
/// uploadable; larger output sets `log_truncated` and keeps the last cap bytes.
const LOG_CAPTURE_MAX: usize = 16 * 1024 * 1024;
/// The `log_summary` bound a result entry carries (§8.2).
const LOG_SUMMARY_MAX: usize = 4096;
/// An `error` conclusion voids its claim (§6.3), so the same attempt is
/// re-claimable at once; cap the per-pass loop so a permanently broken
/// environment defers to the next pass instead of spinning.
const ERROR_RECLAIMS_MAX: u32 = 3;

/// The runner: one checkout, one principal, one state file. A plain git
/// client — everything it writes is a signed collab entry in its own inbox,
/// verifiable by any other client offline (§1).
struct Runner {
    repo: PathBuf,
    remote: String,
    actor: String,
    key: ed25519_dalek::SigningKey,
    state_file: PathBuf,
    task_filter: Option<String>,
}

/// One command execution (§8.1).
struct ExecOutcome {
    conclusion: Conclusion,
    exit_code: Option<i64>,
    duration_ms: u64,
    /// Merged stdout+stderr tail, at most `LOG_CAPTURE_MAX` bytes.
    log: Vec<u8>,
    /// True when earlier output was dropped from `log`.
    log_truncated: bool,
    /// §8.2: declared artifacts collected from the worktree before its
    /// removal — the git blob already written, integrity hash computed.
    artifacts: Vec<CollectedArtifact>,
}

#[derive(Default)]
struct CapturedLog {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl CapturedLog {
    /// Flatten the ring once, after the child has closed both pipes.
    fn into_vec(mut self) -> Vec<u8> {
        self.bytes.make_contiguous().to_vec()
    }
}

/// §8.2: one collected artifact, ready to upload.
struct CollectedArtifact {
    /// Base name (§8.2 `name`, ≤ 128 bytes — enforced at collection).
    name: String,
    /// The declared worktree-relative path (§8.2 `path`).
    path: String,
    /// Integrity hash (§8.2 `sha256`) — also the artifact ref's last segment.
    sha256: String,
    bytes: u64,
    /// The git blob oid already written to the runner's object store.
    oid: String,
}

/// Per-task terminal state within one pass (§4: a ref tip is recorded as
/// processed only once **every** task of that tip reached a terminal state).
enum TaskOutcome {
    /// A verdict exists (possibly after retries) — terminal.
    Settled(Conclusion),
    /// Someone else holds the run, or this pass gave up — revisit later.
    Deferred,
}

struct RefOutcome {
    terminal: bool,
    failed: bool,
    schedules: Vec<(String, Option<String>)>,
}

fn schedule_key(ref_name: &str, task: &str) -> String {
    format!("{ref_name}\u{1f}{task}")
}

fn schedule_prefix(ref_name: &str) -> String {
    format!("{ref_name}\u{1f}")
}

fn apply_schedule_snapshot(
    state: &mut RunnerState,
    ref_name: &str,
    schedules: &[(String, Option<String>)],
    task_filter: Option<&str>,
    preserve_cron: bool,
) {
    if let Some(task_name) = task_filter {
        let key = schedule_key(ref_name, task_name);
        state.schedules.remove(&key);
        if !preserve_cron {
            state.cron.remove(&key);
        }
        if let Some((_, Some(expr))) = schedules
            .iter()
            .find(|(name, _)| name == task_name)
        {
            state.schedules.insert(key, expr.clone());
        }
        return;
    }
    let prefix = schedule_prefix(ref_name);
    state.schedules.retain(|key, _| !key.starts_with(&prefix));
    if !preserve_cron {
        state.cron.retain(|key, _| !key.starts_with(&prefix));
    }
    for (task_name, schedule) in schedules {
        if let Some(expr) = schedule {
            state
                .schedules
                .insert(schedule_key(ref_name, task_name), expr.clone());
        }
    }
}

impl Runner {
    /// First-use self-registration (§2/§11): `refs/collab/meta/principals/
    /// <actor>`, the same shape `walgit collab principal register` writes.
    /// Idempotent — re-registering republishes the same key.
    fn register(&self) -> Result<()> {
        let ref_name = format!("refs/collab/meta/principals/{}", self.actor);
        // Idempotent: walgit's receive-pack refuses a non-force update of a
        // non-commit ref, so an already-registered runner must not push its
        // principal ref again — re-registering (key rotation) is an explicit
        // `walgit collab principal-register`, never a side effect of `ci run`.
        let _ = crate::collab_cmd::git_fetch_collab(&self.repo, &self.remote);
        let known = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["rev-parse", "--verify", "--quiet", &ref_name])
            .output()
            .context("git rev-parse")?;
        if known.status.success() {
            return Ok(());
        }
        let public_key =
            base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes());
        let content = serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "principal": self.actor,
            "public_key": public_key,
            "registered_at": chrono::Utc::now().timestamp(),
        }))?;
        let oid = crate::collab_cmd::git_write_blob(&self.repo, &content)?;
        crate::collab_cmd::git_update_ref(&self.repo, &ref_name, Some(&oid))?;
        crate::collab_cmd::git_push(&self.repo, &self.remote, &ref_name)
    }

    /// One subscription pass (§4): fetch collab refs, diff the remote's tips
    /// against the state file, work every changed ref to a terminal state —
    /// then the cron sweep (§4.3): refs standing still still owe their
    /// scheduled tasks a periodic evaluation. Returns a process exit code:
    /// 0, or 1 when something settled non-success.
    fn run_pass(&self, ttl_override: Option<u64>) -> Result<i32> {
        if let Err(e) = crate::collab_cmd::git_fetch_collab(&self.repo, &self.remote) {
            eprintln!("ci: fetching collab refs failed ({e:#}); continuing with local state");
        }
        let mut state = read_state(&self.state_file)?;
        let mut exit = 0i32;
        let mut dirty = false;
        let tips = self.ls_tips()?;
        let live_refs: std::collections::HashSet<&str> =
            tips.iter().map(|(name, _)| name.as_str()).collect();
        let stale: std::collections::HashSet<String> = state
            .processed
            .keys()
            .filter(|name| !live_refs.contains(name.as_str()))
            .cloned()
            .chain(state.schedules.keys().filter_map(|key| {
                let (ref_name, _) = key.split_once('\u{1f}')?;
                (!live_refs.contains(ref_name)).then(|| ref_name.to_string())
            }))
            .collect();
        for ref_name in stale {
            state.processed.remove(&ref_name);
            let prefix = schedule_prefix(&ref_name);
            state.schedules.retain(|key, _| !key.starts_with(&prefix));
            state.cron.retain(|key, _| !key.starts_with(&prefix));
            state.schedule_migration.remove(&ref_name);
            dirty = true;
        }
        for (ref_name, tip) in &tips {
            if state
                .processed
                .get(ref_name)
                .is_some_and(|seen| seen == tip)
                && !state.schedule_migration.contains(ref_name)
            {
                continue;
            }
            match self.process_ref(ref_name, ttl_override) {
                Ok(outcome) => {
                    let preserve_cron = state.schedule_migration.contains(ref_name)
                        && state
                            .processed
                            .get(ref_name)
                            .is_some_and(|seen| seen == tip);
                    apply_schedule_snapshot(
                        &mut state,
                        ref_name,
                        &outcome.schedules,
                        self.task_filter.as_deref(),
                        preserve_cron,
                    );
                    if state.schedule_migration.remove(ref_name) {
                        dirty = true;
                    }
                    if outcome.terminal {
                        state.processed.insert(ref_name.clone(), tip.clone());
                        dirty = true;
                    }
                    if outcome.failed {
                        exit = 1;
                    }
                }
                Err(e) => {
                    eprintln!("ci: {ref_name}: {e:#}");
                    exit = 1;
                }
            }
        }
        // §4.3: sweep every ref whose tip is fully processed — an unmoved ref
        // is exactly what a schedule fires on. A deferred ref (a task still
        // held elsewhere) is not swept; the trigger path revisits it first.
        let now = chrono::Utc::now().timestamp();
        let tip_by_ref: HashMap<&str, &str> =
            tips.iter().map(|(name, tip)| (name.as_str(), tip.as_str())).collect();
        let mut scheduled_keys: Vec<String> = state.schedules.keys().cloned().collect();
        scheduled_keys.sort();
        for key in scheduled_keys {
            let Some((ref_name, task_name)) = key.split_once('\u{1f}') else {
                continue;
            };
            if self.task_filter.as_deref().is_some_and(|name| name != task_name) {
                continue;
            }
            let Some(tip) = tip_by_ref.get(ref_name).copied() else {
                continue;
            };
            if state.processed.get(ref_name).is_none_or(|seen| seen != tip) {
                continue;
            }
            let Some(expr) = state.schedules.get(&key).cloned() else {
                continue;
            };
            match self.cron_sweep_one(
                ref_name,
                tip,
                task_name,
                &expr,
                &mut state,
                ttl_override,
                now,
                &mut exit,
            ) {
                Ok(swept) => dirty |= swept,
                Err(e) => {
                    eprintln!("ci: {ref_name}: cron sweep: {e:#}");
                    exit = 1;
                }
            }
        }
        if dirty {
            write_state(&self.state_file, &state)?;
        }
        Ok(exit)
    }

    /// §4.3: evaluate one persisted scheduled task of an unmoved ref. The
    /// expression is already validated; git objects are read only when a slot
    /// actually fires. Returns whether state changed.
    #[allow(clippy::too_many_arguments)]
    fn cron_sweep_one(
        &self,
        ref_name: &str,
        tip: &str,
        task_name: &str,
        expr: &str,
        state: &mut RunnerState,
        ttl_override: Option<u64>,
        now: i64,
        exit: &mut i32,
    ) -> Result<bool> {
        let Some(&last) = state.cron.get(&schedule_key(ref_name, task_name)) else {
            state.cron.insert(schedule_key(ref_name, task_name), now);
            return Ok(true);
        };
        let Ok(schedule) = walgit_bundle::schedule::parse_schedule(expr) else {
            let key = schedule_key(ref_name, task_name);
            state.schedules.remove(&key);
            state.cron.remove(&key);
            return Ok(true);
        };
        match walgit_bundle::schedule::due_slot(&schedule, last, now) {
            DueSlot::NotDue => Ok(false),
            DueSlot::Skip(slot) => {
                state.cron.insert(schedule_key(ref_name, task_name), slot);
                Ok(true)
            }
            DueSlot::Run(slot) => {
                // `rev-list -1` peels to a commit and prints nothing for a
                // non-commit; `^{commit}` is not usable — a bash-style git
                // (msys64's on Windows) expands the braces out of the argument.
                let probe = Command::new("git")
                    .args(["-C"])
                    .arg(&self.repo)
                    .args(["rev-list", "-1", "--end-of-options", tip])
                    .output()
                    .context("git rev-list -1")?;
                let have = probe.status.success()
                    && !String::from_utf8_lossy(&probe.stdout).trim().is_empty();
                if !have {
                    self.fetch_commit(ref_name)?;
                }
                let Some(raw) = self.read_ci_toml(tip)? else {
                    let key = schedule_key(ref_name, task_name);
                    state.schedules.remove(&key);
                    state.cron.remove(&key);
                    return Ok(true);
                };
                let Ok(cfg) = parse_and_validate(&raw) else {
                    let key = schedule_key(ref_name, task_name);
                    state.schedules.remove(&key);
                    state.cron.remove(&key);
                    return Ok(true);
                };
                let Some(task) = cfg
                    .matching(ref_name)
                    .into_iter()
                    .find(|task| task.name == task_name && task.schedule.as_deref() == Some(expr))
                else {
                    let key = schedule_key(ref_name, task_name);
                    state.schedules.remove(&key);
                    state.cron.remove(&key);
                    return Ok(true);
                };
                let ttl = ttl_override.unwrap_or(cfg.claim_ttl_secs);
                let key = schedule_key(ref_name, task_name);
                let short: String = tip.chars().take(8).collect();
                let id = scheduled_run_id(task_name, ref_name, tip, slot);
                println!(
                    "ci: {ref_name} @ {short} task {task_name}: schedule fired (slot {slot}), run {id}"
                );
                match self.process_task(task, ref_name, tip, ttl, &id)? {
                    TaskOutcome::Settled(Conclusion::Success) => {
                        state.cron.insert(key, slot);
                        Ok(true)
                    }
                    TaskOutcome::Settled(other) => {
                        println!(
                            "ci: {ref_name} @ {short} task {task_name}: settled {}",
                            other.as_str()
                        );
                        state.cron.insert(key, slot);
                        *exit = 1;
                        Ok(true)
                    }
                    // Held elsewhere or gave up: leave the slot pending so
                    // the next pass re-decides the same run (§4, same rule
                    // as the trigger path's unprocessed tip).
                    TaskOutcome::Deferred => Ok(false),
                }
            }
        }
    }

    /// The trigger surface (§3.2): `refs/heads/*` and `refs/tags/*` of the
    /// remote, sorted. Peeled-tag lines (`<oid>\trefs/tags/x^{}`) are skipped.
    fn ls_tips(&self) -> Result<Vec<(String, String)>> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["ls-remote", "--heads", "--tags"])
            .arg(&self.remote)
            .output()
            .context("git ls-remote")?;
        if !out.status.success() {
            bail!(
                "git ls-remote {} failed: {}",
                self.remote,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut tips = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = line.split_whitespace();
            let (Some(oid), Some(name)) = (it.next(), it.next()) else {
                continue;
            };
            if name.ends_with("^{}") {
                continue;
            }
            tips.push((name.to_string(), oid.to_string()));
        }
        tips.sort();
        Ok(tips)
    }

    /// All tasks of one ref tip (§3.2/§4). The tip itself is the caller's
    /// (`run_pass`) processed-state key. The returned schedule snapshot is
    /// persisted so quiet cron passes do not re-read git objects per ref.
    fn process_ref(&self, ref_name: &str, ttl_override: Option<u64>) -> Result<RefOutcome> {
        let commit = self.fetch_commit(ref_name)?;
        let short: String = commit.chars().take(8).collect();
        let Some(raw) = self.read_ci_toml(&commit)? else {
            println!("ci: {ref_name} @ {short}: no .walgit/ci.toml");
            return Ok(RefOutcome {
                terminal: true,
                failed: false,
                schedules: Vec::new(),
            });
        };
        let cfg = match parse_and_validate(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ci: {ref_name} @ {short}: invalid .walgit/ci.toml, skipping: {e}");
                return Ok(RefOutcome {
                    terminal: true,
                    failed: true,
                    schedules: Vec::new(),
                });
            }
        };
        let schedules = cfg
            .matching(ref_name)
            .into_iter()
            .map(|task| (task.name.clone(), task.schedule.clone()))
            .collect();
        let ttl = ttl_override.unwrap_or(cfg.claim_ttl_secs);
        // §4: the tip is recorded as processed only once **every** task of the
        // tip reached a terminal state — a later task standing down (someone
        // else holds the run, or this pass gave up) keeps the tip unrecorded,
        // so the next pass revisits it. A settled task re-decides on revisit
        // (its result already answers the claim) and never re-executes.
        let mut all_terminal = true;
        let mut any_failed = false;
        for t in cfg.matching(ref_name) {
            if self.task_filter.as_deref().is_some_and(|n| n != t.name) {
                continue;
            }
            let id = run_id(&t.name, ref_name, &commit);
            match self.process_task(t, ref_name, &commit, ttl, &id)? {
                TaskOutcome::Settled(Conclusion::Success) => {}
                TaskOutcome::Settled(other) => {
                    println!(
                        "ci: {ref_name} @ {short} task {}: settled {}",
                        t.name,
                        other.as_str()
                    );
                    // A non-success verdict is still terminal (§4); the pass
                    // exits non-zero.
                    any_failed = true;
                }
                TaskOutcome::Deferred => all_terminal = false,
            }
        }
        Ok(RefOutcome {
            terminal: all_terminal,
            failed: any_failed,
            schedules,
        })
    }

    /// One task of one ref tip: decide, claim, execute, publish — repeat until
    /// a terminal state or a hand-off (§6.2). Bounded: attempts ≤ `max_attempts`
    /// (V7, enforced by `decide`) and error re-claims ≤ `ERROR_RECLAIMS_MAX`.
    /// `id` is the run identity — `run_id` for a ref trigger, `scheduled_run_id`
    /// for a cron slot (§4.3); everything downstream is identity-agnostic.
    fn process_task(
        &self,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        ttl: u64,
        id: &str,
    ) -> Result<TaskOutcome> {
        let mut error_reclaims = 0u32;
        loop {
            let decision = match self.run_view(id)? {
                Some(view) => walgit_wal::ci::decide(&view, &self.actor, task.max_attempts),
                None => Decision::Claim { attempt: 1 },
            };
            match decision {
                Decision::Settled { conclusion, .. } => {
                    return Ok(TaskOutcome::Settled(conclusion));
                }
                Decision::StandDown { holder } => {
                    println!(
                        "ci: run {id} task {}: held by {holder}, standing down",
                        task.name
                    );
                    return Ok(TaskOutcome::Deferred);
                }
                Decision::Resume { claim_oid, attempt } => {
                    if !self.run_once(
                        task,
                        ref_name,
                        commit,
                        id,
                        attempt,
                        &claim_oid,
                        &mut error_reclaims,
                    )? {
                        return Ok(TaskOutcome::Deferred);
                    }
                }
                Decision::Claim { attempt } => {
                    let claim_oid = self.publish_claim(id, task, ref_name, commit, ttl, attempt)?;
                    // The convergence point (§6.2 step 3): re-read the log;
                    // only the deterministic winner executes.
                    let recheck = match self.run_view(id)? {
                        Some(v) => walgit_wal::ci::decide(&v, &self.actor, task.max_attempts),
                        None => Decision::Claim { attempt: 1 },
                    };
                    match recheck {
                        Decision::Resume {
                            claim_oid: mine,
                            attempt,
                        } if mine == claim_oid => {
                            if !self.run_once(
                                task,
                                ref_name,
                                commit,
                                id,
                                attempt,
                                &claim_oid,
                                &mut error_reclaims,
                            )? {
                                return Ok(TaskOutcome::Deferred);
                            }
                        }
                        Decision::StandDown { holder } => {
                            println!(
                                "ci: run {id} task {}: lost the claim race, held by {holder}",
                                task.name
                            );
                            return Ok(TaskOutcome::Deferred);
                        }
                        // Settled between our push and the re-read, or our
                        // claim died already: leave the run to the next pass.
                        _ => return Ok(TaskOutcome::Deferred),
                    }
                }
            }
        }
    }

    /// Execute once under our claim and publish the result. Returns `false`
    /// when this pass should stop working the run (error re-claim cap, §6.3).
    #[allow(clippy::too_many_arguments)]
    fn run_once(
        &self,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        id: &str,
        attempt: u32,
        claim_oid: &str,
        error_reclaims: &mut u32,
    ) -> Result<bool> {
        let outcome = self.execute(task, ref_name, commit, id, attempt);
        // §8.2: the log + artifact bytes travel through the same receive-pack
        // channel BEFORE the result references them — a published reference
        // must never dangle. An upload failure degrades to references-only
        // (the summary still rides in the entry), never blocks the result.
        let artifacts = match self.upload_run_objects(&outcome) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("ci: run {id}: object upload failed ({e:#}); publishing without them");
                Vec::new()
            }
        };
        self.publish_result(
            id, task, ref_name, commit, attempt, claim_oid, &outcome, artifacts,
        )?;
        if outcome.conclusion != Conclusion::Error {
            return Ok(true);
        }
        *error_reclaims += 1;
        if *error_reclaims >= ERROR_RECLAIMS_MAX {
            eprintln!("ci: run {id}: infrastructure keeps failing; deferring to the next pass");
            return Ok(false);
        }
        Ok(true)
    }

    /// §8.2 storage convention: the captured log and every collected artifact
    /// become content-addressed git blobs under
    /// `refs/collab/ci-artifacts/<actor>/<sha256>`, pushed in ONE receive-pack
    /// round trip. Returns the result entry's `artifacts` array.
    fn upload_run_objects(&self, outcome: &ExecOutcome) -> Result<Vec<serde_json::Value>> {
        let prefix = walgit_wal::ci::CI_ARTIFACT_REF_PREFIX;
        // Ref name → blob oid, deduplicated by content (two identical files
        // share one sha256, one ref, one blob).
        let mut refs: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        if !outcome.log.is_empty() {
            let sha = sha256_hex(&outcome.log);
            let oid = crate::collab_cmd::git_write_blob_bytes(&self.repo, &outcome.log)?;
            refs.insert(format!("{prefix}{}/{sha}", self.actor), oid);
        }
        for a in &outcome.artifacts {
            refs.insert(
                format!("{prefix}{}/{}", self.actor, a.sha256),
                a.oid.clone(),
            );
        }
        for (name, oid) in &refs {
            crate::collab_cmd::git_update_ref(&self.repo, name, Some(oid))?;
        }
        let names: Vec<&str> = refs.keys().map(String::as_str).collect();
        crate::collab_cmd::git_push_many(&self.repo, &self.remote, &names)?;
        Ok(outcome
            .artifacts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "name": a.name,
                    "path": a.path,
                    "sha256": a.sha256,
                    "bytes": a.bytes,
                })
            })
            .collect())
    }

    /// The run's view over freshly fetched collab refs (§6.2 step 1).
    fn run_view(&self, id: &str) -> Result<Option<RunView>> {
        if let Err(e) = crate::collab_cmd::git_fetch_collab(&self.repo, &self.remote) {
            eprintln!("ci: fetch collab refs failed ({e:#}); reading local state");
        }
        let (entries, principals) = crate::collab_cmd::CollabReader::new(&self.repo).load()?;
        let ci: Vec<&EntryRef> = entries.iter().filter(|e| e.entry.id == id).collect();
        let now = chrono::Utc::now().timestamp();
        Ok(walgit_wal::ci::run_view(id, &ci, &principals, now))
    }

    /// Bring the ref's tip into this checkout and resolve it to a commit oid.
    fn fetch_commit(&self, ref_name: &str) -> Result<String> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["fetch", "-q", "--no-tags"])
            .arg(&self.remote)
            .arg(ref_name)
            .output()
            .with_context(|| format!("git fetch {ref_name}"))?;
        if !out.status.success() {
            bail!(
                "git fetch {} {ref_name} failed: {}",
                self.remote,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["rev-list", "-1", "--end-of-options", "FETCH_HEAD"])
            .output()
            .context("git rev-list -1 FETCH_HEAD")?;
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || oid.is_empty() {
            bail!("{ref_name}: tip does not resolve to a commit");
        }
        Ok(oid)
    }

    /// `.walgit/ci.toml` from the tested commit's tree (§3): `None` if absent.
    fn read_ci_toml(&self, commit: &str) -> Result<Option<Vec<u8>>> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["ls-tree", commit, "--", ".walgit/ci.toml"])
            .output()
            .context("git ls-tree")?;
        if !out.status.success() {
            bail!(
                "git ls-tree {commit} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let Some(meta) = line.split('\t').next() else {
            return Ok(None);
        };
        // `100644 blob <oid>`: mode, kind, oid. A non-blob entry (a
        // submodule) is no declaration.
        let Some(oid) = meta
            .split_whitespace()
            .nth(2)
            .filter(|_| meta.contains(" blob "))
        else {
            return Ok(None);
        };
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["cat-file", "blob", oid])
            .output()
            .context("git cat-file")?;
        if !out.status.success() {
            bail!(
                "git cat-file {oid} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Some(out.stdout))
    }

    /// Sign an entry into the runner's inbox and push it (D1 §4.2 write path).
    fn publish(
        &self,
        kind: &str,
        id: &str,
        parent: &str,
        body: serde_json::Value,
    ) -> Result<String> {
        let mut entry = Entry {
            version: 1,
            kind: kind.to_string(),
            id: id.to_string(),
            actor: self.actor.clone(),
            ts: chrono::Utc::now().timestamp(),
            parent: parent.to_string(),
            refs: None,
            body,
            sig: String::new(),
        };
        entry.sig = sign_entry(&mut entry, &self.key);
        let content = serde_json::to_string_pretty(&entry)?;
        let oid = crate::collab_cmd::git_write_blob(&self.repo, &content)?;
        let ref_name = format!(
            "refs/collab/inbox/{}/{}",
            self.actor,
            crate::collab_cmd::entry_uuid()
        );
        crate::collab_cmd::git_update_ref(&self.repo, &ref_name, Some(&oid))?;
        crate::collab_cmd::git_push(&self.repo, &self.remote, &ref_name)?;
        Ok(oid)
    }

    fn publish_claim(
        &self,
        id: &str,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        ttl: u64,
        attempt: u32,
    ) -> Result<String> {
        let oid = self.publish(
            CI_CLAIM_KIND,
            id,
            "",
            serde_json::json!({
                "task": task.name,
                "ref": ref_name,
                "commit": commit,
                "ttl": ttl,
                "attempt": attempt,
                "runner": format!("walgit ci/{}", env!("CARGO_PKG_VERSION")),
            }),
        )?;
        println!(
            "ci: run {id} task {} attempt {attempt}: claimed (ttl {ttl}s)",
            task.name
        );
        Ok(oid)
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_result(
        &self,
        id: &str,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        attempt: u32,
        claim_oid: &str,
        outcome: &ExecOutcome,
        artifacts: Vec<serde_json::Value>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "task": task.name,
            "ref": ref_name,
            "commit": commit,
            "attempt": attempt,
            "claim": claim_oid,
            "conclusion": outcome.conclusion.as_str(),
            "exit_code": outcome.exit_code,
            "duration_ms": outcome.duration_ms,
            "log_summary": tail_string(&outcome.log, LOG_SUMMARY_MAX),
            "log_sha256": sha256_hex(&outcome.log),
        });
        if let Some(obj) = body.as_object_mut() {
            if !artifacts.is_empty() {
                obj.insert("artifacts".into(), serde_json::Value::Array(artifacts));
            }
            if outcome.log_truncated {
                obj.insert("log_truncated".into(), serde_json::Value::Bool(true));
            }
        }
        let oid = self.publish(CI_RESULT_KIND, id, claim_oid, body)?;
        println!(
            "ci: run {id} task {} attempt {attempt}: {} (exit {}, {} ms) as {oid}",
            task.name,
            outcome.conclusion.as_str(),
            outcome
                .exit_code
                .map_or("signal".to_string(), |c| c.to_string()),
            outcome.duration_ms
        );
        Ok(())
    }

    /// §8.1: a detached worktree of the tested commit, the command inside it,
    /// cleanup whatever happens. Infrastructure failures (temp dir, worktree,
    /// spawn) map to `Conclusion::Error` (§8.2): no task verdict, the claim it
    /// voids, the run re-claimable at once (§6.3).
    fn execute(
        &self,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        id: &str,
        attempt: u32,
    ) -> ExecOutcome {
        let started = Instant::now();
        let fail = |msg: String| ExecOutcome {
            conclusion: Conclusion::Error,
            exit_code: None,
            duration_ms: millis_since(started),
            log: msg.into_bytes(),
            log_truncated: false,
            artifacts: Vec::new(),
        };
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => return fail(format!("create temp dir: {e}")),
        };
        let added = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["worktree", "add", "--detach"])
            .arg(dir.path())
            .arg(commit)
            .output();
        let added = match added {
            Ok(o) => o,
            Err(e) => return fail(format!("git worktree add: {e}")),
        };
        if !added.status.success() {
            return fail(format!(
                "git worktree add {commit}: {}",
                String::from_utf8_lossy(&added.stderr).trim()
            ));
        }
        let mut outcome = self.run_command(dir.path(), task, ref_name, commit, id, attempt);
        // §8.2: collect declared artifacts before the worktree goes away.
        // Infra-error runs never produced a task state worth collecting from.
        if outcome.conclusion != Conclusion::Error && !task.artifacts.is_empty() {
            outcome.artifacts = Self::collect_artifacts(dir.path(), task);
        }
        // Best-effort cleanup: a failed removal never blocks the result (§8.1).
        let _ = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(dir.path())
            .output();
        let _ = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["worktree", "prune"])
            .output();
        outcome
    }

    /// §8.2: hash each declared artifact into the runner's object store (the
    /// worktree shares it) and compute its integrity sha256, streaming — a
    /// missing/oversized/non-file entry is skipped with a note, never fatal:
    /// the result must still publish (§8.1 cleanup discipline).
    fn collect_artifacts(worktree: &Path, task: &CiResolvedTask) -> Vec<CollectedArtifact> {
        let mut out = Vec::new();
        for rel in &task.artifacts {
            let path = worktree.join(rel);
            let meta = match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => m,
                _ => {
                    eprintln!("ci: artifact {rel:?}: not a regular file, skipped");
                    continue;
                }
            };
            if meta.len() > walgit_wal::ci::CI_ARTIFACT_MAX_BYTES {
                eprintln!(
                    "ci: artifact {rel:?}: {} bytes over the {}-byte cap, skipped",
                    meta.len(),
                    walgit_wal::ci::CI_ARTIFACT_MAX_BYTES
                );
                continue;
            }
            let Some(name) = Path::new(rel)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty() && n.len() <= 128)
            else {
                eprintln!("ci: artifact {rel:?}: base name missing or over 128 bytes, skipped");
                continue;
            };
            let oid = match git_hash_object_file(worktree, rel) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("ci: artifact {rel:?}: hash-object failed ({e:#}), skipped");
                    continue;
                }
            };
            let sha256 = match sha256_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ci: artifact {rel:?}: hashing failed ({e:#}), skipped");
                    continue;
                }
            };
            out.push(CollectedArtifact {
                name,
                path: rel.clone(),
                sha256,
                bytes: meta.len(),
                oid,
            });
        }
        out
    }

    /// Spawn the command (§8.1): `sh -c` (POSIX) / `cmd /C` (Windows), cwd =
    /// worktree, cleared environment with the platform basics + `env_allow` +
    /// `WALGIT_CI_*`, capped capture of the merged output, hard timeout — the
    /// child runs in its own process group, so the timeout kills its whole
    /// tree. Never returns `Err`: spawn failures are `Conclusion::Error`.
    fn run_command(
        &self,
        dir: &Path,
        task: &CiResolvedTask,
        ref_name: &str,
        commit: &str,
        id: &str,
        attempt: u32,
    ) -> ExecOutcome {
        let started = Instant::now();
        let fail = |msg: String| ExecOutcome {
            conclusion: Conclusion::Error,
            exit_code: None,
            duration_ms: millis_since(started),
            log: msg.into_bytes(),
            log_truncated: false,
            artifacts: Vec::new(),
        };
        let (program, prefix): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C"])
        } else {
            ("sh", &["-c"])
        };
        let mut cmd = Command::new(program);
        cmd.args(prefix)
            .arg(&task.command)
            .current_dir(dir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in BASE_ENV_ALLOW {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        for name in &task.env_allow {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        cmd.env("WALGIT_CI_TASK", &task.name)
            .env("WALGIT_CI_REF", ref_name)
            .env("WALGIT_CI_COMMIT", commit)
            .env("WALGIT_CI_RUN_ID", id)
            .env("WALGIT_CI_ATTEMPT", attempt.to_string())
            .env("WALGIT_CI_ACTOR", &self.actor);
        spawn_in_own_group(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return fail(format!("spawn `{}`: {e}", task.command)),
        };
        let log = Arc::new(Mutex::new(CapturedLog::default()));
        let readers = [
            drain(child.stdout.take(), Arc::clone(&log)),
            drain(child.stderr.take(), Arc::clone(&log)),
        ];
        let deadline = started + Duration::from_secs(task.timeout_secs);
        let mut status = child.try_wait().ok().flatten();
        while status.is_none() {
            if Instant::now() >= deadline {
                kill_tree(&mut child);
                let _ = child.wait();
                for r in readers {
                    let _ = r.join();
                }
                let captured = take_log(&log);
                let log_truncated = captured.truncated;
                return ExecOutcome {
                    conclusion: Conclusion::Timeout,
                    exit_code: None,
                    duration_ms: millis_since(started),
                    log: captured.into_vec(),
                    log_truncated,
                    artifacts: Vec::new(),
                };
            }
            std::thread::sleep(Duration::from_millis(25));
            status = child.try_wait().ok().flatten();
        }
        for r in readers {
            let _ = r.join();
        }
        let code = status.and_then(|s| s.code());
        let conclusion = if code == Some(0) {
            Conclusion::Success
        } else {
            Conclusion::Failure
        };
        let captured = take_log(&log);
        let log_truncated = captured.truncated;
        ExecOutcome {
            conclusion,
            exit_code: code.map(i64::from),
            duration_ms: millis_since(started),
            log: captured.into_vec(),
            log_truncated,
            artifacts: Vec::new(),
        }
    }
}

/// Milliseconds elapsed since `started`, saturating (§8.2 `duration_ms`).
fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The captured tail out of the shared buffer (poison-tolerant: a panicking
/// reader thread must not lose the task's output).
fn take_log(log: &Mutex<CapturedLog>) -> CapturedLog {
    let mut guard = log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *guard)
}

/// Append one chunk while retaining only the last `limit` bytes.
fn append_capped(captured: &mut CapturedLog, chunk: &[u8], limit: usize) {
    if chunk.len() >= limit {
        captured.bytes.clear();
        let start = chunk.len().saturating_sub(limit);
        if let Some(tail) = chunk.get(start..) {
            captured.bytes.extend(tail);
        }
        captured.truncated = true;
        return;
    }
    if captured.bytes.len() + chunk.len() > limit {
        let drop = captured
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(limit);
        captured.bytes.drain(..drop);
        captured.truncated = true;
    }
    captured.bytes.extend(chunk);
}

/// Pipe one child stream into the shared, capped log tail (§8.2).
fn drain<R: Read + Send + 'static>(
    pipe: Option<R>,
    log: Arc<Mutex<CapturedLog>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(reader) = pipe else {
            return;
        };
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut captured = log
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(chunk) = buf.get(..n) {
                        append_capped(&mut captured, chunk, LOG_CAPTURE_MAX);
                    }
                }
            }
        }
    })
}

/// §8.2 `log_summary`: the last ≤ `max` bytes, snapped forward to a UTF-8
/// char boundary so the writer never truncates mid-codepoint.
fn tail_string(bytes: &[u8], max: usize) -> String {
    let mut start = bytes.len().saturating_sub(max);
    // A UTF-8 continuation byte (0b10xxxxxx) is never a boundary.
    while bytes.get(start).is_some_and(|b| b & 0xC0 == 0x80) {
        start += 1;
    }
    match bytes.get(start..) {
        Some(b) => String::from_utf8_lossy(b).into_owned(),
        None => String::new(),
    }
}

/// §8.2 `log_sha256`: integrity of the full captured output.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// §8.2: streaming sha256 of a file (artifacts are capped at 16 MiB but never
/// buffered whole).
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    std::io::copy(&mut f, &mut h).with_context(|| format!("read {}", path.display()))?;
    Ok(hex::encode(h.finalize()))
}

/// `git hash-object -w -- <rel>` inside the task worktree: the blob lands in
/// the shared object store of the runner's checkout. Returns the blob oid.
fn git_hash_object_file(worktree: &Path, rel: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(worktree)
        .args(["hash-object", "-w", "--"])
        .arg(rel)
        .output()
        .context("git hash-object")?;
    if !out.status.success() {
        bail!(
            "git hash-object -w {rel} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `<gitdir>/ci-run.json` (§4/§4.3), the runner's local bookkeeping — a cache
/// of what the collab log already says, never a second source of truth:
/// `{"processed": {"<ref>": "<tip oid>"}, "cron": {"<ref>\u{1f}<task>":
/// <slot epoch>}, "schedules": {"<ref>\u{1f}<task>": "<cron>"}}`. Missing file
/// = nothing processed yet.
#[derive(Default)]
struct RunnerState {
    /// Ref tips whose every task reached a terminal state (§4).
    processed: HashMap<String, String>,
    /// The newest slot each scheduled (ref, task) pair has accounted for
    /// (§4.3) — seeded at first sight, advanced past each due slot.
    cron: HashMap<String, i64>,
    /// Validated cron expressions by `<ref>\u{1f}<task>`. Populated while the
    /// tip is processed, so a quiet pass never re-reads ci.toml per ref.
    schedules: HashMap<String, String>,
    /// One-shot migration for pre-`schedules` state files: refs whose cron
    /// entries need one schedule-discovery pass before the quiet sweep can
    /// resume. Never serialized.
    schedule_migration: HashSet<String>,
}

fn read_state(path: &Path) -> Result<RunnerState> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(RunnerState::default());
    };
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let mut state = RunnerState::default();
    if let Some(obj) = v.get("processed").and_then(serde_json::Value::as_object) {
        for (k, val) in obj {
            if let Some(oid) = val.as_str() {
                state.processed.insert(k.clone(), oid.to_string());
            }
        }
    }
    if let Some(obj) = v.get("cron").and_then(serde_json::Value::as_object) {
        for (k, val) in obj {
            if let Some(slot) = val.as_i64() {
                state.cron.insert(k.clone(), slot);
            }
        }
    }
    let schedules_present = v.get("schedules").is_some_and(serde_json::Value::is_object);
    if let Some(obj) = v.get("schedules").and_then(serde_json::Value::as_object) {
        for (k, val) in obj {
            if let Some(expr) = val.as_str() {
                state.schedules.insert(k.clone(), expr.to_string());
            }
        }
    }
    if !schedules_present {
        for key in state.cron.keys() {
            if let Some((ref_name, _)) = key.split_once('\u{1f}') {
                state.schedule_migration.insert(ref_name.to_string());
            }
        }
    }
    Ok(state)
}

fn write_state(path: &Path, state: &RunnerState) -> Result<()> {
    let processed: serde_json::Map<String, serde_json::Value> = state
        .processed
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let cron: serde_json::Map<String, serde_json::Value> = state
        .cron
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();
    let schedules: serde_json::Map<String, serde_json::Value> = state
        .schedules
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    std::fs::write(
        path,
        serde_json::to_string_pretty(
            &serde_json::json!({ "processed": processed, "cron": cron, "schedules": schedules }),
        )?,
    )
    .with_context(|| format!("write state {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use walgit_wal::ci::RunState;

    fn ok(raw: &str) -> CiResolved {
        parse_and_validate(raw.as_bytes()).unwrap_or_else(|e| panic!("should validate: {e}"))
    }

    fn errs(raw: &str) -> String {
        parse_and_validate(raw.as_bytes()).expect_err("should fail validation")
    }

    const MINIMAL: &str = r#"
version = 1
[[task]]
name = "test"
command = "cargo test"
"#;

    #[test]
    fn captured_log_keeps_full_bytes_until_the_cap_then_marks_truncation() {
        let mut captured = CapturedLog::default();
        append_capped(&mut captured, b"123456", 10);
        append_capped(&mut captured, b"7890", 10);
        assert_eq!(captured.bytes.make_contiguous(), b"1234567890");
        assert!(!captured.truncated);

        append_capped(&mut captured, b"abcdefghijkl", 10);
        assert_eq!(captured.bytes.make_contiguous(), b"cdefghijkl");
        assert!(captured.truncated);
    }

    #[tokio::test]
    async fn remote_oversize_is_rejected_before_git_fetch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = axum::Router::new().route(
            "/o/r/api/collab/ci-artifacts/{sha256}",
            axum::routing::head(move || {
                let hits = handler_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                        "artifact too large",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args(["remote", "add", "origin"])
            .arg(format!("http://{addr}/o/r.git"))
            .status()
            .unwrap();

        let sha = "ab".repeat(32);
        let allowed = remote_artifact_fits_cap(dir.path(), "origin", "ci-a", &sha)
            .await
            .unwrap();
        assert!(!allowed, "413 closes the fetch path");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "only HEAD was issued");
    }

    #[test]
    fn git_urlmatch_reads_scoped_tls_config() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(dir.path())
            .args([
                "config",
                "http.https://example.test/.sslVerify",
                "false",
            ])
            .status()
            .unwrap();
        assert_eq!(
            git_config_urlmatch(
                dir.path(),
                "https://example.test/o/r.git",
                "http.sslVerify"
            )
            .unwrap()
            .as_deref(),
            Some("false")
        );
    }

    #[cfg(unix)]
    #[test]
    fn artifact_download_refuses_to_follow_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"safe").unwrap();
        let link = dir.path().join("artifact.bin");
        symlink(&victim, &link).unwrap();

        let err = write_artifact_file(&link, b"owned").unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"), "{err:#}");
        assert_eq!(std::fs::read(&victim).unwrap(), b"safe");
    }

    #[test]
    fn cron_bookkeeping_persists_only_scheduled_tasks() {
        let mut state = RunnerState::default();
        apply_schedule_snapshot(
            &mut state,
            "refs/heads/main",
            &[
                ("build".into(), Some("0 0 2 * * *".into())),
                ("test".into(), None),
            ],
            None,
            false,
        );
        assert_eq!(
            state
                .schedules
                .get("refs/heads/main\u{1f}build")
                .map(String::as_str),
            Some("0 0 2 * * *")
        );
        assert!(!state.schedules.contains_key("refs/heads/main\u{1f}test"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ci-run.json");
        write_state(&path, &state).unwrap();
        let loaded = read_state(&path).unwrap();
        assert_eq!(loaded.schedules, state.schedules);
    }

    #[test]
    fn old_state_without_schedules_requests_one_discovery_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ci-run.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "processed": {"refs/heads/main": "abc"},
                "cron": {"refs/heads/main\u{1f}tick": 123}
            }))
            .unwrap(),
        )
        .unwrap();
        let state = read_state(&path).unwrap();
        assert_eq!(state.schedules.len(), 0);
        assert!(
            state
                .schedule_migration
                .contains("refs/heads/main"),
            "legacy cron state must rediscover its schedules"
        );
    }

    #[test]
    fn minimal_valid_with_defaults() {
        let r = ok(MINIMAL);
        assert_eq!(r.claim_ttl_secs, DEFAULT_CLAIM_TTL_SECS);
        assert_eq!(r.tasks.len(), 1);
        let t = &r.tasks[0];
        assert_eq!(t.refs, vec!["refs/heads/*"], "default refs");
        assert_eq!(t.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(t.max_attempts, 1);
        assert!(t.env_allow.is_empty());
        assert_eq!(r.matching("refs/heads/main").len(), 1);
        assert!(
            r.matching("refs/tags/v1").is_empty(),
            "default does not match tags"
        );
    }

    #[test]
    fn task_level_overrides_and_star_crosses_slashes() {
        let r = ok(r#"
version = 1
claim_ttl = "30s"
[[task]]
name = "tag.build"
refs = ["refs/tags/v*"]
command = "make release"
timeout = "1h"
max_attempts = 3
env_allow = ["RUSTFLAGS"]
"#);
        assert_eq!(r.claim_ttl_secs, 30);
        assert_eq!(r.tasks[0].timeout_secs, 3600);
        assert_eq!(r.tasks[0].max_attempts, 3);
        assert_eq!(r.tasks[0].env_allow, vec!["RUSTFLAGS"]);
        assert_eq!(r.matching("refs/tags/v1.2").len(), 1, "`*` crosses `/`");
        assert!(r.matching("refs/heads/main").is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected_top_level_and_per_task() {
        let e = errs(
            r#"
version = 1
concurrency = 4
[[task]]
name = "a"
command = "true"
"#,
        );
        assert!(e.contains("V1"), "{e}");
        let e = errs(
            r#"
version = 1
[[task]]
name = "a"
command = "true"
secrets = ["x"]
"#,
        );
        assert!(e.contains("V1"), "{e}");
    }

    #[test]
    fn validation_matrix() {
        assert!(errs("version = 2\n[[task]]\nname=\"a\"\ncommand=\"true\"\n").contains("V2"));
        assert!(errs("version = 1\n").contains("V3"));
        assert!(errs(
            "version = 1\n[[task]]\nname=\"a\"\ncommand=\"x\"\n[[task]]\nname=\"a\"\ncommand=\"y\"\n"
        )
        .contains("duplicate task name"));
        assert!(errs("version = 1\n[[task]]\nname=\"a b\"\ncommand=\"true\"\n").contains("V3"));
        assert!(errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"\"\n").contains("V4"));
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nrefs=[\"heads/x\"]\n")
                .contains("V5")
        );
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\ntimeout=\"0s\"\n")
                .contains("V6")
        );
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\ntimeout=\"soon\"\n")
                .contains("V6")
        );
        assert!(
            errs("version = 1\nclaim_ttl = \"48h\"\n[[task]]\nname=\"a\"\ncommand=\"true\"\n")
                .contains("V6")
        );
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nmax_attempts=0\n")
                .contains("V7")
        );
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nmax_attempts=11\n")
                .contains("V7")
        );
        assert!(
            errs("version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nenv_allow=[\"1BAD\"]\n")
                .contains("V8")
        );
        assert!(errs(
            "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nenv_allow=[\"WALGIT_CI_TASK\"]\n"
        )
        .contains("V8"));
        assert!(
            errs(
                "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nenv_allow=[\"HAS SPACE\"]\n"
            )
            .contains("V8")
        );
    }

    #[test]
    fn schedule_is_validated_and_carried() {
        // V10 (§4.3): a 6/7-field UTC cron or a shorthand alias, isomorphic to
        // the bundle strategies' schedule (walgit-bundle/src/schedule.rs).
        for expr in [
            "0 0 2 * * *",
            "0 30 9 * * 1-5",
            "@daily",
            "@hourly",
            "@weekly",
        ] {
            let r = ok(&format!(
                "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nschedule=\"{expr}\"\n"
            ));
            assert_eq!(r.tasks[0].schedule.as_deref(), Some(expr));
        }
        // No schedule = the default: ref-triggered only.
        let r = ok(MINIMAL);
        assert_eq!(r.tasks[0].schedule, None);
        // Bad expressions are validation failures naming V10.
        for bad in ["not a cron", "*/2 * * *", "", "61 * * * * *"] {
            let e = errs(&format!(
                "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nschedule=\"{bad}\"\n"
            ));
            assert!(e.contains("V10"), "{bad:?}: {e}");
        }
        // Overlong is rejected even if the prefix parses.
        let long = format!(
            "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nschedule=\"{}\"\n",
            "0 ".repeat(200)
        );
        assert!(errs(&long).contains("V10"));
    }

    #[test]
    fn artifacts_are_validated_and_carried() {
        // V11 (§8.2): declared artifacts are relative paths inside the task
        // worktree — collectable, never absolute, never escaping.
        let r = ok(
            "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nartifacts=[\"target/report.html\", \"dist/out.bin\"]\n",
        );
        assert_eq!(
            r.tasks[0].artifacts,
            vec!["target/report.html".to_string(), "dist/out.bin".to_string()]
        );
        let r = ok(MINIMAL);
        assert!(r.tasks[0].artifacts.is_empty(), "default: no artifacts");
        // TOML literal strings (single quotes) so backslashes stay literal.
        for bad in [
            "/abs/path",
            "C:\\win\\out",
            "a/../escape",
            "../up",
            "./dot",
            "back\\slash",
            "drive:D/x",
            "a//double",
            "",
        ] {
            let e = errs(&format!(
                "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nartifacts=['{bad}']\n"
            ));
            assert!(e.contains("V11"), "{bad:?}: {e}");
        }
        let many = (0..33)
            .map(|i| format!("'f{i}'"))
            .collect::<Vec<_>>()
            .join(",");
        let e = errs(&format!(
            "version = 1\n[[task]]\nname=\"a\"\ncommand=\"true\"\nartifacts=[{many}]\n"
        ));
        assert!(e.contains("V11"), "{e}");
    }

    #[test]
    fn pipeline_defaults_are_inherited_by_omitting_tasks() {
        let r = ok(r#"
version = 1
claim_ttl = "30s"
timeout = "2m"
max_attempts = 4
[[task]]
name = "inherits"
command = "true"
[[task]]
name = "overrides"
command = "true"
timeout = "3m"
max_attempts = 2
"#);
        assert_eq!(r.claim_ttl_secs, 30);
        let inherits = &r.tasks[0];
        assert_eq!(inherits.timeout_secs, 120, "pipeline timeout inherited");
        assert_eq!(inherits.max_attempts, 4, "pipeline attempts inherited");
        let overrides = &r.tasks[1];
        assert_eq!(overrides.timeout_secs, 180);
        assert_eq!(overrides.max_attempts, 2);
        // A bad pipeline-level value is still a validation failure.
        assert!(
            errs("version = 1\nmax_attempts = 0\n[[task]]\nname=\"a\"\ncommand=\"t\"\n")
                .contains("V7")
        );
    }

    #[test]
    fn glob_semantics() {
        assert!(
            glob_match("refs/heads/*", "refs/heads/a/b"),
            "`*` crosses `/`"
        );
        assert!(glob_match("refs/tags/v*", "refs/tags/v1.0.0"));
        assert!(
            !glob_match("refs/heads/main", "refs/heads/mainx"),
            "whole-name match"
        );
        assert!(
            !glob_match("refs/heads/ma*", "refs/heads/x"),
            "literal prefix"
        );
        assert!(glob_match("refs/heads/ma?n", "refs/heads/main"));
        assert!(
            !glob_match("refs/heads/ma?n", "refs/heads/maain"),
            "`?` is one char"
        );
        assert!(glob_match("*", "refs/anything/at/all"));
        assert!(!glob_match("refs/*", "refsx/a"));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_file_is_rejected_before_parse() {
        let raw = vec![b'#'; FILE_MAX_BYTES + 1];
        let e = parse_and_validate(&raw).expect_err("must reject");
        assert!(e.contains("V9"), "{e}");
    }

    #[test]
    fn tail_string_takes_the_last_bytes_at_a_char_boundary() {
        let full = "αβγδεζηθικ";
        let tail = tail_string(full.as_bytes(), 4);
        assert!(full.ends_with(&tail), "{tail:?} must be a suffix");
        assert!(tail.len() <= 4, "{tail:?}");
        assert!(
            tail.chars().count() >= 1,
            "never empty when there is content"
        );
        // A multibyte tail is not cut mid-codepoint: the result is valid UTF-8.
        let noisy = "aé中".repeat(4_000);
        let t = tail_string(noisy.as_bytes(), 100);
        assert!(noisy.ends_with(&t));
        assert!(t.len() <= 100 + 3, "{}", t.len());
        assert_eq!(tail_string(b"hello", 3), "llo");
        assert_eq!(tail_string(b"", 10), "");
    }

    #[test]
    fn runner_state_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("ci-run.json");
        let empty = read_state(&p).expect("read");
        assert!(empty.processed.is_empty(), "missing = empty");
        assert!(empty.cron.is_empty(), "missing = empty");
        let mut state = RunnerState::default();
        state
            .processed
            .insert("refs/heads/main".to_string(), "abc".to_string());
        state
            .processed
            .insert("refs/tags/v1".to_string(), "def".to_string());
        state
            .cron
            .insert("refs/heads/main\u{1f}nightly".to_string(), 1_700_092_800);
        write_state(&p, &state).expect("write");
        let back = read_state(&p).expect("read");
        assert_eq!(back.processed, state.processed);
        assert_eq!(back.cron, state.cron);
    }

    #[test]
    fn runner_state_reads_a_trigger_only_file() {
        // The §4 shape (no "cron" key) is still read — it is what every
        // pre-§4.3 runner wrote; first sight re-seeds the baseline (§4.3).
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("ci-run.json");
        std::fs::write(&p, r#"{"processed": {"refs/heads/main": "abc"}}"#).expect("write");
        let state = read_state(&p).expect("read");
        assert_eq!(
            state.processed.get("refs/heads/main").map(String::as_str),
            Some("abc")
        );
        assert!(state.cron.is_empty());
        // Junk entries in either map are dropped, not fatal.
        std::fs::write(
            &p,
            r#"{"processed": {"refs/heads/main": 7}, "cron": {"k": "not-a-number"}}"#,
        )
        .expect("write");
        let state = read_state(&p).expect("read");
        assert!(state.processed.is_empty());
        assert!(state.cron.is_empty());
    }

    #[test]
    fn status_renders_the_same_view_three_ways() {
        let mut runs = std::collections::BTreeMap::new();
        runs.insert(
            "ci-0123456789abcdef".to_string(),
            RunView {
                id: "ci-0123456789abcdef".to_string(),
                task: "test".to_string(),
                repo_ref: "refs/heads/main".to_string(),
                commit: "76d957cabc".to_string(),
                attempts: Vec::new(),
                latest_attempt: 2,
                state: RunState::Done,
                conclusion: Some(Conclusion::Success),
                runner: Some("ci-1".to_string()),
                last_ts: 100,
                unverified: 1,
            },
        );
        let t = ci_runs_text(&runs);
        assert!(t.contains("done") && t.contains("success"), "{t}");
        assert!(t.contains("attempt 2 by ci-1"), "{t}");
        assert!(t.contains("unverified/malformed"), "{t}");
        let m = ci_runs_markdown(&runs);
        assert!(m.starts_with("| run | task | ref | commit |"), "{m}");
        assert!(m.contains("refs/heads/main") && m.contains("ci-1"), "{m}");
        let h = ci_runs_html(&runs);
        assert!(h.starts_with("<h2>CI runs</h2>"), "{h}");
        assert!(
            h.contains("<td>done</td>") && h.contains("<td>success</td>"),
            "{h}"
        );
    }

    // ---- the executor (§8.1): where the runtime hazards live --------------------
    // These tests shell out to git and run real commands, so they need a real
    // checkout fixture; the shell paths are POSIX-only (the Windows leg of the
    // runner is covered by e2e, where walgit-cli tests do not run at all).

    #[cfg(unix)]
    mod exec {
        use super::*;
        use std::process::Command as SysCommand;

        fn shell_quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }

        #[allow(unsafe_code)] // kill(pid, 0) is the Unix process-existence probe.
        fn process_exists(pid: u32) -> bool {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                return false;
            };
            // SAFETY: signal 0 performs access checks only; it never signals the
            // process or mutates any state.
            if unsafe { libc::kill(pid, 0) } == 0 {
                true
            } else {
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            }
        }

        fn assert_processes_are_gone(pid_file: &Path) {
            let raw = std::fs::read_to_string(pid_file).expect("pid file");
            let pids: Vec<u32> = raw
                .lines()
                .map(|line| line.trim().parse().expect("pid"))
                .collect();
            assert_eq!(
                pids.len(),
                2,
                "both shell and grandchild wrote PIDs: {raw:?}"
            );
            for pid in pids {
                for _ in 0..100 {
                    if !process_exists(pid) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(!process_exists(pid), "PID {pid} survived the tree kill");
            }
        }

        struct Fixture {
            _dir: tempfile::TempDir,
            repo: PathBuf,
            commit: String,
        }

        fn fixture_repo() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let git = |args: &[&str]| {
                let out = SysCommand::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .output()
                    .expect("spawn git");
                assert!(
                    out.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            let init = SysCommand::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(dir.path())
                .output()
                .expect("spawn git init");
            assert!(
                init.status.success(),
                "git init: {}",
                String::from_utf8_lossy(&init.stderr)
            );
            std::fs::write(dir.path().join("f.txt"), "hello\n").expect("write file");
            git(&["add", "."]);
            git(&[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "one",
            ]);
            let out = SysCommand::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse");
            Fixture {
                repo: dir.path().to_path_buf(),
                commit: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                _dir: dir,
            }
        }

        fn runner(repo: &Path) -> Runner {
            Runner {
                repo: repo.to_path_buf(),
                remote: "origin".to_string(),
                actor: "ci-test".to_string(),
                key: ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
                state_file: PathBuf::from("/nonexistent/ci-run.json"),
                task_filter: None,
            }
        }

        fn task(command: &str, timeout_secs: u64, env_allow: &[&str]) -> CiResolvedTask {
            CiResolvedTask {
                name: "test".to_string(),
                refs: vec!["refs/heads/*".to_string()],
                command: command.to_string(),
                timeout_secs,
                max_attempts: 1,
                env_allow: env_allow.iter().map(|s| (*s).to_string()).collect(),
                schedule: None,
                artifacts: Vec::new(),
            }
        }

        #[test]
        fn success_captures_log_and_exit_code() {
            let f = fixture_repo();
            let r = runner(&f.repo);
            let out = r.execute(
                &task("printf hello", 10, &[]),
                "refs/heads/main",
                &f.commit,
                "ci-x",
                1,
            );
            assert_eq!(out.conclusion, Conclusion::Success);
            assert_eq!(out.exit_code, Some(0));
            assert_eq!(tail_string(&out.log, LOG_SUMMARY_MAX), "hello");
            assert!(out.duration_ms < 60_000, "{} ms", out.duration_ms);
        }

        #[test]
        fn failure_maps_nonzero_exit() {
            let f = fixture_repo();
            let r = runner(&f.repo);
            let out = r.execute(
                &task("echo boom >&2; exit 3", 10, &[]),
                "refs/heads/main",
                &f.commit,
                "ci-x",
                1,
            );
            assert_eq!(out.conclusion, Conclusion::Failure);
            assert_eq!(out.exit_code, Some(3));
            assert!(
                String::from_utf8_lossy(&out.log).contains("boom"),
                "stderr merged"
            );
        }

        #[test]
        fn timeout_kills_the_command() {
            // `sh -c` execs its single command on some shells and forks on
            // others; the background job + `wait` forces the forked shape on
            // every platform, so this guards the process-group kill (the
            // grandchild would otherwise hold the capture pipes open past the
            // kill — the ubuntu failure this regression came from).
            let f = fixture_repo();
            let r = runner(&f.repo);
            let dir = tempfile::tempdir().expect("tempdir");
            let pid_file = dir.path().join("pids");
            let command = format!(
                "echo $$ > {path}\nsh -c 'echo $$ >> {path}; exec sleep 30' sh {path} &\nwhile [ \"$(wc -l < {path} | tr -d ' ')\" -lt 2 ]; do sleep 0.01; done\nwait",
                path = shell_quote(&pid_file.to_string_lossy())
            );
            let out = r.execute(
                &task(&command, 1, &[]),
                "refs/heads/main",
                &f.commit,
                "ci-x",
                1,
            );
            assert_eq!(out.conclusion, Conclusion::Timeout);
            assert_eq!(out.exit_code, None);
            assert!(
                out.duration_ms < 5_000,
                "the command must be killed, not waited out: {} ms",
                out.duration_ms
            );
            assert_processes_are_gone(&pid_file);
        }

        #[test]
        fn env_is_allowlist_only_with_injected_task_vars() {
            // §8.1/§9: after env_clear the task sees the platform basics, the
            // allow-listed names and the injected WALGIT_CI_* — nothing else.
            // The guard that has seen red: this session runs with
            // RUSTUP_TOOLCHAIN and CARGO_TARGET_DIR in the environment; were
            // the allowlist leaky (or env_clear missing), `test -z` would fail.
            let f = fixture_repo();
            let r = runner(&f.repo);
            let cmd = "test -n \"$PATH\" \
                && test -z \"$RUSTUP_TOOLCHAIN\" && test -z \"$CARGO_TARGET_DIR\" \
                && test \"$CARGO_PKG_NAME\" = walgit-cli \
                && test \"$WALGIT_CI_TASK\" = test && test \"$WALGIT_CI_REF\" = refs/heads/main \
                && test \"$WALGIT_CI_ATTEMPT\" = 1 && test \"$WALGIT_CI_RUN_ID\" = ci-x \
                && test \"$WALGIT_CI_ACTOR\" = ci-test && test -n \"$WALGIT_CI_COMMIT\"";
            let out = r.execute(
                &task(cmd, 10, &["CARGO_PKG_NAME"]),
                "refs/heads/main",
                &f.commit,
                "ci-x",
                1,
            );
            assert_eq!(
                out.conclusion,
                Conclusion::Success,
                "log: {}",
                String::from_utf8_lossy(&out.log)
            );
        }

        #[test]
        fn infra_failure_maps_to_error_conclusion() {
            // A commit that does not exist locally cannot be worktree'd — the
            // runner reports Conclusion::Error, not a task verdict (§8.2).
            let f = fixture_repo();
            let r = runner(&f.repo);
            let out = r.execute(
                &task("true", 10, &[]),
                "refs/heads/main",
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "ci-x",
                1,
            );
            assert_eq!(out.conclusion, Conclusion::Error);
            assert_eq!(out.exit_code, None);
            assert!(!out.log.is_empty(), "the reason is captured");
        }
    }
}
