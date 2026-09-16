#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::dbg_macro
)]
// Tests may panic freely (the intent recorded in clippy.toml); helpers outside
// #[test] fns are not covered by allow-*-in-tests, so each test target opts out.
#![allow(dead_code)]
//! Test harness: spin up walgit-server on a random port backed by the in-memory
//! store + a tempdir cache, and drive real upstream `git` against it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bytesize::ByteSize;
use walgit_config::{Config, StoreBackend};
use walgit_server::{AppState, router};
use walgit_store::DynStore;
use walgit_store::memory::MemoryStore;

pub struct Server {
    pub base_url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    pub store: Arc<MemoryStore>,
    registry: Arc<walgit_wal::Registry>,
    bundles: Arc<walgit_bundle::Bundler>,
    /// The instance's state (maintenance loop tests drive `run_pass` directly).
    pub state: Arc<walgit_server::AppState>,
    /// Cache dir; removed when the server is dropped (hermetic, no /tmp pile-up).
    /// `None` when the caller owns the cache dir's lifetime — a wipe/restart
    /// test needs the cache to survive the server ("process exit").
    _cache: Option<tempfile::TempDir>,
}

impl Server {
    /// Start one server instance on a random port with a fresh tempdir cache.
    pub async fn start() -> Result<Self> {
        // Install the production JSON telemetry layer (spans across awaits under a
        // multi-threaded runtime once crashed the process; see the concurrency test).
        {
            let mut tcfg = walgit_config::Config::default();
            tcfg.telemetry.log_format = walgit_config::LogFormat::Json;
            tcfg.telemetry.log_filter = "warn,walgit=info,walgit_server=debug,walgit_wal=debug,walgit_git=debug,walgit_store=debug".into();
            walgit_server::telemetry::tracing_init(&tcfg);
        }
        let store = MemoryStore::shared();
        Self::start_with(store, tempfile::tempdir()?).await
    }

    async fn start_with(store: Arc<MemoryStore>, cache: tempfile::TempDir) -> Result<Self> {
        Self::start_with_cfg(store, cache, |_| {}).await
    }

    /// Fresh store + tweaked config.
    pub async fn start_with_tweak(tweak: impl FnOnce(&mut Config)) -> Result<Self> {
        Self::start_with_cfg(MemoryStore::shared(), tempfile::tempdir()?, tweak).await
    }

    /// A given memory store (e.g. with `signing_fails`) + tweaked config.
    pub async fn start_with_store_and_tweak(
        store: Arc<MemoryStore>,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        Self::start_with_cfg(store, tempfile::tempdir()?, tweak).await
    }

    /// Second instance on the same store with a tweaked config (e.g. a tiny
    /// `cache.max_bytes` to simulate a front that cannot hold a repo's packs).
    pub async fn start_sibling_with(&self, tweak: impl FnOnce(&mut Config)) -> Result<Self> {
        Self::start_with_cfg(self.store.clone(), tempfile::tempdir()?, tweak).await
    }

    /// A given memory store and a **caller-owned** cache dir: the caller keeps
    /// the `TempDir`, so the cache survives dropping this server — the building
    /// block for "fresh process, wiped bucket, leftover cache.dir" tests.
    pub async fn start_with_store_and_cache_dir(
        store: Arc<MemoryStore>,
        cache_dir: &std::path::Path,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        Self::start_with_parts(store, cache_dir.to_path_buf(), None, tweak).await
    }

    async fn start_with_cfg(
        store: Arc<MemoryStore>,
        cache: tempfile::TempDir,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        let dir = cache.path().to_path_buf();
        Self::start_with_parts(store, dir, Some(cache), tweak).await
    }

    /// The setup wizard's shape: memory WITHOUT the deliberate flag
    /// (`needs_setup`) plus the config file the save writes back to.
    pub async fn start_setup(config_path: &std::path::Path) -> Result<Self> {
        Self::start_setup_with_tweak(config_path, |_| {}).await
    }

    /// `start_setup` with a further config tweak (e.g. an auth mode that is
    /// already `token` — the F1 branch of the save).
    pub async fn start_setup_with_tweak(
        config_path: &std::path::Path,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        let store = MemoryStore::shared();
        let cache = tempfile::tempdir()?;
        let path = config_path.to_path_buf();
        Self::start_with_parts_and_state_hook(
            store,
            cache.path().to_path_buf(),
            Some(cache),
            move |c| {
                c.store.memory_backend_intentional = false;
                tweak(c);
            },
            move |st| {
                st.config_path = Some(path);
            },
        )
        .await
    }

    /// A **configured** instance (harness defaults: intentional memory store,
    /// so `needs_setup` is false) that nonetheless runs from a config file —
    /// the fixture shape of the admin store-settings surface (#127), whose
    /// PUT edits that file.
    pub async fn start_configured_with_config(
        config_path: &std::path::Path,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        let store = MemoryStore::shared();
        let cache = tempfile::tempdir()?;
        let path = config_path.to_path_buf();
        Self::start_with_parts_and_state_hook(
            store,
            cache.path().to_path_buf(),
            Some(cache),
            tweak,
            move |st| {
                st.config_path = Some(path);
            },
        )
        .await
    }

    async fn start_with_parts(
        store: Arc<MemoryStore>,
        cache_dir: std::path::PathBuf,
        cache: Option<tempfile::TempDir>,
        tweak: impl FnOnce(&mut Config),
    ) -> Result<Self> {
        Self::start_with_parts_and_state_hook(store, cache_dir, cache, tweak, |_| {}).await
    }

    /// `start_with_parts` plus a post-build hook on `AppState` (the setup
    /// wizard's tests arm `config_path` this way — `Arc::get_mut` succeeds
    /// because nothing has cloned the state yet).
    async fn start_with_parts_and_state_hook(
        mut store: Arc<MemoryStore>,
        cache_dir: std::path::PathBuf,
        cache: Option<tempfile::TempDir>,
        tweak: impl FnOnce(&mut Config),
        state_hook: impl FnOnce(&mut AppState),
    ) -> Result<Self> {
        let mut cfg = Config::default();
        cfg.store.backend = StoreBackend::Memory;
        // Tests mean it: the memory backend here is a deliberate in-process
        // store, not the "unconfigured" placeholder the setup wizard (D43)
        // keys on.
        cfg.store.memory_backend_intentional = true;
        cfg.store.bucket = "test".into();
        cfg.cache.dir = cache_dir;
        cfg.cache.max_bytes = ByteSize::gib(2);
        cfg.server.listen = "127.0.0.1:0".parse().unwrap();
        cfg.server.max_concurrent_per_repo = 8;
        cfg.server.request_timeout = std::time::Duration::from_secs(600);
        cfg.server.max_push_bytes = ByteSize::gib(2);
        cfg.wal.fsck_objects = true;
        cfg.wal.check_connectivity = true;
        cfg.wal.freshness_ttl = std::time::Duration::ZERO;
        cfg.git.allow_filter = true;
        cfg.bundles.advertise = true;
        cfg.bundles.min_commits = 0; // tests cut tiny incrementals on purpose; the gate has its own test

        // Allow the e2e suite to run under either upload-pack engine.
        // Default: git (the subprocess engine). Set =gix to use the
        // in-process engine for focused engine tests.
        match std::env::var("WALGIT_TEST_UPLOAD_PACK_ENGINE")
            .unwrap_or_default()
            .as_str()
        {
            "gix" => cfg.git.upload_pack_engine = walgit_config::UploadPackEngine::Gix,
            "auto" => cfg.git.upload_pack_engine = walgit_config::UploadPackEngine::Auto,
            _ => cfg.git.upload_pack_engine = walgit_config::UploadPackEngine::Git,
        }
        if let Ok(ms) = std::env::var("WALGIT_TEST_MEMORY_LATENCY_MS")
            && let Ok(ms) = ms.parse::<u64>()
            && let Some(s) = Arc::get_mut(&mut store)
        {
            s.latency = Some(std::time::Duration::from_millis(ms));
        }

        tweak(&mut cfg);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://{addr}");
        cfg.server.public_url = Some(base_url.clone());
        cfg.validate().context("config validate")?;

        let dyn_store: DynStore = store.clone();
        let mut state = AppState::new(Arc::new(cfg), dyn_store).await?;
        if let Some(st) = Arc::get_mut(&mut state) {
            state_hook(st);
        }

        let registry = state.registry.clone();
        let bundles = state.bundles.clone();
        let app = router(state.clone());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        Ok(Self {
            base_url,
            _shutdown: tx,
            store,
            registry,
            bundles,
            state,
            _cache: cache,
        })
    }

    /// Two instances sharing one `MemoryStore`, different cache dirs.
    pub async fn start_pair() -> Result<(Self, Self)> {
        let mut store = MemoryStore::shared();
        if let Ok(ms) = std::env::var("WALGIT_TEST_MEMORY_LATENCY_MS")
            && let Ok(ms) = ms.parse::<u64>()
            && let Some(s) = Arc::get_mut(&mut store)
        {
            s.latency = Some(std::time::Duration::from_millis(ms));
        }
        let a = Self::start_with(store.clone(), tempfile::tempdir()?).await?;
        let b = Self::start_with(store.clone(), tempfile::tempdir()?).await?;
        Ok((a, b))
    }

    pub fn repo_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/{owner}/{repo}.git", self.base_url)
    }

    /// `repo_url` with HTTP userinfo, percent-encoded — the form a push URL
    /// carries when the test drives the challenge flow: git starts
    /// credential-less, the 401 that offers Basic makes it send the userinfo
    /// (password = the token; §1.3 / issue #79).
    pub fn repo_url_with_userinfo(
        &self,
        owner: &str,
        repo: &str,
        user: &str,
        pass: &str,
    ) -> String {
        let enc = |s: &str| {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        };
        self.repo_url(owner, repo)
            .replacen("://", &format!("://{}:{}@", enc(user), enc(pass)), 1)
    }

    pub async fn put_repo(&self, owner: &str, repo: &str) -> Result<()> {
        let url = format!("{}/{owner}/{repo}", self.base_url);
        let resp = reqwest::Client::new().put(&url).send().await?;
        assert!(resp.status().is_success() || resp.status() == axum::http::StatusCode::CONFLICT);
        Ok(())
    }

    pub async fn get_text(&self, path: &str, headers: &[(&str, &str)]) -> Result<String> {
        let mut req = reqwest::Client::new().get(format!("{}{path}", self.base_url));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        assert!(status.is_success(), "GET {path} -> {status}: {text}");
        Ok(text)
    }

    pub async fn get_status(&self, path: &str) -> Result<axum::http::StatusCode> {
        let resp = reqwest::Client::new()
            .get(format!("{}{path}", self.base_url))
            .send()
            .await?;
        Ok(resp.status())
    }

    pub async fn read_log(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<walgit_proto::v1::LogEntry>> {
        let id = walgit_git::RepoId::new(owner, repo)?;
        let handle = self.registry.open(&id).await?;
        Ok(handle.read_log(1, None).await?)
    }

    /// True when the instance's local copy of the repo has its pack set.
    pub async fn registry_has_packs(&self, owner: &str, repo: &str) -> bool {
        let id = walgit_git::RepoId::new(owner, repo).unwrap();
        match self.registry.open(&id).await {
            Ok(h) => h.packs_ready() && !h.local().packs().map_or(true, |p| p.is_empty()),
            Err(_) => false,
        }
    }

    pub async fn build_bundle(&self, owner: &str, repo: &str, strategy: &str) -> Result<()> {
        let id = walgit_git::RepoId::new(owner, repo)?;
        self.bundles.build(&id, strategy).await?;
        Ok(())
    }

    pub fn ls_remote(&self, owner: &str, repo: &str) -> Result<String> {
        let out = Command::new("git")
            .args(["ls-remote", &self.repo_url(owner, repo)])
            .output()?;
        assert!(
            out.status.success(),
            "ls-remote failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

pub struct TestRepo {
    pub dir: PathBuf,
    /// Removed on drop.
    _tmp: tempfile::TempDir,
}

impl TestRepo {
    /// Create a synthetic repo with `commits` commits, each adding `files` files.
    ///
    /// Use one `git fast-import` process rather than spawning `git add` and
    /// `git commit` for every revision.  Besides being much faster for the
    /// large-repository e2e test, fixed author timestamps make the object
    /// graph deterministic.
    pub fn synthetic(commits: usize, files: usize) -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().to_path_buf();
        git_in(&dir, &["init", "-q"])?;
        git_in(&dir, &["config", "user.email", "test@walgit"])?;
        git_in(&dir, &["config", "user.name", "walgit test"])?;
        git_in(&dir, &["config", "commit.gpgsign", "false"])?;

        let mut stream = Vec::with_capacity(commits.saturating_mul(files).saturating_mul(64));
        let mut next_mark = 1_u64;
        let mut parent = None;
        for c in 0..commits {
            // Reuse one deterministic blob for the files in this revision;
            // the M-file tree shape is unchanged while object creation stays
            // bounded by commits rather than commits × files.
            let mark = next_mark;
            next_mark += 1;
            let content = format!("content {c}\n");
            stream.extend_from_slice(b"blob\n");
            stream.extend_from_slice(format!("mark :{mark}\n").as_bytes());
            stream.extend_from_slice(format!("data {}\n", content.len()).as_bytes());
            stream.extend_from_slice(content.as_bytes());
            stream.push(b'\n');
            let blobs: Vec<_> = (0..files)
                .map(|f| (mark, format!("f{c}_{f}.txt")))
                .collect();

            let commit_mark = next_mark;
            next_mark += 1;
            let timestamp = 1_262_304_000_u64 + c as u64 * 60;
            let message = format!("commit {c}\n");
            stream.extend_from_slice(b"commit refs/heads/master\n");
            stream.extend_from_slice(format!("mark :{commit_mark}\n").as_bytes());
            stream.extend_from_slice(
                format!("author walgit test <test@walgit> {timestamp} +0000\n").as_bytes(),
            );
            stream.extend_from_slice(
                format!("committer walgit test <test@walgit> {timestamp} +0000\n").as_bytes(),
            );
            stream.extend_from_slice(format!("data {}\n", message.len()).as_bytes());
            stream.extend_from_slice(message.as_bytes());
            if let Some(parent) = parent {
                stream.extend_from_slice(format!("from :{parent}\n").as_bytes());
            }
            for (mark, path) in blobs {
                stream.extend_from_slice(format!("M 100644 :{mark} {path}\n").as_bytes());
            }
            stream.push(b'\n');
            parent = Some(commit_mark);
        }
        stream.extend_from_slice(b"done\n");

        let mut child = Command::new("git")
            .args(["fast-import", "--quiet", "--done"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning git fast-import")?;
        child
            .stdin
            .take()
            .context("git fast-import stdin")?
            .write_all(&stream)?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "git fast-import failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if commits > 0 {
            let head = git_in(&dir, &["rev-parse", "refs/heads/master"])?;
            git_in(&dir, &["update-ref", "refs/heads/main", head.trim()])?;
            git_in(&dir, &["update-ref", "-d", "refs/heads/master"])?;
            git_in(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
        Ok(Self { dir, _tmp: tmp })
    }
}

impl std::ops::Deref for TestRepo {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.dir
    }
}

/// Run `git` with `args` in `cwd`.
pub fn git(args: &[&str], cwd: &Path) -> Result<()> {
    let out = Command::new("git").current_dir(cwd).args(args).output()?;
    ensure!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Run `git` in `dir`, returning stdout.
pub fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").current_dir(dir).args(args).output()?;
    ensure!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `git <first> | git <second>` without a POSIX shell: the suite also runs on
/// Windows, where spawning `/bin/sh` simply fails. Collects the first
/// process's whole output, then feeds it to the second through its stdin — a
/// buffered hop avoids the flaky instant-EOF a live child-to-child pipe shows
/// on Windows (same shape as `crates/walgit-cli/src/testutil.rs`; each
/// integration binary is its own world, so the helper is duplicated per crate).
pub fn git_pipe(cwd: &Path, first: &[&str], second: &[&str]) -> std::process::Output {
    use std::io::Write;

    let up = Command::new("git")
        .args(first)
        .current_dir(cwd)
        .stderr(Stdio::piped())
        .output()
        .expect("run upstream git");
    assert!(
        up.status.success(),
        "git {first:?} failed: {} — stderr: {}",
        up.status,
        String::from_utf8_lossy(&up.stderr)
    );

    let mut down = Command::new("git")
        .args(second)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn downstream git");
    // The whole input is already in memory: write it, close stdin, then wait.
    // A write error means the child died early — its status says how.
    if let Some(mut stdin) = down.stdin.take() {
        let _ = stdin.write_all(&up.stdout);
    }
    let out = down.wait_with_output().expect("wait downstream git");
    assert!(
        out.status.success(),
        "git {second:?} failed: {} — stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
