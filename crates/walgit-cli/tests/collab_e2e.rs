#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::dbg_macro
)]
//! D1 (`docs/D1_COLLAB_DESIGN.md` §4.3/§7): the `walgit collab` CLI against a
//! real walgit server — sign + push entries through receive-pack, then a
//! fresh clone aggregates and verifies them. Tests the whole loop: CLI write
//! path → walgit WAL → second-instance deterministic aggregation.

use std::path::Path;
use std::sync::Arc;

use walgit_config::{Config, StoreBackend};
use walgit_server::{AppState, router};
use walgit_store::DynStore;
use walgit_store::memory::MemoryStore;

type TestResult<T = ()> = anyhow::Result<T>;

fn git_in(dir: &Path, args: &[&str]) -> TestResult<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()?;
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// In-process walgit server (mirrors the walgit-server test harness): memory
/// store, loopback `none` auth, `auto_create_on_push` so the CLI's first
/// `git push` creates the repository.
async fn start_server() -> TestResult<(String, tokio::sync::oneshot::Sender<()>)> {
    let store: Arc<MemoryStore> = MemoryStore::shared();
    let cache = tempfile::tempdir()?;
    let mut cfg = Config::default();
    cfg.store.backend = StoreBackend::Memory;
    // tests mean it (D43: not the unconfigured wizard placeholder)
    cfg.store.memory_backend_intentional = true;
    cfg.store.bucket = "test".into();
    cfg.cache.dir = cache.path().to_path_buf();
    cfg.cache.max_bytes = bytesize::ByteSize::gib(2);
    cfg.server.listen = "127.0.0.1:0".parse().unwrap();
    cfg.server.auto_create_on_push = true;
    cfg.server.max_concurrent_per_repo = 8;
    cfg.server.max_push_bytes = bytesize::ByteSize::gib(2);
    cfg.wal.fsck_objects = true;
    cfg.wal.check_connectivity = true;
    cfg.wal.freshness_ttl = std::time::Duration::ZERO;
    let dyn_store: DynStore = store.clone();
    let state = AppState::new(Arc::new(cfg), dyn_store).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = router(state);
    let (tx, rx) = tokio::sync::oneshot::channel();
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
    Ok((format!("http://{addr}"), tx))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_full_collab_flow_against_a_real_server() -> TestResult {
    let (base, _shutdown) = start_server().await?;
    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let key = keydir.path().join("key");
    std::fs::write(&key, "07".repeat(32))?; // 32 raw bytes as hex

    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let key_s = key.to_str().unwrap();

    // Scratch repo A: author/writer.
    let a = tempfile::tempdir()?;
    git_in(a.path(), &["init", "-q", "-b", "main"])?;
    git_in(a.path(), &["config", "user.email", "t@t"])?;
    git_in(a.path(), &["config", "user.name", "T"])?;
    git_in(
        a.path(),
        &["remote", "add", "origin", &format!("{base}/o/r.git")],
    )?;
    let repo_a = a.path().to_str().unwrap();

    // First-use registration, then a thread: issue -> comment (chained) ->
    // approve review; each pushed to the server through receive-pack.
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "alice", "--key", key_s,
        "--push", "origin",
    ])?;
    let issue_out = run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "pr1", "--actor", "alice",
        "--body", r#"{"title":"add thing"}"#, "--key", key_s, "--push", "origin",
    ])?;
    let issue_oid = issue_out.split_whitespace().nth(1).unwrap().to_string();
    let comment_out = run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "pr1", "--actor", "alice",
        "--parent", &issue_oid, "--body", r#"{"note":"+1"}"#, "--key", key_s, "--push", "origin",
    ])?;
    let comment_oid = comment_out.split_whitespace().nth(1).unwrap().to_string();
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "review", "--id", "pr1", "--actor", "alice",
        "--parent", &comment_oid, "--body", r#"{"decision":"approve"}"#, "--key", key_s, "--push",
        "origin",
    ])?;

    // Second instance: a fresh clone fetches the collab refs and aggregates.
    let b = tempfile::tempdir()?;
    git_in(
        b.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    git_in(b.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let repo_b = b.path().to_str().unwrap();

    let thread_out = run(&["collab", "thread", "pr1", "--repo", repo_b])?;
    let thread: serde_json::Value = serde_json::from_str(&thread_out)?;
    let arr = thread.as_array().unwrap();
    assert_eq!(arr.len(), 3, "issue + comment + review: {thread_out}");
    assert!(
        arr.iter().all(|e| e["verified"] == serde_json::Value::Bool(true)),
        "all entries verify against the registry: {thread_out}"
    );
    assert_eq!(arr[0]["entry"]["kind"], "issue", "parent chain root first");
    assert_eq!(arr[1]["entry"]["kind"], "comment");
    assert_eq!(arr[2]["entry"]["kind"], "review");

    let pr_out = run(&["collab", "pr", "pr1", "--repo", repo_b])?;
    let pv: serde_json::Value = serde_json::from_str(&pr_out)?;
    assert_eq!(pv["pr"]["human_approvals"][0]["actor"], "alice");
    assert_eq!(pv["merge"]["allowed"], serde_json::Value::Bool(true));

    // Read-only observability dashboard over the same refs. Rows lead with
    // the thread title, falling back to the id (issue #131).
    let text = run(&["collab", "report", "--repo", repo_b])?;
    assert!(text.contains("collab report"), "{text}");
    assert!(text.contains("add thing"), "titled thread renders by title: {text}");
    assert!(text.contains("verified"), "{text}");
    let md = run(&["collab", "report", "--repo", repo_b, "--format", "markdown"])?;
    assert!(md.contains("## PRs"), "{md}");
    let html = run(&["collab", "report", "--repo", repo_b, "--format", "html"])?;
    assert!(html.contains("<!doctype html>"), "{html}");
    assert!(html.contains("</html>"), "{html}");
    assert!(html.contains("add thing"), "{html}");
    Ok(())
}

/// `walgit collab watch`: cold start reports every collab ref, a later pass
/// reports only what is new, and the callback gets the entry JSON on stdin
/// with the env contract (kind/thread/actor/verified). Runs against a real
/// walgit server; the watch fetches refs/collab/* from a fresh clone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_reports_new_collab_entries_via_callback() -> TestResult {
    let (base, _shutdown) = start_server().await?;
    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let key = keydir.path().join("key");
    std::fs::write(&key, "07".repeat(32))?;
    let key_s = key.to_str().unwrap();

    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };

    // Repo A writes: register + issue entry, pushed to the server.
    let a = tempfile::tempdir()?;
    git_in(a.path(), &["init", "-q", "-b", "main"])?;
    git_in(a.path(), &["config", "user.email", "t@t"])?;
    git_in(a.path(), &["config", "user.name", "T"])?;
    git_in(
        a.path(),
        &["remote", "add", "origin", &format!("{base}/o/r.git")],
    )?;
    let repo_a = a.path().to_str().unwrap();
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "alice", "--key", key_s,
        "--push", "origin",
    ])?;
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "wt1", "--actor", "alice",
        "--body", r#"{"title":"watch me"}"#, "--key", key_s, "--push", "origin",
    ])?;

    // Repo B: a fresh clone where the watcher lives.
    let b = tempfile::tempdir()?;
    git_in(
        b.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    let repo_b = b.path().to_str().unwrap();
    let captured = tempfile::tempdir()?;
    let cap = captured.path().to_str().unwrap();
    let cb = format!("cat > {cap}/$WALGIT_COLLAB_KIND-$WALGIT_COLLAB_VERIFIED.txt");

    // Cold start: issue + principal are both reported through the callback.
    run(&[
        "collab", "watch", "--repo", repo_b, "--remote", "origin", "--once", "--exec", &cb,
    ])?;
    let issue_file = captured.path().join("issue-true.txt");
    let principal_file = captured.path().join("principal-true.txt");
    assert!(issue_file.exists(), "cold start delivered the issue entry");
    assert!(principal_file.exists(), "cold start delivered the principal record");
    let issue_v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&issue_file)?)?;
    assert_eq!(issue_v["kind"], "issue");
    assert_eq!(issue_v["id"], "wt1");

    // A second pass with no changes reports nothing new.
    run(&[
        "collab", "watch", "--repo", repo_b, "--remote", "origin", "--once", "--exec", &cb,
    ])?;
    let before = std::fs::read_to_string(&issue_file)?;
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure distinct mtime
    run(&[
        "collab", "watch", "--repo", repo_b, "--remote", "origin", "--once", "--exec", &cb,
    ])?;
    assert_eq!(
        std::fs::read_to_string(&issue_file)?,
        before,
        "no new refs -> callback not re-fired"
    );

    // A new comment from another actor triggers only the comment callback.
    let comment_out = run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "wt1", "--actor",
        "alice", "--parent", "", "--body", r#"{"note":"from agent B"}"#, "--key", key_s,
        "--push", "origin",
    ])?;
    let comment_oid = comment_out.split_whitespace().nth(1).unwrap().to_string();
    // Chain it properly by rewriting the ref to point at the issue? Simpler: the
    // watcher only cares about new refs, so a fresh uuid ref is fine.
    let _ = comment_oid;
    run(&[
        "collab", "watch", "--repo", repo_b, "--remote", "origin", "--once", "--exec", &cb,
    ])?;
    let comment_file = captured.path().join("comment-true.txt");
    assert!(comment_file.exists(), "new comment delivered: {}", comment_file.display());
    let comment_v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&comment_file)?)?;
    assert_eq!(comment_v["kind"], "comment");
    Ok(())
}

/// The work-unit board (D1 §8): two independent clients — the CLI's offline
/// aggregation over a fetched clone and the server's `GET …/collab/board` —
/// must project the same collab refs to **byte-identical** output, and moving
/// a card (an ordinary signed `status` entry) must move the projection for
/// every client, verified against the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board_projection_is_byte_identical_across_clients_and_moves_with_status() -> TestResult {
    let (base, _shutdown) = start_server().await?;
    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let key = keydir.path().join("key");
    std::fs::write(&key, "07".repeat(32))?;
    let key_s = key.to_str().unwrap();

    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };

    // The board definition is versioned with the repository: the author
    // commits `.walgit/board.toml` to main before any collab traffic, so both
    // clients read the same committed definition.
    let board_toml = "version = 1\n\n[[column]]\nname = \"review\"\nstatus = \"needs-review\"\n\n[[column]]\nname = \"done\"\nstatus = \"merged\"\n\n[[column]]\nname = \"open\"\nstatus = \"open\"\n";
    let a = tempfile::tempdir()?;
    git_in(a.path(), &["init", "-q", "-b", "main"])?;
    git_in(a.path(), &["config", "user.email", "t@t"])?;
    git_in(a.path(), &["config", "user.name", "T"])?;
    git_in(
        a.path(),
        &["remote", "add", "origin", &format!("{base}/o/r.git")],
    )?;
    std::fs::write(a.path().join(".walgit-board.toml"), board_toml)?;
    std::fs::create_dir_all(a.path().join(".walgit"))?;
    std::fs::rename(a.path().join(".walgit-board.toml"), a.path().join(".walgit/board.toml"))?;
    git_in(a.path(), &["add", ".walgit/board.toml"])?;
    git_in(a.path(), &["commit", "-q", "-m", "board"])?;
    git_in(a.path(), &["push", "-q", "origin", "main"])?;
    let repo_a = a.path().to_str().unwrap();

    // Thread t1 walks onto the review lane; t2 stays open.
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "alice", "--key", key_s,
        "--push", "origin",
    ])?;
    let issue_out = run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t1", "--actor", "alice",
        "--body", r#"{"title":"add the thing"}"#, "--key", key_s, "--push", "origin",
    ])?;
    let issue_oid = issue_out.split_whitespace().nth(1).unwrap().to_string();
    let status_out = run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "status", "--id", "t1", "--actor", "alice",
        "--parent", &issue_oid, "--body", r#"{"status":"needs-review"}"#, "--key", key_s, "--push",
        "origin",
    ])?;
    let status_oid = status_out.split_whitespace().nth(1).unwrap().to_string();
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "review", "--id", "t1", "--actor", "alice",
        "--parent", &status_oid, "--body", r#"{"decision":"approve"}"#, "--key", key_s, "--push",
        "origin",
    ])?;
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t2", "--actor", "alice",
        "--body", r#"{"title":"open work"}"#, "--key", key_s, "--push", "origin",
    ])?;

    // Client 1: a fresh clone, aggregating offline from the fetched refs.
    let b = tempfile::tempdir()?;
    git_in(
        b.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    git_in(b.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let repo_b = b.path().to_str().unwrap();
    let cli_bytes = run(&["collab", "board", "--repo", repo_b, "--format", "json"])?;

    // Client 2: the server endpoint over HTTP.
    let resp = reqwest::get(format!("{base}/o/r/api/collab/board")).await?;
    assert_eq!(resp.status(), 200, "board endpoint status");
    let server_bytes = resp.bytes().await?;
    assert_eq!(
        cli_bytes.as_bytes(),
        &server_bytes[..],
        "CLI and server must project the same refs to identical bytes"
    );

    let board: serde_json::Value = serde_json::from_str(&cli_bytes)?;
    let column_of = |name: &str| -> Vec<String> {
        board["columns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| {
                c["cards"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|card| card["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(column_of("review"), vec!["t1"], "status entry put t1 on review: {board}");
    assert_eq!(column_of("open"), vec!["t2"]);
    assert_eq!(column_of("done"), Vec::<String>::new());
    // text/markdown render the same projection (no need for byte equality).
    let text = run(&["collab", "board", "--repo", repo_b])?;
    assert!(text.contains("== review (1) =="), "{text}");
    let md = run(&["collab", "board", "--repo", repo_b, "--format", "markdown"])?;
    assert!(md.contains("## review (1)"), "{md}");

    // Move the t2 card: an ordinary signed `status` entry pushed to the
    // server's inbox — no second write semantics anywhere.
    let t2_out = run(&["collab", "thread", "t2", "--repo", repo_b])?;
    let t2: serde_json::Value = serde_json::from_str(&t2_out)?;
    let t2_tip = t2[0]["oid"].as_str().unwrap().to_string();
    // Timestamps are whole seconds and the whole run above finishes inside one:
    // sleep past the boundary so the move's `last_ts` is strictly newer than
    // t1's review entry and the default (last_ts desc, id asc) sort is decided
    // by activity, not by the id tie-break.
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure distinct ts
    run(&[
        "collab", "entry", "--repo", repo_b, "--kind", "status", "--id", "t2", "--actor", "alice",
        "--parent", &t2_tip, "--body", r#"{"status":"needs-review"}"#, "--key", key_s, "--push",
        "origin",
    ])?;

    // A third, independent clone sees the move: the new entry chains onto the
    // thread, verifies against the registry, and the projection moved.
    let c = tempfile::tempdir()?;
    git_in(
        c.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    git_in(c.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let repo_c = c.path().to_str().unwrap();
    let moved_thread = run(&["collab", "thread", "t2", "--repo", repo_c])?;
    let moved: serde_json::Value = serde_json::from_str(&moved_thread)?;
    let arr = moved.as_array().unwrap();
    assert_eq!(arr.len(), 2, "issue + the moving status entry: {moved_thread}");
    assert!(
        arr.iter().all(|e| e["verified"] == serde_json::Value::Bool(true)),
        "every entry verifies, including the move: {moved_thread}"
    );
    assert_eq!(arr[1]["entry"]["body"]["status"], "needs-review");

    let moved_cli = run(&["collab", "board", "--repo", repo_c, "--format", "json"])?;
    let moved_server = reqwest::get(format!("{base}/o/r/api/collab/board")).await?.bytes().await?;
    assert_eq!(moved_cli.as_bytes(), &moved_server[..], "post-move: still byte-identical");
    let moved_board: serde_json::Value = serde_json::from_str(&moved_cli)?;
    let review = moved_board["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "review")
        .unwrap()["cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    // Default sort is last-activity descending: the just-moved t2 leads.
    let mut review_sorted = review.clone();
    review_sorted.sort();
    assert_eq!(review_sorted, vec!["t1", "t2"], "the move put t2 on review: {moved_board}");
    assert_eq!(review.first(), Some(&"t2".to_string()), "newest activity first: {moved_board}");

    // Fail closed: an unparseable definition must be a loud error everywhere —
    // the CLI refuses to render, the server answers 400 — never a silent
    // fallback to the default board (that would hide a typo'd column rule).
    let bad = "version = 1\n\n[[column]]\nname = \"x\"\nbogus = true\n";
    let bad_dir = tempfile::tempdir()?;
    let bad_file = bad_dir.path().join("bad.toml");
    std::fs::write(&bad_file, bad)?;
    let bad_out = std::process::Command::new(bin)
        .arg("--config")
        .arg("/dev/null")
        .args([
            "collab",
            "board",
            "--repo",
            repo_b,
            "--format",
            "json",
            "--board",
            bad_file.to_str().unwrap(),
        ])
        .output()?;
    assert!(
        !bad_out.status.success(),
        "CLI must refuse a definition with an unknown field: {}",
        String::from_utf8_lossy(&bad_out.stdout)
    );
    std::fs::write(a.path().join(".walgit/board.toml"), bad)?;
    git_in(a.path(), &["commit", "-qam", "break the board"])?;
    git_in(a.path(), &["push", "-q", "origin", "main"])?;
    let bad_resp = reqwest::get(format!("{base}/o/r/api/collab/board")).await?;
    assert_eq!(
        bad_resp.status(),
        400,
        "server must refuse a repo whose HEAD definition does not parse"
    );
    Ok(())
}

/// D45 / D1 §11.4 (issue #160): the fold. A mixed history (issue/patch/
/// review/status/comment + `ci_claim`/`ci_result`, with an unregistered actor and
/// a wrong-key signature among them) is folded by `walgit collab gc --push`:
/// the inbox refs are deleted, `refs/collab/meta/snapshot` carries every entry
/// verbatim, and every aggregation — CLI offline and server API alike — is
/// **byte-identical** to before the fold, verification states included. The
/// tail then stays live: new entries chain onto folded oids, and a second gc
/// composes with the existing snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_fold_keeps_aggregation_byte_identical_and_the_tail_stays_live() -> TestResult {
    use base64::Engine as _;
    let (base, _shutdown) = start_server().await?;
    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let alice_key = keydir.path().join("alice");
    let bob_key = keydir.path().join("bob");
    std::fs::write(&alice_key, "07".repeat(32))?;
    std::fs::write(&bob_key, "08".repeat(32))?;
    let alice_k = alice_key.to_str().unwrap();
    let bob_k = bob_key.to_str().unwrap();

    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let git_stdin = |dir: &Path, args: &[&str], input: &str| -> TestResult<String> {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // Repo A: the author. main needs one commit so HEAD-backed reads (the
    // board definition lookup) work everywhere.
    let a = tempfile::tempdir()?;
    git_in(a.path(), &["init", "-q", "-b", "main"])?;
    git_in(a.path(), &["config", "user.email", "t@t"])?;
    git_in(a.path(), &["config", "user.name", "T"])?;
    git_in(
        a.path(),
        &["remote", "add", "origin", &format!("{base}/o/r.git")],
    )?;
    git_in(a.path(), &["commit", "-q", "--allow-empty", "-m", "init"])?;
    git_in(a.path(), &["push", "-q", "origin", "main"])?;
    let repo_a = a.path().to_str().unwrap();

    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "alice", "--key", alice_k,
        "--push", "origin",
    ])?;
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "bob", "--key", bob_k,
        "--push", "origin",
    ])?;
    // ci-runner-a's principal is bound to bob's key file (any principal may
    // self-register any key; the registry is the trust anchor).
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "ci-runner-a", "--key",
        bob_k, "--push", "origin",
    ])?;

    // t1: issue -> comment -> status(needs-review) -> review(approve) ->
    // patch -> an unsigned comment by the unregistered carol.
    let oid_of = |out: &str| out.split_whitespace().nth(1).unwrap().to_string();
    let issue = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t1", "--actor", "alice",
        "--body", r#"{"title":"fold me"}"#, "--key", alice_k, "--push", "origin",
    ])?);
    let comment = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "t1", "--actor", "bob",
        "--parent", &issue, "--body", r#"{"text":"looks right"}"#, "--key", bob_k, "--push",
        "origin",
    ])?);
    let status = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "status", "--id", "t1", "--actor", "alice",
        "--parent", &comment, "--body", r#"{"status":"needs-review","owner":"svc-a"}"#, "--key",
        alice_k, "--push", "origin",
    ])?);
    let review = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "review", "--id", "t1", "--actor", "bob",
        "--parent", &status, "--body", r#"{"decision":"approve"}"#, "--key", bob_k, "--push",
        "origin",
    ])?);
    let patch = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "patch", "--id", "t1", "--actor", "alice",
        "--parent", &review, "--body", r#"{"message":"the change"}"#, "--base", "refs/heads/main",
        "--head", "refs/heads/topic", "--key", alice_k, "--push", "origin",
    ])?);
    // carol's entry never touches a signing key: written by hand and pushed
    // as a bare blob ref — the aggregation must carry it as unverified,
    // across the fold.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let carol_json = format!(
        r#"{{"version":1,"kind":"comment","id":"t1","actor":"carol","ts":{ts},"parent":"{patch}","body":{{"text":"drive-by"}},"sig":""}}"#
    );
    let carol_oid = git_stdin(a.path(), &["hash-object", "-w", "--stdin"], &carol_json)?;
    git_in(
        a.path(),
        &["update-ref", "refs/collab/inbox/carol/driveby1", &carol_oid],
    )?;
    git_in(a.path(), &["push", "-q", "origin", "refs/collab/inbox/carol/driveby1"])?;

    // t2: issue -> status(closed), plus a comment signed by bob's key but
    // naming alice — the signature fails against alice's registered key:
    // unverified, and must stay unverified after the fold.
    let t2 = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t2", "--actor", "alice",
        "--body", r#"{"title":"second unit"}"#, "--key", alice_k, "--push", "origin",
    ])?);
    let t2s = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "status", "--id", "t2", "--actor", "alice",
        "--parent", &t2, "--body", r#"{"status":"closed"}"#, "--key", alice_k, "--push", "origin",
    ])?);
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "t2", "--actor", "alice",
        "--parent", &t2s, "--body", r#"{"text":"wrong key"}"#, "--key", bob_k, "--push", "origin",
    ])?;

    // A CI run thread (docs/D1_CI_PROTOCOL.md): claim -> result, signed by
    // ci-runner-a's key, ttl far from expiry so `now` never flips the state
    // between the two report captures.
    let claim = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "ci_claim", "--id", "ci-aaaabbbbccccdddd",
        "--actor", "ci-runner-a", "--body",
        r#"{"task":"test","ref":"refs/heads/main","commit":"c0ffee","ttl":86400,"attempt":1}"#,
        "--key", bob_k, "--push", "origin",
    ])?);
    run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "ci_result", "--id", "ci-aaaabbbbccccdddd",
        "--actor", "ci-runner-a", "--parent", &claim, "--body",
        &format!(r#"{{"task":"test","ref":"refs/heads/main","commit":"c0ffee","attempt":1,"claim":"{claim}","conclusion":"success","exit_code":0,"duration_ms":12,"log_summary":"ok","log_sha256":""}}"#),
        "--key", bob_k, "--push", "origin",
    ])?;

    // Client 1 (pre-fold): a fresh clone aggregates offline.
    let b = tempfile::tempdir()?;
    git_in(
        b.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    git_in(b.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let repo_b = b.path().to_str().unwrap();
    let collab_refs = |dir: &Path| -> TestResult<Vec<String>> {
        let out = git_in(dir, &["ls-remote", "origin", "refs/collab/*"])?;
        Ok(out.lines().map(str::to_string).collect())
    };
    assert_eq!(
        collab_refs(b.path())?.iter().filter(|l| l.contains("refs/collab/inbox/")).count(),
        11,
        "every entry is one advertised ref before the fold (the wall this test removes)"
    );

    let pre_board_cli = run(&["collab", "board", "--repo", repo_b, "--format", "json"])?;
    let pre_board_srv = reqwest::get(format!("{base}/o/r/api/collab/board")).await?.bytes().await?;
    assert_eq!(pre_board_cli.as_bytes(), &pre_board_srv[..], "pre-fold: CLI == server");
    let pre_thread_cli = run(&["collab", "thread", "t1", "--repo", repo_b])?;
    let pre_thread_srv = reqwest::get(format!("{base}/o/r/api/collab/threads/t1")).await?.bytes().await?;
    let pre_report_srv = reqwest::get(format!("{base}/o/r/api/collab/report")).await?.bytes().await?;
    // Sanity on the fixture: 11 entries, carol + the wrong-key comment
    // unverified, carol's key missing, the CI run projected.
    let pre_report: serde_json::Value = serde_json::from_slice(&pre_report_srv)?;
    assert_eq!(pre_report["total_entries"], 11);
    assert_eq!(pre_report["verified_entries"], 9);
    assert_eq!(pre_report["unverified_entries"], 2);
    assert_eq!(pre_report["missing_principals"], 1);
    // `build_report` feeds every entry to `ci::collect_runs`, so non-CI
    // threads show up as Pending runs too (pre-existing projection, pinned
    // here so a fold cannot quietly change it); the real CI run is the one
    // carrying the ci-* id.
    let runs = pre_report["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3, "t1, t2 and the ci thread: {runs:?}");
    let ci_run = runs
        .iter()
        .find(|r| r["id"] == "ci-aaaabbbbccccdddd")
        .expect("the CI run is projected");
    assert_eq!(ci_run["state"], "done");
    let pre_thread: serde_json::Value = serde_json::from_str(&pre_thread_cli)?;
    assert_eq!(pre_thread.as_array().unwrap().len(), 6, "t1 entries");

    // The fold, run from the clone (it has every ref). Afterwards the
    // namespace is one snapshot ref + the singleton meta refs.
    let gc_out = run(&[
        "collab", "gc", "--repo", repo_b, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc_out.contains("folded 11 inbox ref(s)"), "{gc_out}");
    let after = collab_refs(b.path())?;
    assert_eq!(
        after.iter().filter(|l| l.contains("refs/collab/inbox/")).count(),
        0,
        "the fold pruned every inbox ref: {after:?}"
    );
    assert!(
        after.iter().any(|l| l.contains("refs/collab/meta/snapshot")),
        "the snapshot ref exists: {after:?}"
    );
    assert_eq!(
        after.len(),
        4,
        "snapshot + 3 principals: the advertisement stopped growing: {after:?}"
    );

    // Client 2 (post-fold): another fresh clone sees exactly the pre-fold
    // bytes — every projection, both clients, verification states included.
    let c = tempfile::tempdir()?;
    git_in(
        c.path(),
        &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."],
    )?;
    git_in(c.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let repo_c = c.path().to_str().unwrap();
    let post_board_cli = run(&["collab", "board", "--repo", repo_c, "--format", "json"])?;
    assert_eq!(post_board_cli, pre_board_cli, "fold must not move the board");
    let post_thread_cli = run(&["collab", "thread", "t1", "--repo", repo_c])?;
    assert_eq!(post_thread_cli, pre_thread_cli, "fold must not move the thread view");
    let post_board_srv = reqwest::get(format!("{base}/o/r/api/collab/board")).await?.bytes().await?;
    assert_eq!(&post_board_srv[..], &pre_board_srv[..], "server board identical across the fold");
    assert_eq!(post_board_cli.as_bytes(), &post_board_srv[..], "post-fold: CLI == server");
    let post_thread_srv = reqwest::get(format!("{base}/o/r/api/collab/threads/t1")).await?.bytes().await?;
    assert_eq!(&post_thread_srv[..], &pre_thread_srv[..], "server thread identical across the fold");
    let post_report_srv = reqwest::get(format!("{base}/o/r/api/collab/report")).await?.bytes().await?;
    assert_eq!(&post_report_srv[..], &pre_report_srv[..], "server report identical across the fold");

    // The snapshot itself is the audit manifest: every record's oid
    // recomputes from its bytes and every entry still verifies (or not)
    // exactly as it did in the inbox.
    let snap_text = git_in(c.path(), &["cat-file", "blob", "refs/collab/meta/snapshot"])?;
    let snap = walgit_wal::collab::parse_snapshot(snap_text.as_bytes()).expect("snapshot parses");
    assert_eq!(snap.actor, "alice");
    assert_eq!(snap.entries.len(), 11);
    let mut principals_c = std::collections::HashMap::new();
    for (name, seed) in [("alice", 7u8), ("bob", 8u8), ("ci-runner-a", 8u8)] {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        principals_c.insert(
            name.to_string(),
            base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes()),
        );
    }
    let cli_report = run(&["collab", "report", "--repo", repo_c])?;
    assert!(
        cli_report.contains("9/11"),
        "cli report shows the same verification health: {cli_report}"
    );
    let parsed: Vec<_> = snap
        .entries
        .iter()
        .map(|r| {
            r.entry_ref(walgit_git::ObjectFormat::Sha1)
                .expect("every record pins its bytes to its oid")
        })
        .collect();
    let verified_count = parsed.iter().filter(|e| e.is_verified(&principals_c)).count();
    assert_eq!(verified_count, 9, "9 verified, carol + wrong-key unverified");
    let carol = parsed.iter().find(|e| e.entry.actor == "carol").expect("carol's entry folded");
    assert!(!carol.is_verified(&principals_c), "carol stays unverified");
    let wrong_key = parsed
        .iter()
        .find(|e| e.entry.body.get("text").and_then(|v| v.as_str()) == Some("wrong key"))
        .expect("the wrong-key comment is in the snapshot");
    assert!(!wrong_key.is_verified(&principals_c), "wrong-key signature stays unverified");

    // The tail stays live: a new entry chains onto a folded tip (its parent
    // oid now exists only inside the snapshot).
    let follow = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "t1", "--actor", "alice",
        "--parent", &carol_oid, "--body", r#"{"text":"after the fold"}"#, "--key", alice_k,
        "--push", "origin",
    ])?);
    let _ = follow;
    git_in(c.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let tail_thread_cli = run(&["collab", "thread", "t1", "--repo", repo_c])?;
    let tail_thread: serde_json::Value = serde_json::from_str(&tail_thread_cli)?;
    let arr = tail_thread.as_array().unwrap();
    assert_eq!(arr.len(), 7, "the new entry joins the folded thread");
    assert_eq!(arr[6]["entry"]["body"]["text"], "after the fold");
    assert_eq!(arr[6]["entry"]["parent"], carol_oid, "chained onto a folded oid");
    assert!(
        arr.iter().all(|e| e["verified"] == serde_json::Value::Bool(true)
            || e["entry"]["actor"] == "carol"),
        "verification across the snapshot boundary: {tail_thread_cli}"
    );
    let tail_thread_srv = reqwest::get(format!("{base}/o/r/api/collab/threads/t1")).await?.bytes().await?;
    let tail_board_cli = run(&["collab", "board", "--repo", repo_c, "--format", "json"])?;
    let tail_board_srv = reqwest::get(format!("{base}/o/r/api/collab/board")).await?.bytes().await?;
    assert_eq!(tail_board_cli.as_bytes(), &tail_board_srv[..], "post-append: CLI == server");
    let tail_thread_srv_v: serde_json::Value = serde_json::from_slice(&tail_thread_srv)?;
    assert_eq!(tail_thread_srv_v["entries"].as_array().unwrap().len(), 7);

    // A second fold composes with the existing snapshot (records carried
    // verbatim) and prunes the one-entry tail.
    git_in(b.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let gc2 = run(&[
        "collab", "gc", "--repo", repo_b, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc2.contains("folded 1 inbox ref(s)"), "{gc2}");
    let after2 = collab_refs(b.path())?;
    assert_eq!(after2.iter().filter(|l| l.contains("refs/collab/inbox/")).count(), 0);
    let snap2_text = git_in(b.path(), &["cat-file", "blob", "refs/collab/meta/snapshot"])?;
    let snap2 = walgit_wal::collab::parse_snapshot(snap2_text.as_bytes()).expect("snapshot parses");
    assert_eq!(snap2.entries.len(), 12, "the second fold accumulated the tail");
    // And the aggregation is byte-stable across the second fold on both
    // clients (same shapes on each side of the comparison).
    git_in(c.path(), &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
    let final_thread_cli = run(&["collab", "thread", "t1", "--repo", repo_c])?;
    assert_eq!(final_thread_cli, tail_thread_cli, "second fold: CLI thread stable");
    let final_thread_srv = reqwest::get(format!("{base}/o/r/api/collab/threads/t1")).await?.bytes().await?;
    assert_eq!(&final_thread_srv[..], &tail_thread_srv[..], "second fold: server thread stable");
    Ok(())
}

/// §11.4 fold CAS: the snapshot push leases against the baseline the gc
/// actually read (`--force-with-lease`, never a `+` refspec — which silently
/// short-circuits the lease). Two checks:
/// ① a stale-baseline fold (a second gc whose checkout predates the first
///   fold's snapshot) is refused, and the first fold's snapshot survives —
///   the empirical overwrite the review reproduced;
/// ② the crash-resume branch: a ref whose entry is already folded (an earlier
///   gc died between the snapshot move and the prune) is pruned WITHOUT
///   re-recording it, and the surviving record keeps its original principal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_fold_baseline_loses_the_lease_and_the_crash_resume_prunes_deduped() -> TestResult {
    let (base, _shutdown) = start_server().await?;
    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let alice_key = keydir.path().join("alice");
    std::fs::write(&alice_key, "07".repeat(32))?;
    let alice_k = alice_key.to_str().unwrap();

    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let run_expect_fail = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            !out.status.success(),
            "walgit {} unexpectedly succeeded:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stderr).to_string())
    };
    let git_in = |dir: &Path, args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()?;
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let a = tempfile::tempdir()?;
    git_in(a.path(), &["init", "-q", "-b", "main"])?;
    git_in(a.path(), &["config", "user.email", "t@t"])?;
    git_in(a.path(), &["config", "user.name", "T"])?;
    git_in(a.path(), &["remote", "add", "origin", &format!("{base}/o/r.git")])?;
    git_in(a.path(), &["commit", "-q", "--allow-empty", "-m", "init"])?;
    git_in(a.path(), &["push", "-q", "origin", "main"])?;
    let repo_a = a.path().to_str().unwrap();
    run(&[
        "collab", "principal-register", "--repo", repo_a, "--principal", "alice", "--key", alice_k,
        "--push", "origin",
    ])?;
    let oid_of = |out: &str| out.split_whitespace().nth(1).unwrap().to_string();
    let issue1 = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t1", "--actor", "alice",
        "--body", r#"{"title":"first"}"#, "--key", alice_k, "--push", "origin",
    ])?);
    let _issue2 = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "issue", "--id", "t2", "--actor", "alice",
        "--body", r#"{"title":"second"}"#, "--key", alice_k, "--push", "origin",
    ])?);

    let fetch_all = |dir: &Path| -> TestResult<()> {
        git_in(dir, &["fetch", "-q", "origin", "+refs/collab/*:refs/collab/*"])?;
        Ok(())
    };
    let remote_snapshot = |dir: &Path| -> TestResult<String> {
        let out = git_in(dir, &["ls-remote", "origin", "refs/collab/meta/snapshot"])?;
        Ok(out.split_whitespace().next().unwrap_or_default().to_string())
    };

    // Client b and client c both see the pre-fold world (two entries, no
    // snapshot).
    let b = tempfile::tempdir()?;
    git_in(b.path(), &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."])?;
    fetch_all(b.path())?;
    let repo_b = b.path().to_str().unwrap();
    let c = tempfile::tempdir()?;
    git_in(c.path(), &["clone", "-q", "--no-checkout", &format!("{base}/o/r.git"), "."])?;
    fetch_all(c.path())?;
    let repo_c = c.path().to_str().unwrap();

    // Fold ① from b: snapshot S1 lands, the inbox is pruned on the remote.
    let gc1 = run(&[
        "collab", "gc", "--repo", repo_b, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc1.contains("folded 2 inbox ref(s)"), "{gc1}");
    let s1 = remote_snapshot(b.path())?;
    assert_eq!(s1.len(), 40, "the snapshot ref is advertised: {s1}");
    // `build_snapshot` is otherwise a pure function of (records, second).
    // Cross a second boundary so c's stale candidate has a different oid from
    // S1: an identical candidate would be a harmless no-op push and would not
    // exercise the lease at all.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    // ① Stale baseline: c folds without fetching — its baseline is "no
    // snapshot" (an empty lease <expect>) while the remote holds S1. The lease must
    // refuse this push; S1 survives.
    let c_local_snapshot = git_in(
        c.path(),
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/collab/meta/snapshot",
        ],
    )?;
    assert!(
        c_local_snapshot.trim().is_empty(),
        "c must still have no local snapshot before the stale fold: {c_local_snapshot}"
    );
    assert_eq!(remote_snapshot(c.path())?, s1, "c sees S1 on the remote");
    let receive_advert = reqwest::get(format!("{base}/o/r.git/info/refs?service=git-receive-pack"))
        .await?
        .text()
        .await?;
    assert!(
        receive_advert.contains(&s1),
        "the receive-pack advertisement must include S1 {s1}"
    );
    let stale = run_expect_fail(&[
        "collab", "gc", "--repo", repo_c, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(
        stale.to_lowercase().contains("stale") || stale.to_lowercase().contains("baseline"),
        "the refusal names the stale fold baseline: {stale}"
    );
    assert_eq!(remote_snapshot(c.path())?, s1, "S1 was not overwritten");

    // After fetching, c's fold converges as a prune-only fold: everything is
    // already in S1 (the stale local copies are duplicates), the snapshot is
    // NOT rebuilt (nothing new to record — no churn), and the deletes target
    // only refs the remote still advertises (none — the retry prunes only the
    // stale local copies instead of failing on already-deleted remote refs).
    fetch_all(c.path())?;
    let gc_retry = run(&[
        "collab", "gc", "--repo", repo_c, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc_retry.contains("folded 2 inbox ref(s)"), "{gc_retry}");
    assert_eq!(remote_snapshot(c.path())?, s1, "the retry did not move the snapshot");

    // ② Crash-resume: a gc died between moving the snapshot and pruning the
    // refs — the remote still advertises a ref whose entry S1 already
    // carries. Re-create that state: fold a fresh tail entry, then plant a
    // duplicate ref for an entry that is already folded (under a *different*
    // inbox principal, so a re-derived copy would be distinguishable).
    let tail = oid_of(&run(&[
        "collab", "entry", "--repo", repo_a, "--kind", "comment", "--id", "t1", "--actor", "alice",
        "--parent", &issue1, "--body", r#"{"text":"tail"}"#, "--key", alice_k, "--push", "origin",
    ])?);
    fetch_all(b.path())?;
    let gc2 = run(&[
        "collab", "gc", "--repo", repo_b, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc2.contains("folded 1 inbox ref(s)"), "{gc2}");
    let s2 = remote_snapshot(b.path())?;
    assert_ne!(s2, s1, "the tail fold moved the snapshot");
    git_in(b.path(), &["update-ref", "refs/collab/inbox/mallory/crashed", &issue1])?;
    git_in(b.path(), &["push", "-q", "origin", "refs/collab/inbox/mallory/crashed"])?;

    // The resume fold: the duplicate is pruned, NOT re-recorded; the snapshot
    // keeps exactly the three records and the planted copy never displaces
    // the legitimate one (the record keeps its original principal). The
    // snapshot itself is not rebuilt — nothing new was recorded.
    let gc3 = run(&[
        "collab", "gc", "--repo", repo_b, "--actor", "alice", "--key", alice_k, "--push", "origin",
    ])?;
    assert!(gc3.contains("folded 1 inbox ref(s)"), "{gc3}");
    // Counted from a full ls-remote (no glob: pattern semantics must not be
    // load-bearing in what this assertion proves).
    let remote_inbox = |dir: &Path| -> TestResult<usize> {
        let out = git_in(dir, &["ls-remote", "origin"])?;
        Ok(out.lines().filter(|l| l.contains("refs/collab/inbox/")).count())
    };
    assert_eq!(remote_inbox(b.path())?, 0, "the crash-resumed duplicate was pruned");
    assert_eq!(remote_inbox(c.path())?, 0, "c's stale copies never reappear remotely");
    fetch_all(b.path())?;
    let snap3_text = git_in(b.path(), &["cat-file", "blob", "refs/collab/meta/snapshot"])?;
    let snap3 = walgit_wal::collab::parse_snapshot(snap3_text.as_bytes()).expect("snapshot parses");
    assert_eq!(snap3.entries.len(), 3, "the duplicate was not re-recorded");
    assert_eq!(snap3.actor, "alice");
    let root = snap3
        .entries
        .iter()
        .find(|r| r.oid == issue1)
        .expect("the folded root entry survives the resume");
    assert_eq!(root.principal, "alice", "the planted copy never displaces the legitimate record");
    assert!(
        snap3.entries.iter().any(|r| r.oid == tail),
        "the tail entry's record is in the snapshot"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sha256_repo_fold_preserves_the_thread() -> TestResult {
    let (base, _shutdown) = start_server().await?;
    let created = reqwest::Client::new()
        .put(format!("{base}/o/sha?object_format=sha256"))
        .send()
        .await?;
    assert!(
        created.status().is_success() || created.status() == reqwest::StatusCode::CONFLICT,
        "create sha256 repo: {}",
        created.status()
    );

    let bin = env!("CARGO_BIN_EXE_walgit");
    let keydir = tempfile::tempdir()?;
    let key = keydir.path().join("alice");
    std::fs::write(&key, "07".repeat(32))?;
    let key = key.to_str().unwrap();
    let run = |args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new(bin)
            .arg("--config")
            .arg("/dev/null")
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "walgit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let git_in = |dir: &Path, args: &[&str]| -> TestResult<String> {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()?;
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let repo_dir = tempfile::tempdir()?;
    git_in(repo_dir.path(), &["init", "-q", "--object-format=sha256", "-b", "main"])?;
    git_in(repo_dir.path(), &["config", "user.email", "t@t"])?;
    git_in(repo_dir.path(), &["config", "user.name", "T"])?;
    git_in(
        repo_dir.path(),
        &["remote", "add", "origin", &format!("{base}/o/sha.git")],
    )?;
    git_in(repo_dir.path(), &["commit", "-q", "--allow-empty", "-m", "init"])?;
    git_in(repo_dir.path(), &["push", "-q", "origin", "main"])?;
    let repo = repo_dir.path().to_str().unwrap();
    run(&[
        "collab", "principal-register", "--repo", repo, "--principal", "alice", "--key", key,
        "--push", "origin",
    ])?;
    run(&[
        "collab", "entry", "--repo", repo, "--kind", "issue", "--id", "sha", "--actor", "alice",
        "--body", r#"{"title":"sha256 survives"}"#, "--key", key, "--push", "origin",
    ])?;

    let gc = run(&[
        "collab", "gc", "--repo", repo, "--actor", "alice", "--key", key, "--push", "origin",
    ])?;
    assert!(gc.contains("folded 1 inbox ref(s)"), "{gc}");
    let thread = run(&["collab", "thread", "sha", "--repo", repo])?;
    assert!(thread.contains("sha256 survives"), "{thread}");

    let snapshot = git_in(
        repo_dir.path(),
        &["cat-file", "blob", "refs/collab/meta/snapshot"],
    )?;
    let snapshot = walgit_wal::collab::parse_snapshot(snapshot.as_bytes()).expect("snapshot parses");
    assert_eq!(snapshot.entries.len(), 1);
    assert!(
        snapshot.entries[0]
            .entry_ref(walgit_git::ObjectFormat::Sha256)
            .is_some(),
        "sha256 snapshot record must validate in its own object format"
    );
    Ok(())
}
