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
//! The `maintain` role's pass: checkpoint-if-due (refs-level, on an instance
//! that cannot hold the packs), bundles-if-due, compaction, all as tasks.

mod harness;

use harness::{Server, git, git_in, git_pipe};

/// Every await is bounded so a hang names the step instead of stalling CI.
macro_rules! step {
    ($name:literal, $e:expr) => {
        tokio::time::timeout(std::time::Duration::from_secs(30), $e)
            .await
            .unwrap_or_else(|_| panic!("step timed out: {}", $name))
    };
}

/// A timestamp clearly outside the test retention windows.
fn old_timestamp() -> prost_types::Timestamp {
    let mut ts = walgit_proto::time::now();
    ts.seconds -= 8 * 24 * 3600;
    ts
}

/// A valid one-entry log segment whose newest entry is `at`. GC now ages log
/// segments by their own content, so zero-filled fixtures would (correctly)
/// fail safe and never be reclaimed.
fn log_segment_bytes(seq: u64, at: &prost_types::Timestamp) -> bytes::Bytes {
    let entry = walgit_proto::v1::LogEntry {
        seq,
        created_at: Some(at.clone()),
        ..Default::default()
    };
    walgit_proto::frame::encode_entries(std::iter::once(&entry))
}

/// A valid checkpoint object carrying the age GC reads.
fn checkpoint_bytes(seq: u64, at: &prost_types::Timestamp) -> Vec<u8> {
    use prost::Message;
    walgit_proto::v1::Checkpoint {
        seq,
        refs_key: walgit_proto::keys::checkpoint_refs_key(seq),
        created_at: Some(at.clone()),
        ..Default::default()
    }
    .encode_to_vec()
}

/// A valid refs snapshot carrying the same age (used for crash remnants and
/// half-written directories).
fn refs_snapshot_bytes(at: &prost_types::Timestamp) -> Vec<u8> {
    use prost::Message;
    walgit_proto::v1::RefSnapshot {
        created_at: Some(at.clone()),
        ..Default::default()
    }
    .encode_to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pass_checkpoints_due_repos_refs_level_and_reports_tasks() -> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    // Writer front: count trigger off, so nothing auto-checkpoints on push.
    let front = step!("start front", Server::start())?;
    step!("put repo", front.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    for i in 0..3 {
        std::fs::write(src.path().join(format!("f{i}.txt")), format!("{i}\n"))?;
        git_in(src.path(), &["add", "."])?;
        git_in(src.path(), &["commit", "-q", "-m", &format!("c{i}")])?;
        git(
            &["push", "-q", &front.repo_url("o", "r"), "main"],
            src.path(),
        )?;
    }
    let m = step!(
        "open on front",
        front
            .state
            .registry
            .open(&walgit_git::RepoId::new("o", "r")?)
    )?
    .manifest();
    assert_eq!(m.head_seq, 3);
    assert!(
        m.checkpoint.is_none(),
        "no checkpoint yet: {:?}",
        m.checkpoint
    );

    // Maintainer: age trigger (1 ms) and a cache too small for any pack.
    let maint = step!(
        "start maintainer",
        front.start_sibling_with(|c| {
            c.server.roles = vec![walgit_config::Role::Maintain];
            c.cache.max_bytes = walgit_config::ByteSize::b(1);
            c.wal.snapshot_every_entries = 0;
            c.wal.checkpoint_interval = std::time::Duration::from_millis(1);
            c.compaction.enabled = false;
            c.bundles.enabled = false;
        })
    )?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let report = step!(
        "maintain pass 1",
        walgit_server::maintain::run_pass(&maint.state)
    )?;
    assert_eq!(report.repos, 1);
    assert_eq!(report.checkpoints, 1, "{report:?}");

    // Manifest folded; the task is discoverable on the maintainer.
    let h = step!(
        "open on maintainer",
        maint
            .state
            .registry
            .open(&walgit_git::RepoId::new("o", "r")?)
    )?;
    let m = h.manifest();
    assert_eq!(m.checkpoint.as_ref().map(|c| c.seq), Some(3));
    assert!(m.log_segments.is_empty());
    assert!(
        h.local().packs()?.is_empty(),
        "refs-level: no pack downloaded"
    );
    let tasks = step!("tasks list", maint.get_text("/o/r/api/tasks", &[]))?;
    assert!(tasks.contains("\"checkpoint\""), "{tasks}");
    assert!(
        tasks.contains("\"trigger\":\"age\"") || tasks.contains("age"),
        "{tasks}"
    );

    // Second pass: nothing due.
    let report = step!(
        "maintain pass 2",
        walgit_server::maintain::run_pass(&maint.state)
    )?;
    assert_eq!(report.checkpoints, 0);

    // Bundles from a maintainer that never served this repo: the build must
    // materialize the packs itself (prod failed with "bad object refs/heads/main").
    let bundler = step!(
        "start bundler",
        front.start_sibling_with(|c| {
            c.server.roles = vec![walgit_config::Role::Maintain];
            c.wal.snapshot_every_entries = 0;
            c.compaction.enabled = false;
            c.bundles.enabled = true;
        })
    )?;
    // Priority loop: the first unit is the missing weekly slot (checkpoint is
    // not due), one unit per pass, next pass moves to the daily chain, and a
    // re-run after everything is built is idempotent (Idle).
    let id = walgit_git::RepoId::new("o", "r")?;
    assert!(
        matches!(step!("unit 1", next_unit(&bundler.state, &id))?, Unit::BundleSlot(ref s, _) if s == "weekly")
    );
    let report = step!("bundler pass", run_pass(&bundler.state))?;
    assert_eq!((report.units, report.bundles), (1, 1), "{report:?}");
    let list = step!(
        "bundle list",
        bundler.get_text("/o/r.git/bundles/list", &[])
    )?;
    assert!(list.contains("[bundle \"weekly-"), "{list}");
    // Weekly token = its slot (a Sunday 23:00 UTC epoch, divisible by 3600).
    let tok: u64 = list
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("creationToken = ")
                .and_then(|v| v.parse().ok())
        })
        .unwrap();
    assert_eq!(tok % 3600, 0, "token is a slot epoch: {tok}");
    // Next units: dailies (oldest first) — but a daily slot with no new objects
    // over the weekly is skipped — then the lowest-priority audit (fsck, once;
    // clean), so the loop converges to Idle.
    let mut audits = 0;
    for _ in 0..40 {
        match step!("unit n", next_unit(&bundler.state, &id))? {
            Unit::Idle => break,
            Unit::BundleSlot(..) => {
                let _ = step!("pass n", run_pass(&bundler.state))?;
            }
            Unit::Fsck(_) => {
                audits += 1;
                let _ = step!("pass fsck", run_pass(&bundler.state))?;
            }
            other => panic!("unexpected unit {other:?}"),
        }
    }
    assert_eq!(
        audits, 1,
        "the audit runs once (never audited) and is then not due for fsck_interval"
    );
    assert_eq!(
        step!("unit idle", next_unit(&bundler.state, &id))?,
        Unit::Idle
    );
    let report = step!("idempotent pass", run_pass(&bundler.state))?;
    assert_eq!(report.units, 0, "{report:?}");
    // Placement by rule: a maintainer not assigned to the repo plans nothing.
    let elsewhere = step!(
        "start elsewhere",
        front.start_sibling_with(|c| {
            c.server.roles = vec![walgit_config::Role::Maintain];
            c.placement.maintain = vec!["acme/*".into()];
        })
    )?;
    assert_eq!(
        step!("not assigned", next_unit(&elsewhere.state, &id))?,
        Unit::NotAssigned
    );
    let report = step!("elsewhere pass", run_pass(&elsewhere.state))?;
    assert_eq!((report.repos, report.units), (0, 0));
    // Heartbeat: the maintainer writes maintain/<host>.pb.
    let excluded = step!(
        "start excluded",
        front.start_sibling_with(|c| {
            c.server.roles = vec![walgit_config::Role::Maintain];
            c.placement.maintain_exclude = vec!["o/r".into()];
        })
    )?;
    assert_eq!(
        step!("excluded", next_unit(&excluded.state, &id))?,
        Unit::NotAssigned
    );

    // The front sees the checkpoint and a fresh instance cold-starts from it.
    let cold = step!("start cold", front.start_sibling_with(|_| {}))?;
    let refs = cold.ls_remote("o", "r")?;
    let head = git_in(src.path(), &["rev-parse", "HEAD"])?;
    assert!(refs.contains(head.trim()), "{refs}");
    Ok(())
}

/// D28: a maintainer that excludes a repository is not its writer and refuses
/// the push up front (no sync, no pack read) naming the writer; the same host
/// accepts pushes for repositories it is assigned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintainer_refuses_pushes_for_excluded_repos() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![walgit_config::Role::Serve, walgit_config::Role::Maintain];
            c.placement.maintain_exclude = vec!["o/large".into()];
            c.maintenance.host = Some("broker".into());
        })
    )?;
    step!("put excluded", server.put_repo("o", "large"))?;
    step!("put assigned", server.put_repo("o", "small"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "hi\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "c"])?;

    let url = server.repo_url("o", "large");
    let out = std::process::Command::new("git")
        .args(["push", "--porcelain", &url, "main"])
        .current_dir(src.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "push must be refused: {text}");
    assert!(
        text.contains("o/large is written by"),
        "names the writer: {text}"
    );
    assert!(text.contains("refs/heads/main"), "ng per ref: {text}");
    let h = step!(
        "open",
        server
            .state
            .registry
            .open(&walgit_git::RepoId::new("o", "large")?)
    )?;
    assert_eq!(h.manifest().head_seq, 0, "nothing published");
    assert!(
        !server.registry_has_packs("o", "large").await,
        "no sync happened"
    );

    git(
        &["push", "-q", &server.repo_url("o", "small"), "main"],
        src.path(),
    )?;
    let h = step!(
        "open small",
        server
            .state
            .registry
            .open(&walgit_git::RepoId::new("o", "small")?)
    )?;
    assert_eq!(h.manifest().head_seq, 1);
    Ok(())
}

/// Integrity units (the original large-repository measurements, a large repository's 1,952 missing blobs): the
/// weekly `fsck` unit records missing objects at fsck.pb; the `repair` unit
/// fetches exactly those from `upstream.git` and publishes them as a pack; the
/// next `fsck` re-verifies clean. The upstream here is a second repository on
/// the same server (walgit serves wants by SHA when configured, like GitHub).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_unit_records_missing_objects_and_repair_unit_fetches_them_from_upstream()
-> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.git.allow_any_sha1_in_want = true;
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::from_secs(3600);
            c.maintenance.gc_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    step!("put upstream", server.put_repo("o", "up"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    std::fs::write(src.path().join("b.txt"), "the blob the import dropped\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let tree2 = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let blob2 = git_in(src.path(), &["rev-parse", "HEAD:b.txt"])?
        .trim()
        .to_string();
    // The upstream has everything.
    git(
        &["push", "-q", &server.repo_url("o", "up"), "main"],
        src.path(),
    )?;

    // The hole: publish commit 2 + its tree WITHOUT the new blob (a pack that is
    // not the closure of the ref), then move main onto it — exactly the import's
    // mistake, which receive-pack's connectivity check would have refused.
    let holes = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", holes.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c2}\n{tree2}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    step!(
        "add hole pack",
        h.add_pack(
            &holes.path().join(format!("pack-{sha}.pack")),
            &holes.path().join(format!("pack-{sha}.idx")),
            0,
            None
        )
    )?;
    step!("sync2", h.sync())?;
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1.clone(),
            new_oid: c2.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "move main",
        h.publish_push_synced(None, txn, std::collections::HashMap::default())
    )?;

    // Pass 1: the audit (never audited) → fsck.pb lists the blob; the unit succeeds (a finding, not a failure).
    let unit = step!(
        "plan 1",
        walgit_server::maintain::next_unit(&server.state, &id)
    )?;
    assert!(
        matches!(unit, walgit_server::maintain::Unit::Fsck(_)),
        "{unit:?}"
    );
    let report = step!("pass 1", walgit_server::maintain::run_pass(&server.state))?;
    assert_eq!(report.units, 2, "both repositories audited: {report:?}");
    let f = walgit_server::ops::read_fsck(&h)
        .await
        .unwrap()
        .expect("fsck.pb written");
    assert_eq!(f.missing, vec![blob2.clone()], "{f:?}");
    assert_eq!(f.repaired_seq, 0);

    // No upstream → nothing can repair; the plan says so (Idle, the audit is fresh).
    let unit = step!(
        "plan no upstream",
        walgit_server::maintain::next_unit(&server.state, &id)
    )?;
    assert_eq!(unit, walgit_server::maintain::Unit::Idle, "{unit:?}");

    // With upstream.git (D24 setting) the repair unit is next.
    let client = reqwest::Client::new();
    let r = client
        .put(format!("{}/o/r/api/settings", server.base_url))
        .header("Content-Type", "application/toml")
        .body(format!(
            "[upstream]\ngit = \"{}\"\n",
            server.repo_url("o", "up")
        ))
        .send()
        .await?;
    assert!(r.status().is_success(), "{}", r.text().await?);
    let unit = step!(
        "plan 2",
        walgit_server::maintain::next_unit(&server.state, &id)
    )?;
    assert_eq!(unit, walgit_server::maintain::Unit::Repair(1), "{unit:?}");
    let head_before = h.manifest().head_seq;
    let report = step!("pass 2", walgit_server::maintain::run_pass(&server.state))?;
    assert_eq!(report.units, 1, "{report:?}");
    step!("sync3", h.sync())?;
    assert_eq!(
        h.manifest().head_seq,
        head_before + 1,
        "one COMPACT entry with the repaired objects"
    );
    let f = walgit_server::ops::read_fsck(&h).await.unwrap().unwrap();
    assert_eq!(f.repaired_seq, head_before + 1);
    let ok = std::process::Command::new("git")
        .current_dir(h.local().path())
        .args(["cat-file", "-e", &blob2])
        .status()?
        .success();
    assert!(ok, "the blob is back in the serving copy");

    // Pass 3: re-verify after the repair → clean, nothing due afterwards.
    let unit = step!(
        "plan 3",
        walgit_server::maintain::next_unit(&server.state, &id)
    )?;
    assert!(
        matches!(unit, walgit_server::maintain::Unit::Fsck(ref w) if w.contains("re-verify")),
        "{unit:?}"
    );
    let report = step!("pass 3", walgit_server::maintain::run_pass(&server.state))?;
    assert_eq!(report.units, 1, "{report:?}");
    let f = walgit_server::ops::read_fsck(&h).await.unwrap().unwrap();
    assert!(f.missing.is_empty() && f.problems == 0, "{f:?}");
    let unit = step!(
        "plan 4",
        walgit_server::maintain::next_unit(&server.state, &id)
    )?;
    assert_eq!(unit, walgit_server::maintain::Unit::Idle);
    Ok(())
}

/// A push whose pack references an object the server lacks (the client
/// believes the server has it) is refused with the reason ON EVERY REF —
/// `unpack ng` alone made git print "remote failed to report status" and the
/// server logged nothing (prod 2026-08-21 03:28Z, the 1,952-blob hole).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connectivity_failure_is_reported_per_ref_not_as_remote_failure() -> anyhow::Result<()> {
    let server = step!("start", Server::start())?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    std::fs::write(src.path().join("b.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let blob2 = git_in(src.path(), &["rev-parse", "HEAD:b.txt"])?
        .trim()
        .to_string();
    // Make the client believe the server already has commit 2: a second remote ref
    // in the advertisement. We fake it by pushing only the commit + tree via a
    // ref the server accepts without connectivity (a tag on a pack that lacks the
    // blob is refused too) — so instead feed receive-pack a thin pack directly.
    // Simplest faithful reproduction: push main with `--no-thin` disabled and the
    // blob object deleted from the client's own odb *after* git decided it is
    // unchanged... Too brittle. Use the server API: publish the commit+tree pack
    // (no blob) and advertise `refs/heads/x` at c2; then `git push main` sends
    // zero objects (c2 is "already there") and the server's connectivity check
    // trips on the blob.
    let holes = tempfile::tempdir()?;
    let tree2 = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", holes.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c2}\n{tree2}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    step!(
        "add hole pack",
        h.add_pack(
            &holes.path().join(format!("pack-{sha}.pack")),
            &holes.path().join(format!("pack-{sha}.idx")),
            0,
            None
        )
    )?;
    step!("sync2", h.sync())?;
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: "refs/heads/x".into(),
            old_oid: String::new(),
            new_oid: c2.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "advertise x",
        h.publish_push_synced(None, txn, std::collections::HashMap::default())
    )?;
    // A new commit on top whose tree still references the missing blob (b.txt
    // unchanged): git sends commit 3 + its root tree, the server walks into b.txt.
    std::fs::write(src.path().join("a.txt"), "three\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "three"])?;

    let out = std::process::Command::new("git")
        .args(["push", "--porcelain", &server.repo_url("o", "r"), "main"])
        .current_dir(src.path())
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "must be refused: {text}");
    assert!(
        !text.contains("remote failure") && !text.contains("failed to report status"),
        "git must see a proper report: {text}"
    );
    assert!(
        text.contains("refs/heads/main") && text.contains("connectivity") && text.contains(&blob2),
        "per-ref reason names the oid: {text}"
    );
    Ok(())
}

/// Placement (D29/D30, the operator: "the SSD host looks after acme/monorepo and a serverless host
/// doesn't"): a host whose `[placement] serve_exclude` names a repository answers
/// its object work — fetch, push, LFS — with 503 + Retry-After BEFORE any sync
/// (no task, no materialize), while refs-level reads (info/refs, ls-remote, the
/// API) keep working so the edge's read-only fallback is useful. Other repos are
/// served normally by the same host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_excluded_from_serving_a_repo_refuses_object_work_with_503() -> anyhow::Result<()> {
    // The writer host holds both repos.
    let writer = step!("start writer", Server::start())?;
    step!("put big", writer.put_repo("acme", "big"))?;
    step!("put small", writer.put_repo("o", "small"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "hi\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "c"])?;
    git(
        &["push", "-q", &writer.repo_url("acme", "big"), "main"],
        src.path(),
    )?;
    git(
        &["push", "-q", &writer.repo_url("o", "small"), "main"],
        src.path(),
    )?;

    // A front that does not serve acme/*.
    let front = step!(
        "start front",
        writer.start_sibling_with(|c| {
            c.placement.serve_exclude = vec!["acme/*".into()];
        })
    )?;
    let client = reqwest::Client::new();

    // Refs-level still answers on the front.
    let refs = step!(
        "info/refs",
        front.get_text("/acme/big.git/info/refs?service=git-upload-pack", &[])
    )?;
    assert!(refs.contains("refs/heads/main"), "{refs}");
    let ls = front.ls_remote("acme", "big")?;
    assert!(ls.contains("refs/heads/main"));

    // Fetch (v2) → 503 + Retry-After + ERR naming the host; no task started.
    let body =
        b"0011command=fetch0001000ewant 0000000000000000000000000000000000000000\n0009done\n0000"
            .to_vec();
    let r = client
        .post(format!("{}/acme/big.git/git-upload-pack", front.base_url))
        .header("Git-Protocol", "version=2")
        .header("Content-Type", "application/x-git-upload-pack-request")
        .body(body)
        .send()
        .await?;
    assert_eq!(r.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.headers().get("retry-after").map(|v| v.to_str().unwrap()),
        Some("15")
    );
    let text = r.text().await?;
    assert!(text.contains("ERR walgit: acme/big is served by"), "{text}");
    let tasks = step!("tasks", front.get_text("/acme/big/api/tasks", &[]))?;
    assert!(
        !tasks.contains("materialize") && !tasks.contains("remote-index"),
        "no sync started: {tasks}"
    );

    // Push → 503 (git shows the RPC failure) and nothing published.
    std::fs::write(src.path().join("g.txt"), "more\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "d"])?;
    let out = std::process::Command::new("git")
        .args(["push", &front.repo_url("acme", "big"), "main"])
        .current_dir(src.path())
        .output()?;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("503"), "{err}");
    let h = step!(
        "open",
        writer
            .state
            .registry
            .open(&walgit_git::RepoId::new("acme", "big")?)
    )?;
    step!("sync", h.sync_refs())?;
    assert_eq!(
        h.manifest().head_seq,
        1,
        "nothing published through the front"
    );

    // LFS batch → 503 too.
    let r = client
        .post(format!("{}/acme/big.git/info/lfs/objects/batch", front.base_url))
        .json(&serde_json::json!({"operation": "download", "objects": [{"oid": "0".repeat(64), "size": 1}]}))
        .send()
        .await?;
    assert_eq!(r.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

    // The same front serves o/small: push + clone work.
    git(
        &["push", "-q", &front.repo_url("o", "small"), "main"],
        src.path(),
    )?;
    let clone = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            &front.repo_url("o", "small"),
            clone.path().to_str().unwrap(),
        ],
        clone.path().parent().unwrap(),
    )?;
    assert!(clone.path().join("g.txt").exists());
    Ok(())
}

/// The static bundle list must show a bundle this host just built — the list is
/// cached per repo (TTL) and the `bundle` op invalidates it. Prod 2026-08-21:
/// the SSD host advertised 4 hourlies for 20+ min after it had published the 5th
/// (the cache was keyed by manifest version, which a publish does not change).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bundle_list_shows_a_bundle_right_after_this_host_builds_it() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![walgit_config::Role::Serve, walgit_config::Role::Maintain];
            c.bundles.enabled = true;
            c.bundles.min_commits = 1;
            c.compaction.enabled = false;
            c.maintenance.checkpoints = false;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;

    // Weekly via the op (what the maintainer runs), then read + cache the list.
    let id = walgit_git::RepoId::new("o", "r")?;
    let mut params = std::collections::HashMap::new();
    params.insert("strategy".to_string(), "weekly".to_string());
    let t = walgit_server::ops::start(server.state.clone(), id.clone(), "bundle", params)
        .await
        .map_err(|_| anyhow::anyhow!("op start failed"))?;
    assert!(t.wait_done(std::time::Duration::from_secs(30)).await);
    let list1 = step!("list 1", server.get_text("/o/r.git/bundles/list", &[]))?;
    assert!(list1.contains("[bundle \"weekly-"), "{list1}");
    assert!(!list1.contains("daily-"));
    let _again = step!(
        "list 1 again (cached)",
        server.get_text("/o/r.git/bundles/list", &[])
    )?;

    // New objects, a daily built by the op → the NEXT list shows it, no TTL wait.
    std::fs::write(src.path().join("g.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let mut params = std::collections::HashMap::new();
    params.insert("strategy".to_string(), "daily".to_string());
    let t = walgit_server::ops::start(server.state.clone(), id.clone(), "bundle", params)
        .await
        .map_err(|_| anyhow::anyhow!("op start failed"))?;
    assert!(t.wait_done(std::time::Duration::from_secs(30)).await);
    assert!(t.outcome().is_some_and(|o| o.is_ok()), "{:?}", t.outcome());
    let list2 = step!("list 2", server.get_text("/o/r.git/bundles/list", &[]))?;
    assert!(
        list2.contains("[bundle \"daily-"),
        "the list served right after the build must contain it:\n{list2}"
    );

    // Another host on the same bucket, which cached the list BEFORE the next build
    // and is never told about it: its next GET must still be fresh (the cache is
    // keyed by list.pb's own version, probed per request — not by a TTL).
    let other = step!(
        "start other",
        server.start_sibling_with(|c| {
            c.bundles.enabled = true;
        })
    )?;
    let seen = step!("other list", other.get_text("/o/r.git/bundles/list", &[]))?;
    assert!(seen.contains("daily-") && !seen.contains("hourly-"));
    std::fs::write(src.path().join("h.txt"), "three\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "three"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let mut params = std::collections::HashMap::new();
    params.insert("strategy".to_string(), "hourly".to_string());
    let t = walgit_server::ops::start(server.state.clone(), id.clone(), "bundle", params)
        .await
        .map_err(|_| anyhow::anyhow!("op start failed"))?;
    assert!(t.wait_done(std::time::Duration::from_secs(30)).await);
    let list3 = step!(
        "other list after a build elsewhere",
        other.get_text("/o/r.git/bundles/list", &[])
    )?;
    assert!(
        list3.contains("[bundle \"hourly-"),
        "a host that did not build must still serve the new list at once:\n{list3}"
    );
    Ok(())
}

/// A hundred closed hourly slots with nothing to cut must not cost a hundred passes
/// (closed = the slot's as-of instant has passed, whatever the strategy's period —
/// a daily is final an hour after 23:00, not at the next 23:00):
/// `next_unit` settles them at plan time (refs-level; verdicts recorded in the
/// list), and the live slot with real objects is built in the SAME pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_pass_settles_all_closed_empty_slots() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![walgit_config::Role::Serve, walgit_config::Role::Maintain];
            c.bundles.enabled = true;
            c.bundles.main_only = true;
            c.bundles.min_commits = 1;
            c.compaction.enabled = false;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::ZERO;
            // weekly (full) + hourly on weekly: the closed hours since the weekly are empty.
            c.bundles.strategy.retain(|s| s.name != "daily");
            for s in &mut c.bundles.strategy {
                if s.name == "hourly" {
                    s.base = Some("weekly".into());
                    s.backfill_max = 0;
                }
            }
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    // Publish the first state 31 hours in the past so that 30 closed hourly slots exist.
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    let now = std::time::SystemTime::now();
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    // Push normally first (objects), then time-shift the ref history with an explicit created_at.
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync", h.sync())?;
    // A weekly cut at the Sunday before yesterday (earliest state), via the op.
    let weekly = server
        .state
        .cfg
        .bundles
        .strategy
        .iter()
        .find(|s| s.name == "weekly")
        .unwrap()
        .clone();
    // The most recent weekly slot (not `now - 36h`): if an OLDER weekly were
    // cut, the maintainer pass would first build the NEWER due weekly and
    // re-base the hourlies, leaving the hourly skips stale (they were recorded
    // against the old base) and the plan re-reporting them Missing. With the
    // last weekly as the base, no newer weekly is due during the pass and the
    // hourly verdicts stay stable (issue #17).
    let sunday = walgit_bundle::slots::last_slot_at_or_before(&weekly, now)?.unwrap();
    let mut params = std::collections::HashMap::new();
    params.insert("strategy".to_string(), "weekly".to_string());
    params.insert("slot".to_string(), sunday.to_string());
    let t = walgit_server::ops::start(server.state.clone(), id.clone(), "bundle", params)
        .await
        .map_err(|_| anyhow::anyhow!("op start"))?;
    assert!(t.wait_done(std::time::Duration::from_secs(30)).await);
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("list");
    assert_eq!(list.bundles.len(), 1, "{list:?}");
    drop(c1);

    // New objects NOW (the live hour has real work).
    std::fs::write(src.path().join("g.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;

    // Plan before: only the two newest hourly slots are wanted (D21, 2026-08-22) — the older ones
    // are not work even though they are missing; the closed one of the two is empty (the weekly
    // holds everything up to now-ish).
    let ctx = walgit_bundle::slots::PlanContext {
        first_state: h.first_state_time(),
        can_full: true,
        can_incremental: true,
        wrong_host_reason: None,
    };
    let hourly = server
        .state
        .cfg
        .bundles
        .strategy
        .iter()
        .find(|s| s.name == "hourly")
        .unwrap()
        .clone();
    let rows = server.state.bundles.plan(&id, now, ctx).await?;
    let planned: Vec<_> = rows.iter().filter(|r| r.strategy == "hourly").collect();
    // The hourly window is anchored at the weekly cut above: planned rows are
    // the integer-hour slots in (weekly anchor, now], truncated to the newest
    // INCREMENTALS_KEPT (D21). Derive the expectation from the anchor instead
    // of assuming a constant — right after the weekly's Sunday 23:00Z fire
    // that is 0 slots, then 1 until 01:00 Monday UTC, and a constant
    // expectation is deterministically red in that window (PR #28 review;
    // probe-verified equal to the plan at every point of the window).
    let expected = walgit_bundle::slots::slots_between(
        &hourly,
        walgit_bundle::slots::from_epoch(sunday),
        now,
    )?
    .len()
    .min(walgit_bundle::slots::INCREMENTALS_KEPT);
    assert_eq!(
        planned.len(),
        expected,
        "only the newest slots are planned: {rows:?}"
    );
    // All CLOSED planned hourlies are Missing. The current hour is Pending
    // (not Missing) while it is still within SLOT_CLOSE_GRACE of its fire —
    // the plan's `now` is real, so the test used to flake in the two minutes
    // after every hour boundary (issue #4 follow-up: one CI run at 07:01Z).
    for r in &planned {
        if walgit_bundle::slots::slot_closed(&hourly, r.slot, now) {
            assert_eq!(
                r.status,
                walgit_bundle::slots::SlotStatus::Missing,
                "closed planned slot not missing: {r:?}"
            );
        }
    }
    let missing_before = planned
        .iter()
        .filter(|r| {
            r.status == walgit_bundle::slots::SlotStatus::Missing
                && walgit_bundle::slots::slot_closed(&hourly, r.slot, now)
        })
        .count();
    // A closed missing row is guaranteed only when both newest slots are
    // planned: the older one fired ≥ 1 h ago and is always closed. In the tie
    // window the only planned slot may still be within the close grace (or
    // there is no planned slot at all).
    if expected == walgit_bundle::slots::INCREMENTALS_KEPT {
        assert!(
            missing_before >= 1,
            "no closed missing hourly planned: {rows:?}"
        );
    }

    // ONE pass.
    let report = step!("pass", walgit_server::maintain::run_pass(&server.state))?;
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("list");
    let hourlies: Vec<_> = list
        .bundles
        .iter()
        .filter(|b| b.strategy == "hourly")
        .collect();
    // Nothing to settle in the tie window's first hour (no closed missing
    // slot was planned) — the pass legitimately records no skip then.
    if missing_before >= 1 {
        assert!(
            !list.skipped.is_empty(),
            "closed empty slots recorded in the list: {report:?}"
        );
    }
    // Every CLOSED missing slot is settled in that one pass. The open (current)
    // slot may stay missing: the commit above was pushed after its fire time, so
    // as of the slot there is nothing new — it belongs to the next hour (D22).
    let rows = server.state.bundles.plan(&id, now, ctx).await?;
    let still_missing_closed: Vec<u64> = rows
        .iter()
        .filter(|r| {
            r.strategy == "hourly"
                && r.status == walgit_bundle::slots::SlotStatus::Missing
                && walgit_bundle::slots::slot_closed(&hourly, r.slot, now)
        })
        .map(|r| r.slot)
        .collect();
    assert!(
        still_missing_closed.is_empty(),
        "after one pass no closed slot stays missing: {still_missing_closed:?}\nskipped={} built={}",
        list.skipped.len(),
        hourlies.len()
    );
    assert!(
        list.skipped.len() >= missing_before.saturating_sub(1),
        "settled at plan time, not one per pass: skipped={} missing_before={missing_before}",
        list.skipped.len()
    );
    Ok(())
}

/// Sunday's weekly on an ssd maintainer (the SSD host): the missing full slot of a
/// repository that has a tier-2 base and pushes since it first yields
/// `BaseRebuild` (compact --base: new base + history pack + checkpoint), then the
/// full slot itself, which COMPOSES header ∘ base (no pack-objects of the
/// history). Pushes landing after the rebuild do not re-trigger it this week.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn weekly_slot_rebuilds_the_base_then_composes_it_on_an_ssd_maintainer() -> anyhow::Result<()>
{
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![
                walgit_config::Role::Serve,
                walgit_config::Role::Maintain,
                walgit_config::Role::Compact,
                walgit_config::Role::Bundle,
            ];
            c.maintenance.disk = walgit_config::MaintainerDisk::Ssd;
            c.cache.mode = walgit_config::CacheMode::Disk;
            c.bundles.enabled = true;
            c.bundles.strategy.truncate(1); // weekly only
            c.compaction.enabled = true;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    // A large repository's shape: the repository's FIRST entry is the base (import --direct
    // publishes a tier-2 pack + the ref snapshot), then pushes land on top.
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let packs = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args([
            "pack-objects",
            "--revs",
            &format!("{}/pack", packs.path().display()),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c1}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    step!("sync0", h.sync())?;
    step!(
        "import base",
        h.add_pack(
            &packs.path().join(format!("pack-{sha}.pack")),
            &packs.path().join(format!("pack-{sha}.idx")),
            2,
            None
        )
    )?;
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: String::new(),
            new_oid: c1.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "import refs",
        h.publish_push_synced(None, txn, std::collections::HashMap::default())
    )?;
    step!("sync after base", h.sync())?;
    std::fs::write(src.path().join("g.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync after push", h.sync())?;

    // The missing weekly slot → rebuild the base first.
    let unit = step!("plan 1", next_unit(&server.state, &id))?;
    assert!(
        matches!(unit, Unit::BaseRebuild(ref s, _) if s == "weekly"),
        "{unit:?}"
    );
    let report = step!("pass 1 (rebuild)", run_pass(&server.state))?;
    assert_eq!(
        report.compactions,
        1,
        "rebuild unit: {report:?}\ntasks: {}",
        server.get_text("/o/r/api/tasks", &[]).await?
    );
    step!("sync after rebuild", h.sync())?;
    let base2 = h
        .manifest()
        .packs
        .iter()
        .filter(|p| p.tier == 2 && p.kind != walgit_proto::v1::PackKind::History as i32)
        .map(|p| p.checksum.clone())
        .next()
        .expect("new base");
    assert_ne!(sha, base2, "a new base was published");
    assert!(
        h.manifest().packs.iter().all(|p| p.tier == 2),
        "the rebuild superseded every smaller pack: {:?}",
        h.manifest()
            .packs
            .iter()
            .map(|p| p.tier)
            .collect::<Vec<_>>()
    );

    // #175: every superseded pack gets a `wal/<checksum>.superseded` marker
    // recording *when* it left the live set. The manifest keeps only the live
    // set and this COMPACT entry is eventually folded into a checkpoint, so the
    // marker is the only thing bucket GC can age a pack by.
    {
        use futures::StreamExt;
        use walgit_store::ObjectStore;
        let manifest = h.manifest();
        let live: std::collections::HashSet<&str> =
            manifest.packs.iter().map(|p| p.checksum.as_str()).collect();
        let mut markers = 0usize;
        let mut stream = h.store().list(walgit_proto::keys::SUPERSEDED_DIR, None);
        while let Some(m) = stream.next().await {
            let m = m?;
            let Some(checksum) = m.key.strip_prefix(walgit_proto::keys::SUPERSEDED_DIR) else {
                continue;
            };
            markers += 1;
            assert!(
                !live.contains(checksum),
                "a live pack must not carry a superseded marker: {checksum}"
            );
        }
        assert!(
            markers > 0,
            "the rebuild must have marked the packs it superseded"
        );
    }

    // A push lands between the rebuild and the compose (the rig's churn, 2026-08-22: the compose
    // refused for as long as refs kept moving — "no ref snapshot at the base's seq"). The header
    // must carry the refs AT THE BASE'S SEQ (replayed from the WAL), not the new tip.
    let base_tip = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    std::fs::write(src.path().join("between.txt"), "between\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "between"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync after push between", h.sync())?;

    // Next: the weekly slot itself, composed from the new base.
    let unit = step!("plan 2", next_unit(&server.state, &id))?;
    assert!(
        matches!(unit, Unit::BundleSlot(ref s, _) if s == "weekly"),
        "{unit:?}"
    );
    let report = step!("pass 2 (compose)", run_pass(&server.state))?;
    assert_eq!(
        report.bundles,
        1,
        "compose unit: {report:?}\ntasks: {}",
        server.get_text("/o/r/api/tasks", &[]).await?
    );
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("list");
    let weekly = list
        .bundles
        .iter()
        .find(|b| b.strategy == "weekly")
        .expect("weekly entry");
    let m2 = h.manifest();
    let base_pack = m2.packs.iter().find(|p| p.checksum == base2).unwrap();
    assert!(
        weekly.size > base_pack.pack_size && weekly.size < base_pack.pack_size + 4096,
        "composed = header ∘ base pack: {} vs pack {}",
        weekly.size,
        base_pack.pack_size
    );
    assert_eq!(weekly.seq, base_pack.seq);
    let main_tip = weekly
        .tips
        .iter()
        .find(|t| t.name == "refs/heads/main")
        .expect("main tip");
    assert_eq!(
        main_tip.oid, base_tip,
        "the header carries main as of the base's seq, not the push that landed since"
    );

    // A push after the rebuild: no second rebuild this week; the plan is idle (slot built).
    std::fs::write(src.path().join("h.txt"), "three\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "three"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync after push 2", h.sync())?;
    let unit = step!("plan 3", next_unit(&server.state, &id))?;
    assert!(!matches!(unit, Unit::BaseRebuild(..)), "{unit:?}");

    // An imported multi-pack set (large-repository measurement: 11 tier-2 packs, the 32 GB
    // base among 5 MB ones) is itself a reason to rebuild next week: the
    // compose needs exactly one base, and "the base" is the biggest one.
    let extra = tempfile::tempdir()?;
    let blob = git_in(src.path(), &["rev-parse", "HEAD:h.txt"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", extra.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{blob}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let small = String::from_utf8_lossy(&out.stdout).trim().to_string();
    step!(
        "second tier-2 pack",
        h.add_pack(
            &extra.path().join(format!("pack-{small}.pack")),
            &extra.path().join(format!("pack-{small}.idx")),
            2,
            None
        )
    )?;
    step!("sync 3", h.sync())?;
    let m3 = h.manifest();
    assert_eq!(walgit_wal::base_packs(&m3).len(), 2);
    assert_eq!(
        walgit_wal::base_pack(&m3).unwrap().checksum,
        base2,
        "the base is the biggest tier-2 pack, not the newest"
    );
    let next_weekly =
        walgit_bundle::slots::from_epoch(weekly.slot) + std::time::Duration::from_hours(168);
    let up = walgit_server::maintain::upcoming(
        &h,
        &h.effective_config(),
        &walgit_server::maintain::heartbeats(&server.state).await?,
        next_weekly - std::time::Duration::from_secs(60),
    )
    .await;
    let w = up
        .iter()
        .find(|u| u.strategy == "weekly")
        .expect("weekly row");
    assert!(w.unit.starts_with("base rebuild"), "{w:?}");
    Ok(())
}

/// A pack published without its `.rev` (git < 2.41 wrote none; a large repository's whole
/// serving copy had none, 2.85 s per fetch — the original large-repository measurements) gets one
/// from the maintainer: built where the pack is local, uploaded as the
/// side-file, advertised in the manifest (`has_rev`) so every other host
/// downloads it on its next sync instead of rebuilding it per `pack-objects`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintainer_builds_and_publishes_missing_rev_indexes() -> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit};
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            // The rev-index assertions below are about a specific unit: keep the
            // (lower-priority) GC unit out of this rig's plan.
            c.maintenance.gc_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    // Push packs (gix ingest) carry no .rev and need none: below
    // REV_INDEX_MIN_OBJECTS the maintainer leaves them alone.
    assert!(
        h.manifest().packs.iter().all(|p| !p.has_rev),
        "{:?}",
        h.manifest().packs
    );
    assert_eq!(
        step!("idle (small packs)", next_unit(&server.state, &id))?,
        Unit::Idle
    );

    // A legacy-shaped pack: pack-objects to a file with reverse indexes off (no .rev).
    let legacy = tempfile::tempdir()?;
    let tree = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args([
            "-c",
            "pack.writeReverseIndex=false",
            "pack-objects",
            &format!("{}/pack", legacy.path().display()),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{tree}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!legacy.path().join(format!("pack-{sha}.rev")).exists());
    step!(
        "add legacy pack",
        h.add_pack(
            &legacy.path().join(format!("pack-{sha}.pack")),
            &legacy.path().join(format!("pack-{sha}.idx")),
            0,
            None
        )
    )?;
    step!("sync2", h.sync())?;
    assert!(
        h.manifest()
            .packs
            .iter()
            .any(|p| p.checksum == sha && !p.has_rev),
        "{:?}",
        h.manifest().packs
    );

    // Another host installs the pack as it is (no .rev) before the unit runs.
    let other = step!(
        "start other",
        server.start_sibling_with(|c| {
            c.server.roles = vec![walgit_config::Role::Serve];
        })
    )?;
    let h2 = step!("open other", other.state.registry.open(&id))?;
    step!("sync other", h2.sync())?;
    let rev2 = h2
        .local()
        .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
        .with_extension("rev");
    assert!(!rev2.exists());

    // The unit (what the planner would emit for a ≥ REV_INDEX_MIN_OBJECTS pack):
    // build locally, upload the side-file, CAS the manifest.
    let mut params = std::collections::HashMap::new();
    params.insert("pack".to_string(), sha.clone());
    let task = walgit_server::ops::start(server.state.clone(), id.clone(), "rev-index", params)
        .await
        .map_err(|_| anyhow::anyhow!("rev-index op did not start"))?;
    assert!(task.wait_done(std::time::Duration::from_secs(60)).await);
    assert!(
        matches!(task.outcome(), Some(Ok(_))),
        "{:?}",
        task.outcome()
    );
    assert!(
        h.local()
            .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
            .with_extension("rev")
            .exists()
    );
    step!("sync3", h.sync())?;
    let p = h
        .manifest()
        .packs
        .iter()
        .find(|p| p.checksum == sha)
        .cloned()
        .unwrap();
    assert!(p.has_rev, "advertised in the manifest: {p:?}");
    assert!(
        walgit_store::ObjectStore::head(h.store(), &walgit_proto::keys::rev_key(&sha))
            .await?
            .is_some(),
        "uploaded as the side-file"
    );
    assert_eq!(step!("idle", next_unit(&server.state, &id))?, Unit::Idle);

    // The other host, pack already installed, picks the side-file up on its
    // next sync (the manifest revision moved) — the fleet converges.
    step!("sync other 2", h2.sync())?;
    assert!(
        rev2.exists(),
        "installed pack gets the newly advertised side-file on sync"
    );
    assert_eq!(
        std::fs::read(&rev2)?,
        std::fs::read(
            h.local()
                .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
                .with_extension("rev")
        )?
    );
    Ok(())
}

/// An incremental slot whose tip set equals the newest built incremental of
/// the strategy on the same base is `skipped (unchanged since <id>)` — recorded
/// like too-small, never cut. Without it an idle night cuts 23–48 identical
/// 315 MB hourlies on a large repository (2026-08-21 08:00/09:00/10:00, same tip, no push
/// since 06:43Z): `min_commits` counts since the BASE, not since the previous
/// incremental. Clients are unaffected (git stops at the first bundle whose
/// prerequisites it has).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_incremental_slots_are_skipped_as_unchanged() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![walgit_config::Role::Serve, walgit_config::Role::Maintain];
            c.bundles.enabled = true;
            c.bundles.main_only = true;
            c.bundles.min_commits = 1;
            c.compaction.enabled = false;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::ZERO;
            c.bundles.strategy.retain(|s| s.name != "daily");
            for s in &mut c.bundles.strategy {
                if s.name == "hourly" {
                    s.base = Some("weekly".into());
                    s.backfill_max = 0;
                }
            }
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    std::fs::write(src.path().join("g.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    let hour = std::time::Duration::from_secs(3600);
    // Fixed synthetic clock: choose a weekly slot, put c1 just before it and
    // c2 one hour after it, then run the pass six hours later. The pass builds
    // the weekly at that slot (already below) but no newer weekly can exist;
    // the first hourly after it sees c2 and later identical slots must settle.
    let weekly = server
        .state
        .cfg
        .bundles
        .strategy
        .iter()
        .find(|s| s.name == "weekly")
        .unwrap()
        .clone();
    let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
    let weekly_slot = walgit_bundle::slots::last_slot_at_or_before(&weekly, base)?
        .expect("weekly slot before fixed base");
    let weekly_at = walgit_bundle::slots::from_epoch(weekly_slot);
    let now = weekly_at + 6 * hour + walgit_bundle::slots::SLOT_CLOSE_GRACE;
    // History with explicit times: c1 before the weekly, c2 after it, nothing
    // since. Revs arrive as one shell-flavored string ("<tip> ^<base>"); the
    // pipe takes argv, so tokenize here like `sh` used to.
    let pack_of = |revs: &str| -> anyhow::Result<Vec<u8>> {
        let mut first: Vec<&str> = vec!["rev-list", "--objects"];
        first.extend(revs.split_whitespace());
        Ok(git_pipe(src.path(), &first, &["pack-objects", "--stdout"]).stdout)
    };
    let txn = |name: &str, old: &str, new: &str| walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: name.into(),
            old_oid: old.into(),
            new_oid: new.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ingest = |bytes: Vec<u8>| async {
        h.local()
            .ingest_pack(
                std::io::Cursor::new(bytes),
                walgit_git::IngestOptions {
                    fsck: false,
                    max_bytes: None,
                    thin: false,
                },
            )
            .await
            .unwrap()
            .unwrap()
    };
    let p1 = ingest(pack_of(&c1)?).await;
    step!(
        "c1 ten days ago",
        h.publish_push_at(
            Some(p1),
            txn("refs/heads/main", "", &c1),
            std::collections::HashMap::default(),
            weekly_at - hour
        )
    )?;
    let p2 = ingest(pack_of(&format!("{c2} ^{c1}"))?).await;
    step!(
        "c2 six hours ago",
        h.publish_push_at(
            Some(p2),
            txn("refs/heads/main", &c1, &c2),
            std::collections::HashMap::default(),
            weekly_at + hour
        )
    )?;
    step!("sync", h.sync())?;

    // Build the fixed weekly (state as of then: c1). run_pass_at will not
    // build any newer weekly because `now` is only six hours after it.
    let sunday = weekly_slot;
    let mut params = std::collections::HashMap::new();
    params.insert("strategy".to_string(), "weekly".to_string());
    params.insert("slot".to_string(), sunday.to_string());
    let t = walgit_server::ops::start(server.state.clone(), id.clone(), "bundle", params)
        .await
        .map_err(|_| anyhow::anyhow!("op start"))?;
    assert!(t.wait_done(std::time::Duration::from_secs(30)).await);

    // Passes until idle: exactly ONE hourly (the slot that first sees c2); every
    // later closed slot is recorded `unchanged since <that hourly>`. The
    // injected `now` keeps the real maintain path from selecting a newer
    // weekly slot based on the CI wall clock.
    for _ in 0..8 {
        let report = step!(
            "pass",
            walgit_server::maintain::run_pass_at(&server.state, now)
        )?;
        if report.units == 0 {
            break;
        }
    }
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("list");
    let hourlies: Vec<_> = list
        .bundles
        .iter()
        .filter(|b| b.strategy == "hourly")
        .collect();
    let hourly = server
        .state
        .cfg
        .bundles
        .strategy
        .iter()
        .find(|s| s.name == "hourly")
        .unwrap()
        .clone();
    let ctx = walgit_bundle::slots::PlanContext {
        first_state: h.first_state_time(),
        can_full: true,
        can_incremental: true,
        wrong_host_reason: None,
    };
    let rows = server.state.bundles.plan(&id, now, ctx).await?;
    let dbg: Vec<_> = rows
        .iter()
        .filter(|r| r.strategy == "hourly")
        .map(|r| (r.slot, format!("{:?}", r.status)))
        .collect();
    assert_eq!(
        hourlies.len(),
        1,
        "one hourly carries c2; the identical later slots are not cut: {:?}\nbundles={:?}\nskipped={:?}\nplan={dbg:#?}",
        hourlies.iter().map(|b| (&b.id, b.slot)).collect::<Vec<_>>(),
        list.bundles
            .iter()
            .map(|b| (&b.id, b.slot))
            .collect::<Vec<_>>(),
        list.skipped
            .iter()
            .map(|s| (s.slot, &s.reason))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        hourlies[0]
            .tips
            .iter()
            .map(|t| t.oid.as_str())
            .collect::<Vec<_>>(),
        vec![c2.as_str()]
    );
    let unchanged: Vec<_> = list
        .skipped
        .iter()
        .filter(|s| s.reason.starts_with("unchanged since "))
        .collect();
    // With only the two newest slots ever planned (D21, 2026-08-22) that is the one slot after it —
    // settled as unchanged once it is *closed* (120 s past its fire time); in the first two minutes of
    // an hour it is still open and legitimately not recorded yet.
    let other_slot = rows
        .iter()
        .filter(|r| r.strategy == "hourly" && r.slot != hourlies[0].slot)
        .map(|r| r.slot)
        .max();
    let other_closed =
        other_slot.is_some_and(|s| walgit_bundle::slots::slot_closed(&hourly, s, now));
    assert!(
        other_closed,
        "the hour after the carrier must be closed at the synthetic now: {other_slot:?}"
    );
    assert!(
        !unchanged.is_empty(),
        "the closed hour after it must be skipped as unchanged: {:?}",
        list.skipped
            .iter()
            .map(|s| (s.slot, &s.reason))
            .collect::<Vec<_>>()
    );
    assert!(
        unchanged.iter().all(
            |s| s.reason == format!("unchanged since {}", hourlies[0].id)
                && s.slot > hourlies[0].slot
        ),
        "{unchanged:?}"
    );
    // And the plan shows them so — nothing is re-measured.
    let closed_missing = rows
        .iter()
        .filter(|r| {
            r.strategy == "hourly"
                && r.status == walgit_bundle::slots::SlotStatus::Missing
                && walgit_bundle::slots::slot_closed(&hourly, r.slot, now)
        })
        .count();
    assert_eq!(closed_missing, 0, "{rows:?}");
    Ok(())
}

/// The blobless bundle family (`filter = "blob:none"` strategies): the weekly
/// "history" bundle is the D18 history pack composed under a `@filter=blob:none`
/// header, incrementals pack with `--filter=blob:none`; they are advertised ONLY
/// at `bundles/list?filter=blob:none` (git does not match `bundle.<id>.filter`
/// against the clone's filter — a full clone would swallow them). A
/// `--filter=blob:none --bundle-uri=<that list>` clone seeds from them and
/// fetches blobs lazily; a full clone with the protocol list never sees them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobless_bundle_family_is_composed_from_the_history_pack_and_served_on_its_own_list()
-> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![
                walgit_config::Role::Serve,
                walgit_config::Role::Maintain,
                walgit_config::Role::Compact,
                walgit_config::Role::Bundle,
            ];
            c.maintenance.disk = walgit_config::MaintainerDisk::Ssd;
            c.cache.mode = walgit_config::CacheMode::Disk;
            c.bundles.enabled = true;
            c.bundles.min_commits = 1;
            c.bundles.strategy.truncate(1); // weekly (full, unfiltered)
            let weekly = c.bundles.strategy[0].clone();
            c.bundles.strategy.push(walgit_config::BundleStrategy {
                name: "weekly-history".into(),
                filter: Some("blob:none".into()),
                ..weekly.clone()
            });
            c.bundles.strategy.push(walgit_config::BundleStrategy {
                name: "hourly-history".into(),
                kind: walgit_config::BundleKind::Incremental,
                base: Some("weekly-history".into()),
                schedule: "0 0 * * * *".into(),
                keep: 0,
                filter: Some("blob:none".into()),
                backfill_max: 0,
                ..weekly
            });
            c.compaction.enabled = true;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("f.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    // A large repository's shape: an imported tier-2 base + ref snapshot, then a push.
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let packs = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args([
            "pack-objects",
            "--revs",
            &format!("{}/pack", packs.path().display()),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c1}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    step!("sync0", h.sync())?;
    step!(
        "import base",
        h.add_pack(
            &packs.path().join(format!("pack-{sha}.pack")),
            &packs.path().join(format!("pack-{sha}.idx")),
            2,
            None
        )
    )?;
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: String::new(),
            new_oid: c1.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "import refs",
        h.publish_push_synced(None, txn, std::collections::HashMap::default())
    )?;
    std::fs::write(src.path().join("f2.txt"), "one and a half\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one.5"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync", h.sync())?;

    // Passes until idle: base rebuild (base + D18 history pack), weekly compose,
    // weekly-history compose (of the history pack).
    for _ in 0..12 {
        let u = next_unit(&server.state, &id).await?;
        if u == Unit::Idle {
            break;
        }
        let _ = step!("pass", run_pass(&server.state))?;
    }
    step!("sync2", h.sync())?;
    let m = h.manifest();
    let base = walgit_wal::base_pack(&m).expect("base").clone();
    let hist = m
        .packs
        .iter()
        .find(|p| {
            p.kind == walgit_proto::v1::PackKind::History as i32 && p.derived_from == base.checksum
        })
        .expect("history pack")
        .clone();
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("list");
    let weekly = list
        .bundles
        .iter()
        .find(|b| b.strategy == "weekly")
        .expect("weekly");
    let weekly_h = list
        .bundles
        .iter()
        .find(|b| b.strategy == "weekly-history")
        .expect("weekly-history");
    assert_eq!(weekly.filter, "");
    assert_eq!(weekly_h.filter, "blob:none");
    assert!(
        weekly.size > base.pack_size && weekly.size < base.pack_size + 4096,
        "weekly = header ∘ base"
    );
    assert!(
        weekly_h.size > hist.pack_size && weekly_h.size < hist.pack_size + 4096,
        "weekly-history = header ∘ history pack: {} vs {}",
        weekly_h.size,
        hist.pack_size
    );
    // Header: v3 with the filter capability.
    let head = server
        .get_text(
            &format!("/o/r.git/{}", weekly_h.key),
            &[("Range", "bytes=0-63")],
        )
        .await?;
    assert!(
        head.starts_with("# v3 git bundle\n@filter=blob:none\n"),
        "{head:?}"
    );

    // A blobless incremental on it ("now" build): new commit with a new blob.
    std::fs::write(
        src.path().join("g.txt"),
        "two — a blob the incremental must NOT carry\n",
    )?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync3", h.sync())?;
    let inc = step!(
        "hourly-history build",
        server.state.bundles.build(&id, "hourly-history")
    )?;
    assert_eq!(
        (inc.kind.as_str(), inc.filter.as_str(), inc.base_id.as_str()),
        ("incremental", "blob:none", weekly_h.id.as_str())
    );
    let head = server
        .get_text(&format!("/o/r.git/{}", inc.key), &[("Range", "bytes=0-63")])
        .await?;
    assert!(
        head.starts_with("# v3 git bundle\n@filter=blob:none\n"),
        "{head:?}"
    );

    // Two lists, one family each.
    let plain = server.get_text("/o/r.git/bundles/list", &[]).await?;
    let blobless = server
        .get_text("/o/r.git/bundles/list?filter=blob:none", &[])
        .await?;
    assert!(
        plain.contains("[bundle \"weekly-")
            && !plain.contains("history")
            && !plain.contains("filter ="),
        "{plain}"
    );
    assert!(
        blobless.contains("[bundle \"weekly-history-")
            && blobless.contains("[bundle \"hourly-history-")
            && !blobless.contains("[bundle \"weekly-1"),
        "{blobless}"
    );
    assert_eq!(
        blobless.matches("    filter = blob:none\n").count(),
        2,
        "{blobless}"
    );
    let v2 = server
        .state
        .bundles
        .protocol_v2_lines(&id, &server.base_url)
        .await?;
    assert!(
        v2.iter().any(|l| l.starts_with("bundle.weekly-1")
            || l.starts_with("bundle.weekly-") && !l.contains("history"))
            && !v2.iter().any(|l| l.contains("history")),
        "{v2:?}"
    );

    // A blobless clone seeded from the blobless list: promisor packs from the
    // bundles, blobs missing until checkout fetches them lazily.
    let tmp = tempfile::tempdir()?;
    let c = tmp.path().join("blobless");
    git(
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!(
                "--bundle-uri={}/o/r.git/bundles/list?filter=blob:none",
                server.base_url
            ),
            &server.repo_url("o", "r"),
            c.to_str().unwrap(),
        ],
        tmp.path(),
    )?;
    let promisors = std::fs::read_dir(c.join(".git/objects/pack"))?
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "promisor")
        })
        .count();
    assert!(
        promisors >= 2,
        "bundle packs unbundled as promisor packs: {promisors}"
    );
    let missing = git_in(&c, &["rev-list", "--objects", "--all", "--missing=print"])?;
    assert!(
        missing.lines().any(|l| l.starts_with('?')),
        "blobs are NOT in the blobless bundles:\n{missing}"
    );
    git_in(&c, &["checkout", "-q", "main"])?; // lazy blob fetch from the server
    assert_eq!(
        std::fs::read_to_string(c.join("g.txt"))?,
        "two — a blob the incremental must NOT carry\n"
    );
    git_in(&c, &["fsck", "--connectivity-only"])?;

    // A full clone with bundle-uri via the protocol never sees the family.
    let f = tmp.path().join("full");
    git(
        &[
            "-c",
            "transfer.bundleURI=true",
            "clone",
            "-q",
            &server.repo_url("o", "r"),
            f.to_str().unwrap(),
        ],
        tmp.path(),
    )?;
    let promisors = std::fs::read_dir(f.join(".git/objects/pack"))?
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "promisor")
        })
        .count();
    assert_eq!(promisors, 0, "no promisor pack in a full clone");
    let missing = git_in(&f, &["rev-list", "--objects", "--all", "--missing=print"])?;
    assert!(!missing.lines().any(|l| l.starts_with('?')), "{missing}");
    git_in(&f, &["fsck"])?;
    Ok(())
}

/// D21 (2026-08-22): the list lists `keep` fulls and the two newest incrementals per
/// strategy — and the maintainer brings an existing list to that shape on its next pass,
/// deleting the pruned objects, even when the repository is idle and publishes nothing
/// (acme/walgit sat at 1 weekly + 3 dailies + 39 hourlies: 43 downloads
/// per fresh clone).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintainer_pass_brings_an_overgrown_bundle_list_to_retention() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![walgit_config::Role::Serve, walgit_config::Role::Maintain];
            c.bundles.enabled = true;
            // The D21 shape this test pins (the default chains the dailies since 2026-08-22).
            for s in &mut c.bundles.strategy {
                s.chain = false;
            }
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let store =
        walgit_store::Prefixed::new(server.state.registry.store().clone(), id.store_prefix());
    // Seed: one weekly, three dailies on it, 13 hourlies on each daily (objects = dummy bytes).
    let mut list = walgit_proto::v1::BundleList {
        mode: "all".into(),
        heuristic: "creationToken".into(),
        ..Default::default()
    };
    let entry =
        |strategy: &str, kind: &str, slot: u64, base_id: &str| walgit_proto::v1::BundleEntry {
            id: format!("{strategy}-{slot}"),
            key: format!("bundles/{strategy}/{slot}.bundle"),
            strategy: strategy.into(),
            kind: kind.into(),
            creation_token: slot,
            slot,
            base_id: base_id.into(),
            ..Default::default()
        };
    let w0 = 1_787_000_400u64; // a Sunday 23:00-ish epoch; only ordering matters here
    list.bundles.push(entry("weekly", "full", w0, ""));
    for d in 1..=3u64 {
        let ds = w0 + d * 86_400;
        list.bundles
            .push(entry("daily", "incremental", ds, &format!("weekly-{w0}")));
        for h in 1..=13u64 {
            list.bundles.push(entry(
                "hourly",
                "incremental",
                ds + h * 3600,
                &format!("daily-{ds}"),
            ));
        }
    }
    assert_eq!(list.bundles.len(), 43);
    for b in &list.bundles {
        step!(
            "seed object",
            store.put_bytes(&b.key, b"bundle".as_slice(), PutMode::Create)
        )?;
    }
    step!(
        "seed list",
        store.put_bytes(
            walgit_proto::keys::BUNDLE_LIST,
            list.encode_to_vec(),
            PutMode::Create
        )
    )?;
    let text = step!("list before", server.get_text("/o/r.git/bundles/list", &[]))?;
    assert_eq!(text.matches("uri = ").count(), 43, "{text}");

    // One maintainer pass (whatever unit it picks) applies retention first.
    let _ = step!("pass", walgit_server::maintain::run_pass(&server.state))?;
    let after = walgit_bundle::ops::read_list(&store).await?.unwrap();
    let ids: Vec<&str> = after.bundles.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(after.bundles.len(), 5, "{ids:?}");
    let d2 = w0 + 2 * 86_400;
    let d3 = w0 + 3 * 86_400;
    for want in [
        format!("weekly-{w0}"),
        format!("daily-{d2}"),
        format!("daily-{d3}"),
        format!("hourly-{}", d3 + 12 * 3600),
        format!("hourly-{}", d3 + 13 * 3600),
    ] {
        assert!(ids.contains(&want.as_str()), "{want} missing from {ids:?}");
    }
    // Pruned objects are gone, kept ones stay.
    assert!(
        step!(
            "pruned gone",
            store.head(&format!("bundles/hourly/{}.bundle", d2 + 3600))
        )?
        .is_none()
    );
    assert!(
        step!(
            "kept stays",
            store.head(&format!("bundles/hourly/{}.bundle", d3 + 13 * 3600))
        )?
        .is_some()
    );
    // Idempotent: a second pass changes nothing.
    let _ = step!("pass 2", walgit_server::maintain::run_pass(&server.state))?;
    let again = walgit_bundle::ops::read_list(&store).await?.unwrap();
    assert_eq!(again.bundles.len(), 5);
    Ok(())
}

/// #175: GC runs under the per-repo `gc` lease. A pass that cannot take it must
/// do nothing and leave the unit due (no `gc.pb`), or a claim it never
/// established would be reported as a completed pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_skips_while_another_instance_holds_the_lease() -> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::ObjectStore;

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // Another instance holds the lease for ten minutes.
    let lease_store: walgit_store::DynStore = std::sync::Arc::new(h.store().clone());
    let lease = walgit_store::coord::try_acquire(
        lease_store,
        &walgit_proto::keys::lease_key("gc"),
        "another-host",
        "gc",
        std::time::Duration::from_secs(600),
    )
    .await?
    .expect("the test takes the lease");

    step!("pass while held", run_pass(&server.state))?;
    assert!(
        h.store().head(walgit_proto::keys::GC).await?.is_none(),
        "a pass that could not take the lease must not write gc.pb"
    );
    let unit = step!("plan still due", next_unit(&server.state, &id))?;
    assert!(
        matches!(unit, Unit::Gc(_)),
        "the unit must stay due, got {unit:?}"
    );

    lease.release().await?;
    step!("pass after release", run_pass(&server.state))?;
    assert!(
        h.store().head(walgit_proto::keys::GC).await?.is_some(),
        "with the lease free the pass records gc.pb"
    );
    Ok(())
}

/// #175: a publisher must refuse a checksum bucket GC has listed as reclaiming.
/// This is the other half of the ordering invariant — without it a publisher
/// could put a checksum GC is deleting back into `packs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publishing_a_reclaiming_checksum_is_refused() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // A pack that is not live yet (so the claim CAS accepts it).
    let dir = tempfile::tempdir()?;
    let tree = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", dir.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{tree}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let checksum = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let pack = dir.path().join(format!("pack-{checksum}.pack"));
    let idx = dir.path().join(format!("pack-{checksum}.idx"));

    step!(
        "list candidate",
        h.update_reclaiming(std::slice::from_ref(&checksum), &[], &[], "t", None)
    )?;
    assert!(
        h.manifest()
            .reclaiming
            .iter()
            .any(|r| r.checksum == checksum),
        "the checksum must be listed before the publish is attempted"
    );

    let err = step!("add pack", h.add_pack(&pack, &idx, 0, None))
        .expect_err("a reclaiming checksum must not be publishable");
    assert!(
        matches!(err, walgit_wal::WalError::Reclaiming(ref c) if *c == checksum),
        "expected Reclaiming, got {err:?}"
    );
    Ok(())
}

/// #175: the receive-pack path (`process_batch`) is the *other* CAS gate. A push
/// that regenerates a byte-identical pack must be refused while bucket GC has
/// the checksum listed — this is the path a client actually triggers, and
/// disabling the check in `publish.rs` lets the push adopt bytes GC is deleting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushing_a_reclaiming_checksum_is_refused() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // A checksum that is not live yet (so the claim CAS accepts it): the push
    // below "regenerates" exactly these bytes, as git deterministically does.
    let dead = "a".repeat(40);
    step!(
        "list candidate",
        h.update_reclaiming(std::slice::from_ref(&dead), &[], &[], "t", None)
    )?;
    assert!(
        h.manifest().reclaiming.iter().any(|r| r.checksum == dead),
        "the checksum must be listed before the push is attempted"
    );

    let dir = tempfile::tempdir()?;
    let ingested = walgit_git::IngestedPack {
        checksum: gix_hash::ObjectId::from_hex(dead.as_bytes())?,
        pack_path: dir.path().join(format!("pack-{dead}.pack")),
        idx_path: dir.path().join(format!("pack-{dead}.idx")),
        pack_size: 1,
        idx_size: 1,
        object_count: 1,
    };
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: String::new(),
            new_oid: "0".repeat(40),
            ..Default::default()
        }],
        ..Default::default()
    };
    let Err(err) = step!(
        "push",
        h.publish_push(Some(ingested), txn, std::collections::HashMap::default())
    ) else {
        panic!("a reclaiming checksum must not be pushed");
    };
    let msg = err.to_string();
    assert!(
        msg.contains(&dead) && msg.contains("reclaim"),
        "expected the push to be refused as reclaiming, got: {msg}"
    );
    Ok(())
}

/// #175: the manifest CAS is what orders reclamation against adoption. A pack
/// that is live must never be listed as reclaiming (GC would then be free to
/// delete a pack the manifest points at), and a non-live checksum lists and
/// clears through the same CAS.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaiming_list_never_contains_a_live_pack() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let live = h
        .manifest()
        .packs
        .first()
        .expect("push published a pack")
        .checksum
        .clone();

    // A live pack must not become a GC candidate, even when asked directly.
    step!(
        "list live",
        h.update_reclaiming(std::slice::from_ref(&live), &[], &[], "t", None)
    )?;
    assert!(
        h.manifest().reclaiming.is_empty(),
        "a live checksum was listed as reclaiming: {:?}",
        h.manifest().reclaiming
    );

    // A non-live checksum lists, then clears, through the same CAS.
    let dead = "f".repeat(40);
    step!(
        "list dead",
        h.update_reclaiming(std::slice::from_ref(&dead), &[], &[], "t", None)
    )?;
    assert!(
        h.manifest().reclaiming.iter().any(|r| r.checksum == dead),
        "a non-live checksum must list"
    );
    step!(
        "clear dead",
        h.update_reclaiming(&[], std::slice::from_ref(&dead), &[], "t", None)
    )?;
    assert!(
        h.manifest().reclaiming.is_empty(),
        "clearing must empty the list: {:?}",
        h.manifest().reclaiming
    );
    Ok(())
}

/// #175: bucket GC reclaims packs that a COMPACT entry superseded, but only
/// once the `.superseded` marker has aged past
/// `compaction.retention_superseded` — and never a pack that is still live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_reclaims_expired_superseded_packs_and_keeps_live_and_young_ones() -> anyhow::Result<()>
{
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::SupersededPack;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            // Non-zero so the planner schedules the unit; the marker ages below
            // decide what is actually reclaimed.
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.compaction.retention_superseded = std::time::Duration::from_hours(7 * 24);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;

    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let live = h
        .manifest()
        .packs
        .first()
        .expect("push published a pack")
        .checksum
        .clone();

    // A pack that a COMPACT entry dropped eight days ago: past the 7d window.
    let dead = "d".repeat(40);
    let mut old_ts = walgit_proto::time::now();
    old_ts.seconds -= 8 * 24 * 3600;
    // …and one dropped an hour ago: inside the window, must be kept.
    let young = "e".repeat(40);
    let young_ts = walgit_proto::time::now();

    // A marker on a *live* pack: "superseded then re-adopted" (or a marker
    // written by a CAS that never landed). GC must leave the pack alone.
    for (checksum, at, seq) in [
        (&dead, old_ts, 7u64),
        (&young, young_ts, 9u64),
        (&live, old_ts, 11u64),
    ] {
        let marker = SupersededPack {
            checksum: checksum.clone(),
            superseded_at: Some(at),
            seq,
        };
        step!(
            "marker",
            h.store().put_bytes(
                &keys::superseded_key(checksum),
                marker.encode_to_vec(),
                PutMode::Create,
            )
        )?;
    }
    // The live pack already has its objects (the push published it); only the
    // two synthetic ones need bodies.
    for checksum in [&dead, &young] {
        step!(
            "pack body",
            h.store()
                .put_bytes(&keys::pack_key(checksum), vec![0u8; 32], PutMode::Create)
        )?;
        step!(
            "idx",
            h.store()
                .put_bytes(&keys::idx_key(checksum), vec![0u8; 8], PutMode::Create)
        )?;
    }

    assert!(
        matches!(step!("plan", next_unit(&server.state, &id))?, Unit::Gc(_)),
        "GC should be the due unit"
    );
    step!("gc pass", run_pass(&server.state))?;

    step!("check dead pack", async {
        assert!(
            h.store().head(&keys::pack_key(&dead)).await?.is_none(),
            "superseded pack past retention must be reclaimed"
        );
        assert!(
            h.store().head(&keys::idx_key(&dead)).await?.is_none(),
            "side files go with the pack"
        );
        assert!(
            h.store()
                .head(&keys::superseded_key(&dead))
                .await?
                .is_none(),
            "the marker goes too"
        );
        assert!(
            h.store().head(&keys::pack_key(&young)).await?.is_some(),
            "a pack inside the retention window must survive"
        );
        assert!(
            h.store().head(&keys::pack_key(&live)).await?.is_some(),
            "a live pack must never be reclaimed, even with an old marker"
        );
        assert!(
            h.store()
                .head(&keys::superseded_key(&live))
                .await?
                .is_some(),
            "the live pack's marker is kept (dropping it is irreversible)"
        );
        Ok::<(), anyhow::Error>(())
    })?;

    // The pass leaves the record the planner reads next time.
    assert!(
        step!("gc.pb", h.store().get_bytes(keys::GC))?.is_some(),
        "the GC pass records gc.pb"
    );
    Ok(())
}

/// #175: the unit's bound counts *reclaimable* work, and anything left behind
/// keeps the unit due. Two ways the old shape was wrong, checked together:
/// the bound was applied before the live filter, so (a) an older marker on a
/// superseded-and-re-adopted (live) pack spent quota and starved a dead one, and
/// (b) a pass that stopped at the bound still reported `complete`, writing
/// `gc.pb` and sleeping a whole `gc_interval` with eligibility left on the floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_bounds_reclaimable_work_and_stays_due_while_eligible_markers_remain()
-> anyhow::Result<()> {
    use futures::StreamExt;
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::SupersededPack;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.compaction.retention_superseded = std::time::Duration::from_hours(7 * 24);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let live = h
        .manifest()
        .packs
        .first()
        .expect("push published a pack")
        .checksum
        .clone();

    let mut ts = walgit_proto::time::now();
    // The live pack's marker is the *oldest*: with the bound applied first it
    // would take the first slot every pass and never let the dead ones through.
    ts.seconds -= 10 * 24 * 3600;
    step!(
        "live marker",
        h.store().put_bytes(
            &keys::superseded_key(&live),
            SupersededPack {
                checksum: live.clone(),
                superseded_at: Some(ts),
                seq: 1,
            }
            .encode_to_vec(),
            PutMode::Create,
        )
    )?;
    // 33 dead packs, all past the 7d window: one more than the 32/unit bound.
    let mut deads = Vec::new();
    for i in 0u32..33 {
        let checksum = format!("{:040x}", 0xdead_0000u64 + u64::from(i));
        let mut at = walgit_proto::time::now();
        at.seconds -= 9 * 24 * 3600 + i64::from(i);
        step!(
            "dead marker",
            h.store().put_bytes(
                &keys::superseded_key(&checksum),
                SupersededPack {
                    checksum: checksum.clone(),
                    superseded_at: Some(at),
                    seq: 2 + u64::from(i),
                }
                .encode_to_vec(),
                PutMode::Create,
            )
        )?;
        step!(
            "dead body",
            h.store()
                .put_bytes(&keys::pack_key(&checksum), vec![0u8; 32], PutMode::Create)
        )?;
        deads.push(checksum);
    }

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    // Exactly the 32/unit bound of *dead* packs went, not 31 (the live marker
    // must not have spent a slot).
    let mut remaining = 0usize;
    for checksum in &deads {
        if h.store().head(&keys::pack_key(checksum)).await?.is_some() {
            remaining += 1;
        }
    }
    assert_eq!(
        remaining, 1,
        "the live marker must not spend quota: expected 32 dead reclaimed, {remaining} left"
    );
    // Work remains, so the pass must NOT record gc.pb and the unit stays due.
    assert!(
        h.store().get_bytes(keys::GC).await?.is_none(),
        "an incomplete pass must not write gc.pb"
    );
    assert!(
        matches!(
            step!("plan again", next_unit(&server.state, &id))?,
            Unit::Gc(_)
        ),
        "the GC unit must stay due while eligible markers remain"
    );

    // A clean pass (nothing left but the live marker) drains the rest and records.
    step!("second pass", run_pass(&server.state))?;
    let mut left = 0usize;
    let mut stream = h.store().list(keys::SUPERSEDED_DIR, None);
    while let Some(m) = stream.next().await {
        let m = m?;
        if m.key.starts_with(keys::SUPERSEDED_DIR) {
            left += 1;
        }
    }
    assert_eq!(
        left, 1,
        "only the live pack's marker stays (dropping it is irreversible)"
    );
    assert!(
        h.store().get_bytes(keys::GC).await?.is_some(),
        "a drained pass records gc.pb"
    );
    Ok(())
}

/// #175 / D24: `[compaction]` is a per-repo settings section, so GC must honour
/// the *effective* retention. A repo that asks for a longer provenance window
/// must not have its packs deleted on the host's shorter one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_honours_the_repo_retention_override() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::SupersededPack;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            // Host keeps 7d; the repo below overrides it to 30d.
            c.compaction.retention_superseded = std::time::Duration::from_hours(7 * 24);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    step!(
        "repo settings",
        h.publish_settings(
            "[compaction]\nretention_superseded = \"30d\"\n",
            "tester",
            "longer provenance window",
        )
    )?;
    assert_eq!(
        h.effective_config().compaction.retention_superseded,
        std::time::Duration::from_hours(30 * 24),
        "the repo override must reach the effective config"
    );

    // A pack dropped 8 days ago: past the host's 7d, inside the repo's 30d.
    let dead = "d".repeat(40);
    let mut at = walgit_proto::time::now();
    at.seconds -= 8 * 24 * 3600;
    step!(
        "marker",
        h.store().put_bytes(
            &keys::superseded_key(&dead),
            SupersededPack {
                checksum: dead.clone(),
                superseded_at: Some(at),
                seq: 5,
            }
            .encode_to_vec(),
            PutMode::Create,
        )
    )?;
    step!(
        "body",
        h.store()
            .put_bytes(&keys::pack_key(&dead), vec![0u8; 32], PutMode::Create)
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;
    assert!(
        h.store().head(&keys::pack_key(&dead)).await?.is_some(),
        "the repo's 30d window must win over the host's 7d"
    );
    Ok(())
}

/// #175: a GC pass that retired a marker but died before releasing its claim
/// leaves the checksum listed in `Manifest.reclaiming` forever — with no marker
/// it can never become a candidate again, and a publisher regenerating those
/// bytes is refused permanently. The next pass (it holds the lease) releases
/// such claims: marker gone + pack not live = a completed reclaim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_releases_a_claim_whose_marker_was_already_retired() -> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit, run_pass};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // The crash left: claimed, marker already retired, objects already gone —
    // and old enough (past the age fence) that no live holder can own it.
    let orphan = "b".repeat(40);
    {
        use prost::Message;
        use walgit_proto::keys;
        use walgit_store::{ObjectStoreExt, PutMode};
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("repo has a manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        let mut since = walgit_proto::time::now();
        since.seconds -= 3600;
        m.reclaiming.push(walgit_proto::v1::ReclaimingPack {
            checksum: orphan.clone(),
            since: Some(since),
            owner: "someone-else".into(),
            token: "their-token".into(),
            fence_until: None,
        });
        m.revision += 1;
        step!(
            "claim",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }
    step!("resync", h.sync())?;
    assert!(
        h.manifest().reclaiming.iter().any(|r| r.checksum == orphan),
        "the checksum must be claimed before the pass"
    );

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;
    assert!(
        !h.manifest().reclaiming.iter().any(|r| r.checksum == orphan),
        "a claim whose marker is already gone must be released: {:?}",
        h.manifest().reclaiming
    );
    Ok(())
}

/// #175: the crash-recovery release is fenced by claim *age* — a claim younger
/// than a few lease TTLs may still belong to a live pass whose lease we cannot
/// see, so the next pass must leave it alone rather than free the checksum for
/// re-adoption while that pass is still deleting from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_leaves_a_fresh_claim_to_its_holder() -> anyhow::Result<()> {
    use walgit_server::maintain::{Unit, next_unit, run_pass};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // A fresh claim with no marker: looks exactly like the crash-recovery
    // candidate, but it is too young to be a crashed pass's leftover.
    let fresh = "a".repeat(40);
    step!(
        "claim",
        h.update_reclaiming(std::slice::from_ref(&fresh), &[], &[], "t", None)
    )?;
    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;
    assert!(
        h.manifest().reclaiming.iter().any(|r| r.checksum == fresh),
        "a fresh claim must be left to its holder: {:?}",
        h.manifest().reclaiming
    );
    Ok(())
}

/// #175: a claim whose holder died *while its marker was still present* must be
/// adopted by the next pass — not left forever. Otherwise the pack is never
/// reclaimed and every push/compact regenerating those bytes is refused
/// permanently (`WalError::Reclaiming`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_takes_over_a_stale_claim_whose_marker_still_exists() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.compaction.retention_superseded = std::time::Duration::from_hours(7 * 24);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // A dead pack: aged marker + bodies, plus a stale claim from a holder that
    // died before it could finish (its marker is *still there*).
    let dead = "d".repeat(40);
    let mut old = walgit_proto::time::now();
    old.seconds -= 9 * 24 * 3600;
    step!(
        "marker",
        h.store().put_bytes(
            &keys::superseded_key(&dead),
            walgit_proto::v1::SupersededPack {
                checksum: dead.clone(),
                superseded_at: Some(old),
                seq: 5,
            }
            .encode_to_vec(),
            PutMode::Create,
        )
    )?;
    for key in [keys::pack_key(&dead), keys::idx_key(&dead)] {
        step!(
            "body",
            h.store().put_bytes(&key, vec![0u8; 32], PutMode::Create)
        )?;
    }
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("repo has a manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        let mut since = walgit_proto::time::now();
        since.seconds -= 3600;
        m.reclaiming.push(walgit_proto::v1::ReclaimingPack {
            checksum: dead.clone(),
            since: Some(since),
            owner: "dead-holder".into(),
            token: "their-token".into(),
            fence_until: None,
        });
        m.revision += 1;
        step!(
            "stale claim",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }
    step!("resync", h.sync())?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("check", async {
        assert!(
            h.store().head(&keys::pack_key(&dead)).await?.is_none(),
            "the adopter must finish the reclamation (pack reclaimed)"
        );
        assert!(
            h.store()
                .head(&keys::superseded_key(&dead))
                .await?
                .is_none(),
            "the marker goes with the pack"
        );
        assert!(
            !h.manifest().reclaiming.iter().any(|r| r.checksum == dead),
            "the stale claim must not survive: {:?}",
            h.manifest().reclaiming
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// #175: a claim left by a *previous pass of the same instance* (same owner,
/// different token) with its marker already retired must be recovered too — the
/// "not ours" test is `(owner, token)`, not `owner`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_releases_a_retired_claim_from_an_older_pass_of_the_same_instance() -> anyhow::Result<()>
{
    use prost::Message;
    use walgit_proto::keys;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // Same instance id as this pass (it is the same process), older token, no
    // marker: an interrupted previous pass of ours.
    let orphan = "e".repeat(40);
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("repo has a manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        let mut since = walgit_proto::time::now();
        since.seconds -= 3600;
        m.reclaiming.push(walgit_proto::v1::ReclaimingPack {
            checksum: orphan.clone(),
            since: Some(since),
            owner: walgit_store::coord::instance_id().to_string(),
            token: "token-of-a-previous-pass".into(),
            fence_until: None,
        });
        m.revision += 1;
        step!(
            "claim",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }
    step!("resync", h.sync())?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;
    assert!(
        !h.manifest().reclaiming.iter().any(|r| r.checksum == orphan),
        "an older token of this instance is a different holder and must be released: {:?}",
        h.manifest().reclaiming
    );
    Ok(())
}

/// #177: 存量孤儿没有任何 marker —— 在 marker 机制（#176）出现之前被 supersede
/// 的 pack、上传被放弃的 pack、以及 `.pack` 已被删但 side-file 还在的残骸，都
/// 不在 `wal/_superseded/` 里，GC 的候选扫描永远看不到它们（本机全桶实测：
/// 0 个 marker vs 2,200+ 孤儿 pack）。reconcile 必须给「新鲜 manifest 不再引用
/// 且没有 marker」的对象补上标记，同时绝不碰 live 的 pack。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_marks_orphans_without_markers_and_never_touches_live_packs() -> anyhow::Result<()> {
    use walgit_proto::keys;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.compaction.retention_superseded = std::time::Duration::from_hours(7 * 24);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let live = h
        .manifest()
        .packs
        .first()
        .expect("push published a pack")
        .checksum
        .clone();

    // ① 孤儿 pack（有 pack + idx，没有任何 marker）——存量数据的形态。
    let orphan = "a".repeat(40);
    // ② 只有 side-file 的残骸（pack 已不在）——同样没有 marker。
    let side_only = "b".repeat(40);
    // ③ 删除侧不认识的扩展名（将来新增 side-file 的形态）：**不得标记**，否则
    // 会「标记 → 删不掉 → 下一轮再标记」地空转。
    let unknown_ext = "c".repeat(40);
    for (key, body) in [
        (keys::pack_key(&orphan), vec![0u8; 32]),
        (keys::idx_key(&orphan), vec![0u8; 8]),
        (keys::idx_key(&side_only), vec![0u8; 8]),
        (format!("wal/{unknown_ext}.mtimes"), vec![0u8; 8]),
    ] {
        step!(
            "orphan object",
            h.store().put_bytes(&key, body, PutMode::Create)
        )?;
    }

    assert!(
        matches!(step!("plan", next_unit(&server.state, &id))?, Unit::Gc(_)),
        "GC should be the due unit"
    );
    step!("gc pass", run_pass(&server.state))?;

    step!("check markers", async {
        for (what, checksum) in [("orphan pack", &orphan), ("side-file remnant", &side_only)] {
            let marker = h.store().get_bytes(&keys::superseded_key(checksum)).await?;
            assert!(
                marker.is_some(),
                "{what} must be marked so a later pass can reclaim it"
            );
        }
        assert!(
            h.store()
                .head(&keys::superseded_key(&live))
                .await?
                .is_none(),
            "a live pack must never be marked as superseded"
        );
        assert!(
            h.store()
                .head(&keys::superseded_key(&unknown_ext))
                .await?
                .is_none(),
            "an extension the reclamation loop cannot delete must not be marked"
        );
        // Stamped `now`: inside the retention window, so nothing is deleted yet.
        assert!(
            h.store().head(&keys::pack_key(&orphan)).await?.is_some(),
            "the orphan is marked, not reclaimed: retention still applies"
        );
        assert!(
            h.store().head(&keys::pack_key(&live)).await?.is_some(),
            "the live pack is untouched"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    assert!(
        step!("gc.pb", h.store().get_bytes(keys::GC))?.is_some(),
        "a completed reconcile records gc.pb"
    );

    // 第二次 pass：标记已存在就不再重复写（老时间戳不会被刷新成新时间）。
    let before = step!(
        "marker body",
        h.store().get_bytes(&keys::superseded_key(&orphan))
    )?
    .expect("marker")
    .1;
    step!("second gc pass", run_pass(&server.state))?;
    let after = step!(
        "marker body again",
        h.store().get_bytes(&keys::superseded_key(&orphan))
    )?
    .expect("marker")
    .1;
    assert_eq!(
        before, after,
        "an existing marker must not be rewritten (its stamp is the conservative one)"
    );
    Ok(())
}

/// reconcile 也必须是有界单元：一轮最多写 `GC_MAX_MARKERS_PER_UNIT` 个标记，剩下
/// 的孤儿保持单元 due（不写 gc.pb），下一轮继续 —— 否则一个几万个对象的仓库会把
/// 单个单元拖出 lease 预算。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_reconcile_is_bounded_and_stays_due_while_orphans_remain() -> anyhow::Result<()> {
    use futures::StreamExt;
    use walgit_proto::keys;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_server::ops::GC_MAX_MARKERS_PER_UNIT;
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // 比一轮上限多 3 个孤儿。
    let total = GC_MAX_MARKERS_PER_UNIT + 3;
    for i in 0u64..total as u64 {
        let checksum = format!("{:040x}", 0x0bad_0000u64 + i);
        step!(
            "orphan pack",
            h.store()
                .put_bytes(&keys::pack_key(&checksum), vec![0u8; 32], PutMode::Create)
        )?;
    }

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    // 只有一轮上限的标记被写出。
    let mut markers = 0usize;
    let mut stream = h.store().list(keys::SUPERSEDED_DIR, None);
    while let Some(m) = stream.next().await {
        m?;
        markers += 1;
    }
    assert_eq!(
        markers, GC_MAX_MARKERS_PER_UNIT,
        "one pass must write at most the per-unit marker bound"
    );
    assert!(
        h.store().get_bytes(keys::GC).await?.is_none(),
        "an incomplete reconcile must not record gc.pb"
    );
    assert!(
        matches!(
            step!("plan again", next_unit(&server.state, &id))?,
            Unit::Gc(_)
        ),
        "the unit stays due while orphans remain unmarked"
    );
    Ok(())
}

/// #194：一个**永久失败**的高优先级单元不得饿死它下面的工作。实测生产里 9 个仓库
/// 有 6 个每 pass 选中 `bundle` 且全部失败（base seq 处缺 checkpoint），而 `gc`
/// 排在后面 → 全桶 `gc.pb` 恒为 0、孤儿永远不被回收。
///
/// 这里用最省事的确定性失败源：`fsck.pb` 列出缺失对象 + `upstream.git` 指向不存在
/// 的路径 → `repair`（优先级高于 `gc`）每 pass 必失败。断言：达到退避阈值后，
/// **同一个 pass** 就能把机会交给 `gc`；而当 `gc` 不再 due、`repair` 成为唯一工作
/// 时，它仍然会被重试（自愈循环的意义就在于此）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permanently_failing_unit_hands_the_pass_to_lower_priority_work() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::FsckReport;
    use walgit_server::maintain::{
        BACKOFF_AFTER_FAILURES, Backoff, Unit, next_unit, run_pass_with_backoff,
    };
    use walgit_store::{ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            // Deterministic repair failure: the only source it may fetch from
            // does not exist.
            c.upstream.git = Some("https://127.0.0.1:9/nonexistent-gc-fairness.git".to_string());
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let fsck = FsckReport {
        seq: h.manifest().head_seq,
        at: Some(walgit_proto::time::now()),
        host: "fairness-test".into(),
        missing: vec!["0".repeat(40)],
        missing_total: 1,
        problems: 0,
        elapsed_secs: 0.0,
        repaired_seq: 0,
    };
    step!(
        "fsck.pb",
        h.store()
            .put_bytes(keys::FSCK, fsck.encode_to_vec(), PutMode::Overwrite)
    )?;
    assert!(
        matches!(
            step!("plan", next_unit(&server.state, &id))?,
            Unit::Repair(_)
        ),
        "a repair with missing objects outranks gc"
    );

    let mut backoff = Backoff::default();
    for strike in 1..=BACKOFF_AFTER_FAILURES {
        step!(
            "failing pass",
            run_pass_with_backoff(&server.state, &mut backoff)
        )?;
        assert_eq!(
            backoff.strikes(&id, "repair"),
            strike,
            "the failing repair keeps its strike count"
        );
        if strike < BACKOFF_AFTER_FAILURES {
            assert!(
                h.store().get_bytes(keys::GC).await?.is_none(),
                "below the threshold the pass still ends at the failing unit"
            );
        }
    }
    assert!(
        h.store().get_bytes(keys::GC).await?.is_some(),
        "once the repair reaches the backoff threshold, gc must run in that same pass"
    );

    // gc is not due again for an hour: the backing-off repair is now the only
    // work, and the loop must still retry it rather than idle forever.
    let report = run_pass_with_backoff(&server.state, &mut backoff).await?;
    assert_eq!(report.units, 0, "nothing else is due");
    assert_eq!(
        report.skipped, 1,
        "the only due unit is the backing-off repair — it still runs (and fails)"
    );
    Ok(())
}

/// #194 复审：**多个** kind 同时处于退避时，兜底不能只做一次「无 backoff」规划 ——
/// 那样永远选中优先级最高且仍在失败的那个，较低的 kind 依旧永远拿不到机会。
/// 这里预置 `repair`（必然失败）与 `gc` 都在退避，断言 `gc` 仍能在同一个 pass 拿到
/// 机会（`ran_kinds` 的轮转 + 单元预算是这条路径的唯一依赖）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backing_off_kinds_rotate_instead_of_starving_the_lower_one() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::FsckReport;
    use walgit_server::maintain::{BACKOFF_AFTER_FAILURES, Backoff, run_pass_with_backoff};
    use walgit_store::{ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.upstream.git = Some("https://127.0.0.1:9/nonexistent-gc-fairness.git".to_string());
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;
    let fsck = FsckReport {
        seq: h.manifest().head_seq,
        at: Some(walgit_proto::time::now()),
        host: "fairness-test".into(),
        missing: vec!["0".repeat(40)],
        missing_total: 1,
        problems: 0,
        elapsed_secs: 0.0,
        repaired_seq: 0,
    };
    step!(
        "fsck.pb",
        h.store()
            .put_bytes(keys::FSCK, fsck.encode_to_vec(), PutMode::Overwrite)
    )?;

    // Both kinds are already past the threshold: `repair` keeps failing, `gc`
    // would succeed but sits below it in the priority order.
    let mut backoff = Backoff::default();
    for _ in 0..BACKOFF_AFTER_FAILURES {
        backoff.record_failure(&id, "repair");
        backoff.record_failure(&id, "gc");
    }
    let report = run_pass_with_backoff(&server.state, &mut backoff).await?;
    assert!(
        report.skipped >= 1,
        "the failing kind still gets its turn (retrying is the loop's job)"
    );
    assert!(
        h.store().get_bytes(keys::GC).await?.is_some(),
        "the lower backed-off kind (gc) must get a turn instead of the higher one taking every fallback"
    );
    Ok(())
}

/// #195 端到端：base 在 seq B 发布（并写下见证）→ 之后 push 让 head 前进、再 fold
/// 把 live checkpoint 推过 B → **bundle compose 仍然成功**。修复前这条路径会失败在
/// `refs at seq B are not replayable: log folded up to … and no checkpoint at or before`，
/// 生产里 6 个仓库的 bundle 因此每 pass 失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bundle_compose_works_after_a_fold_moved_past_the_base() -> anyhow::Result<()> {
    use std::collections::HashMap;
    use walgit_store::ObjectStore;
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![
                walgit_config::Role::Serve,
                walgit_config::Role::Maintain,
                walgit_config::Role::Compact,
                walgit_config::Role::Bundle,
            ];
            c.maintenance.disk = walgit_config::MaintainerDisk::Ssd;
            c.cache.mode = walgit_config::CacheMode::Disk;
            c.bundles.enabled = true;
            c.bundles.strategy.truncate(1); // weekly only
            c.compaction.enabled = true;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // 1. Build the base (this is what #195 patches: it also writes the witness).
    let mut params = HashMap::new();
    params.insert("base".to_string(), "1".to_string());
    params.insert("force".to_string(), "1".to_string());
    assert!(
        step!(
            "base rebuild",
            walgit_server::maintain::run_op(&server.state, &id, "compact", params)
        ),
        "the base rebuild must succeed"
    );
    step!("sync after rebuild", h.sync())?;
    let base_seq = step!("base", async {
        let base = walgit_wal::base_pack(&h.manifest())
            .cloned()
            .expect("a live tier-2 base");
        Ok::<u64, anyhow::Error>(base.seq)
    })?;
    assert!(
        h.store()
            .head(&walgit_proto::keys::checkpoint_key(base_seq))
            .await?
            .is_some(),
        "the rebuild must leave an exact witness checkpoint at the base's seq"
    );

    // Retry path: if the publish committed but the witness write failed, the
    // next rebuild sees the base already live and must still re-ensure the
    // witness rather than skipping the publish path (#195 B2).
    h.store()
        .delete(&walgit_proto::keys::checkpoint_key(base_seq), None)
        .await?;
    h.store()
        .delete(&walgit_proto::keys::checkpoint_refs_key(base_seq), None)
        .await?;
    let mut retry_params = HashMap::new();
    retry_params.insert("base".to_string(), "1".to_string());
    retry_params.insert("force".to_string(), "1".to_string());
    assert!(
        step!(
            "base rebuild witness retry",
            walgit_server::maintain::run_op(&server.state, &id, "compact", retry_params)
        ),
        "a retry over an already-live base must re-ensure its witness"
    );
    assert!(
        h.store()
            .head(&walgit_proto::keys::checkpoint_key(base_seq))
            .await?
            .is_some()
            && h.store()
                .head(&walgit_proto::keys::checkpoint_refs_key(base_seq))
                .await?
                .is_some(),
        "the retry must rewrite both witness objects"
    );

    // 2. Advance past the base and fold: the live checkpoint now sits above base_seq.
    std::fs::write(src.path().join("b.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync after push", h.sync())?;
    let mut cp_params = HashMap::new();
    cp_params.insert("trigger".to_string(), "test".to_string());
    assert!(step!(
        "fold",
        walgit_server::maintain::run_op(&server.state, &id, "checkpoint", cp_params)
    ));
    step!("sync after fold", h.sync())?;
    let live_cp = h
        .manifest()
        .checkpoint
        .as_ref()
        .map(|c| c.seq)
        .unwrap_or_default();
    assert!(
        live_cp > base_seq,
        "the fold must move the live checkpoint past the base ({live_cp} > {base_seq})"
    );

    // 3. Compose the weekly bundle: its compose replays refs at the base's seq.
    let weekly = server.state.cfg.bundles.strategy[0].clone();
    let slot = walgit_bundle::slots::last_slot_at_or_before(&weekly, std::time::SystemTime::now())?
        .expect("a weekly slot");
    let mut b_params = HashMap::new();
    b_params.insert("strategy".to_string(), weekly.name.clone());
    b_params.insert("slot".to_string(), slot.to_string());
    assert!(
        step!(
            "compose",
            walgit_server::maintain::run_op(&server.state, &id, "bundle", b_params)
        ),
        "compose must succeed even though the WAL folded past the base (was: refs not replayable)"
    );

    // The header must carry the base's tip, not the live tip. A compose that
    // silently falls back to the newer checkpoint would finish "successfully"
    // but produce an unclonable/mislabeled bundle; assert the actual bytes.
    let list = walgit_bundle::ops::read_list(h.store())
        .await?
        .expect("bundle list");
    let entry = list
        .bundles
        .iter()
        .find(|b| b.strategy == weekly.name)
        .expect("weekly bundle entry");
    let header = server
        .get_text(
            &format!("/o/r.git/{}", entry.key),
            &[("Range", "bytes=0-511")],
        )
        .await?;
    assert!(
        header.starts_with("# v2 git bundle\n")
            && header.contains(&format!("{c1} refs/heads/main")),
        "weekly header must carry the base seq's tip {c1}: {header:?}"
    );
    assert!(
        !header.contains(&c2),
        "weekly header must not carry the later tip {c2}: {header:?}"
    );
    Ok(())
}

/// #214 回归：单个 bitmap'd base、未 predates window，但 exact witness
/// 已丢失时，planner 必须选 BaseRebuild，不能继续把它交给 bundle。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planner_rebuilds_a_base_that_lost_its_witness() -> anyhow::Result<()> {
    use std::collections::HashMap;
    use walgit_server::maintain::{Unit, next_unit};
    use walgit_store::ObjectStore;

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.server.roles = vec![
                walgit_config::Role::Serve,
                walgit_config::Role::Maintain,
                walgit_config::Role::Compact,
                walgit_config::Role::Bundle,
            ];
            c.maintenance.disk = walgit_config::MaintainerDisk::Ssd;
            c.cache.mode = walgit_config::CacheMode::Disk;
            c.bundles.enabled = true;
            c.bundles.strategy.truncate(1); // weekly only
            c.compaction.enabled = true;
            c.maintenance.checkpoints = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let mut base_params = HashMap::new();
    base_params.insert("base".to_string(), "1".to_string());
    base_params.insert("force".to_string(), "1".to_string());
    assert!(
        step!(
            "base rebuild",
            walgit_server::maintain::run_op(&server.state, &id, "compact", base_params)
        ),
        "the base rebuild must succeed"
    );
    step!("sync after rebuild", h.sync())?;
    let base_seq = walgit_wal::base_pack(&h.manifest())
        .cloned()
        .expect("a live tier-2 base")
        .seq;
    assert!(
        h.store()
            .head(&walgit_proto::keys::checkpoint_key(base_seq))
            .await?
            .is_some(),
        "the rebuild must leave an exact witness"
    );

    std::fs::write(src.path().join("b.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    step!("sync after push", h.sync())?;
    let mut cp_params = HashMap::new();
    cp_params.insert("trigger".to_string(), "test".to_string());
    assert!(step!(
        "fold",
        walgit_server::maintain::run_op(&server.state, &id, "checkpoint", cp_params)
    ));
    step!("sync after fold", h.sync())?;
    let live_cp = h
        .manifest()
        .checkpoint
        .as_ref()
        .map(|c| c.seq)
        .unwrap_or_default();
    assert!(live_cp > base_seq, "the fold must move past the base");

    // Self-isolating control: with the witness still present the planner is on
    // the bundle path; only deleting it may flip the decision to BaseRebuild.
    let before = step!("plan before witness removal", next_unit(&server.state, &id))?;
    assert!(
        matches!(before, Unit::BundleSlot(ref s, _) if s == "weekly"),
        "control state must be BundleSlot, got {before:?}"
    );

    // Simulate the #214 production state: the old base's witness pair is gone,
    // while the base itself is still the one live bitmap'd base and no ref
    // change happened before the weekly window start.
    h.store()
        .delete(&walgit_proto::keys::checkpoint_key(base_seq), None)
        .await?;
    h.store()
        .delete(&walgit_proto::keys::checkpoint_refs_key(base_seq), None)
        .await?;
    assert!(
        h.refs_at_seq(base_seq).await.is_err(),
        "the base must be unreplayable without its witness"
    );

    let unit = step!("plan", next_unit(&server.state, &id))?;
    assert!(
        matches!(unit, Unit::BaseRebuild(ref s, _) if s == "weekly"),
        "planner must rebuild the base, got {unit:?}"
    );
    Ok(())
}

/// #175 剩余：folded WAL 对象回收 —— `manifest.min_seq` 以下的 log 段与不再
/// 被引用的 checkpoint 目录，在超过 `maintenance.retention_wal` 之后应被删除；
/// live checkpoint、live log 段必须留下。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_reclaims_folded_wal_objects_past_the_window() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::{CheckpointRef, LogSegmentRef};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    // Candidates are 8 days old; the live checkpoint is *fresh* on purpose.
    // The old global anchor would keep everything forever in an active repo.
    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 20u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(fresh.clone()),
            first_state_at: Some(fresh.clone()),
            as_of: Some(fresh.clone()),
        });
        m.min_seq = 6; // everything below 6 is folded into that checkpoint
        m.log_segments = vec![LogSegmentRef {
            key: keys::log_segment_key(15),
            first_seq: 15,
            last_seq: 19,
            size: 0,
            sealed: true,
        }];
        m.packs.clear(); // no base pack: no checkpoint is needed for compose
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version)
            )
        )?;
    }

    // Folded log segment (below min_seq) and the live one.
    step!(
        "old log segment",
        h.store().put_bytes(
            &keys::log_segment_key(5),
            log_segment_bytes(5, &old),
            PutMode::Create,
        )
    )?;
    step!(
        "live log segment",
        h.store().put_bytes(
            &keys::log_segment_key(15),
            log_segment_bytes(15, &fresh),
            PutMode::Create,
        )
    )?;
    // An unreferenced old checkpoint dir, and the live one.
    step!(
        "old checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(5),
            checkpoint_bytes(5, &old),
            PutMode::Create,
        )
    )?;
    step!(
        "old checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(5),
            refs_snapshot_bytes(&old),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint_bytes(live_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("assertions", async {
        assert!(
            h.store().head(&keys::log_segment_key(5)).await?.is_none(),
            "a folded log segment past the window must be reclaimed"
        );
        assert!(
            h.store().head(&keys::log_segment_key(15)).await?.is_some(),
            "a live log segment must stay"
        );
        assert!(
            h.store().head(&keys::checkpoint_key(5)).await?.is_none(),
            "an unreferenced checkpoint past the window must be reclaimed"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_refs_key(5))
                .await?
                .is_none(),
            "the checkpoint's refs snapshot goes with it"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(live_seq))
                .await?
                .is_some(),
            "the live checkpoint must stay"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// `walgit compact --base` 可能把 base 建在一个*更老*的 checkpoint 上：compose 要靠
/// 它重放 refs。回收必须保住「每个 live base 的 seq 处、最新的那个 checkpoint」——
/// 删了它就是 #195 那种 `refs not replayable`。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_keeps_the_checkpoint_a_live_base_needs() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::{CheckpointRef, LogSegmentRef, PackKind, PackRef};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let mut old = walgit_proto::time::now();
    old.seconds -= 8 * 24 * 3600;
    let live_seq = 30u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(old),
            first_state_at: Some(old),
            as_of: Some(old),
        });
        // Legal invariant: the live checkpoint folded everything up to its seq.
        m.min_seq = live_seq + 1;
        m.log_segments = vec![
            LogSegmentRef {
                key: keys::log_segment_key(2),
                first_seq: 2,
                last_seq: 8,
                size: 0,
                sealed: true,
            },
            LogSegmentRef {
                key: keys::log_segment_key(11),
                first_seq: 11,
                last_seq: 12,
                size: 0,
                sealed: true,
            },
        ];
        // A live base at seq 12: the newest checkpoint at or before it is 10.
        m.packs = vec![PackRef {
            checksum: "a".repeat(40),
            seq: 12,
            tier: 2,
            kind: PackKind::Objects as i32,
            ..Default::default()
        }];
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version)
            )
        )?;
    }
    let refs = walgit_proto::v1::RefSnapshot::default().encode_to_vec();
    for seq in [5u64, 10, live_seq] {
        step!(
            "checkpoint object",
            h.store()
                .put_bytes(&keys::checkpoint_key(seq), vec![0u8; 16], PutMode::Create)
        )?;
        step!(
            "checkpoint refs",
            h.store().put_bytes(
                &keys::checkpoint_refs_key(seq),
                refs.clone(),
                PutMode::Create
            )
        )?;
    }
    // 2 ends before the witness checkpoint (10) → dead weight; 11 covers
    // (10, 12] → the base's replay tail; 20 is past the base → dead weight.
    for seq in [2u64, 11, 20] {
        step!(
            "log segment",
            h.store()
                .put_bytes(&keys::log_segment_key(seq), vec![0u8; 16], PutMode::Create)
        )?;
    }

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("assertions", async {
        assert!(
            h.store().head(&keys::checkpoint_key(10)).await?.is_some(),
            "the newest checkpoint at or before the live base (seq 12) must be kept"
        );
        assert!(
            h.store().head(&keys::checkpoint_key(5)).await?.is_some(),
            "with an unprovable witness even older checkpoints stay (fail closed)"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(live_seq))
                .await?
                .is_some(),
            "the live checkpoint must stay"
        );
        // Fail closed (#195): this base has **no exact checkpoint at its seq**,
        // so which folded segments a rebuild needs cannot be proven — keep them
        // all rather than deleting evidence.
        for seq in [2u64, 11, 20] {
            assert!(
                h.store().head(&keys::log_segment_key(seq)).await?.is_some(),
                "folded segment {seq} must be kept while the base's witness is unprovable"
            );
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// #206 fail-safe: a candidate with a missing/invalid timestamp is kept.
/// `prost` clamps malformed Timestamp values into the epoch, so GC must reject
/// them before calling `time::to_system`; the newest log entry missing its time
/// must not fall back to an older entry's age.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_keeps_objects_with_missing_or_invalid_timestamps() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::{CheckpointRef, LogEntry, LogSegmentRef};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 20u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = 10;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(fresh.clone()),
            first_state_at: Some(fresh.clone()),
            as_of: Some(fresh.clone()),
        });
        m.log_segments = vec![LogSegmentRef {
            key: keys::log_segment_key(15),
            first_seq: 15,
            last_seq: 19,
            size: 0,
            sealed: true,
        }];
        m.packs.clear();
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }

    step!(
        "live checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint_bytes(live_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;

    // Newest entry has no timestamp; only the older entry is old. Do not use
    // the older entry's timestamp as a fallback.
    let entries = [
        LogEntry {
            seq: 4,
            created_at: Some(old.clone()),
            ..Default::default()
        },
        LogEntry {
            seq: 5,
            created_at: None,
            ..Default::default()
        },
    ];
    step!(
        "timestampless log",
        h.store().put_bytes(
            &keys::log_segment_key(5),
            walgit_proto::frame::encode_entries(entries.iter()),
            PutMode::Create,
        )
    )?;
    let mut invalid = walgit_proto::time::now();
    invalid.nanos = -1;
    step!(
        "invalid checkpoint",
        h.store().put_bytes(
            &keys::checkpoint_key(6),
            checkpoint_bytes(6, &invalid),
            PutMode::Create,
        )
    )?;
    step!(
        "invalid refs snapshot",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(6),
            refs_snapshot_bytes(&invalid),
            PutMode::Create,
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    assert!(
        h.store().head(&keys::log_segment_key(5)).await?.is_some(),
        "a log whose newest entry has no timestamp must be kept"
    );
    assert!(
        h.store().head(&keys::checkpoint_key(6)).await?.is_some(),
        "a checkpoint with an invalid timestamp must be kept"
    );
    assert!(
        h.store()
            .head(&keys::checkpoint_refs_key(6))
            .await?
            .is_some(),
        "the checkpoint refs remnant must stay with it"
    );
    Ok(())
}

/// A checkpoint delete can be interrupted after refs.pb was removed but before
/// checkpoint.pb was removed (the deliberate delete order). The remaining
/// checkpoint object must still carry enough age for the next pass to drain it;
/// this test starts from exactly that remnant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_drains_a_checkpoint_remnant_after_an_interrupted_delete() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::CheckpointRef;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 20u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = 10;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(fresh.clone()),
            first_state_at: Some(fresh.clone()),
            as_of: Some(fresh.clone()),
        });
        m.log_segments.clear();
        m.packs.clear();
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }
    step!(
        "live checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint_bytes(live_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;
    // Simulate the interrupted directory: refs.pb is already gone.
    step!(
        "checkpoint remnant",
        h.store().put_bytes(
            &keys::checkpoint_key(5),
            checkpoint_bytes(5, &old),
            PutMode::Create,
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;
    assert!(
        h.store().head(&keys::checkpoint_key(5)).await?.is_none(),
        "a checkpoint.pb remnant must be drained by its own timestamp"
    );
    Ok(())
}

/// The deletion bound must never split a checkpoint directory. Fill the pass
/// with 511 old log objects; the next candidate is a two-object checkpoint.
/// It must be left whole for the next pass, then deleted whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_keeps_a_checkpoint_directory_whole_at_the_delete_bound() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::CheckpointRef;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 1_000u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = live_seq + 1;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(fresh.clone()),
            first_state_at: Some(fresh.clone()),
            as_of: Some(fresh.clone()),
        });
        m.log_segments.clear();
        m.packs.clear();
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }
    step!(
        "live checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint_bytes(live_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;
    step!("old logs", async {
        for seq in 1..=511u64 {
            h.store()
                .put_bytes(
                    &keys::log_segment_key(seq),
                    log_segment_bytes(seq, &old),
                    PutMode::Create,
                )
                .await?;
        }
        Ok::<(), walgit_store::StoreError>(())
    })?;
    step!(
        "boundary checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(600),
            checkpoint_bytes(600, &old),
            PutMode::Create,
        )
    )?;
    step!(
        "boundary checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(600),
            refs_snapshot_bytes(&old),
            PutMode::Create,
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("first gc pass", run_pass(&server.state))?;
    assert!(
        h.store().head(&keys::checkpoint_key(600)).await?.is_some()
            && h.store()
                .head(&keys::checkpoint_refs_key(600))
                .await?
                .is_some(),
        "the checkpoint directory must not be split at the bound"
    );

    assert!(matches!(
        step!("plan again", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("second gc pass", run_pass(&server.state))?;
    assert!(
        h.store().head(&keys::checkpoint_key(600)).await?.is_none()
            && h.store()
                .head(&keys::checkpoint_refs_key(600))
                .await?
                .is_none(),
        "the next pass must delete the directory whole"
    );
    Ok(())
}

/// 并发保护的守门测试：checkpoint 目录是**先 PUT 不可变对象、再 CAS manifest** 的，
/// 所以快照里可能还没有一个正在写入的、seq 更大的目录。GC 只允许删**严格小于** live
/// seq 的目录；更大的一律留给后续 pass（否则那个 CAS 落地后会指向被删对象）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_keeps_checkpoint_dirs_newer_than_the_live_one() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::CheckpointRef;
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 30u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = live_seq + 1;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(old),
            first_state_at: Some(old),
            as_of: Some(old),
        });
        m.log_segments.clear();
        m.packs.clear();
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version)
            )
        )?;
    }
    for (seq, at) in [
        (20u64, &old),
        (live_seq, &fresh),
        (31u64, &fresh),
        (40u64, &fresh),
    ] {
        step!(
            "checkpoint object",
            h.store().put_bytes(
                &keys::checkpoint_key(seq),
                checkpoint_bytes(seq, at),
                PutMode::Create
            )
        )?;
        step!(
            "checkpoint refs",
            h.store().put_bytes(
                &keys::checkpoint_refs_key(seq),
                refs_snapshot_bytes(at),
                PutMode::Create
            )
        )?;
    }

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("assertions", async {
        assert!(
            h.store().head(&keys::checkpoint_key(20)).await?.is_none(),
            "an older, unreferenced checkpoint is reclaimed"
        );
        assert!(
            h.store().head(&keys::checkpoint_key(31)).await?.is_some(),
            "a directory newer than the live checkpoint may belong to an in-flight CAS — keep it"
        );
        assert!(
            h.store().head(&keys::checkpoint_key(40)).await?.is_some(),
            "same for any newer directory"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// 反面：当 live base 在它自己的 seq 处**有完整、可验证的 exact witness**
/// （`checkpoint.pb` + `refs.pb` 都在），fail-closed 就该解除，回收恢复：更老的
/// checkpoint 与 folded log 段都可以删，只有 live 对象与 exact witness 留下。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_reclaims_folded_wal_objects_when_the_base_witness_is_complete() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::{CheckpointRef, PackKind, PackRef};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let old = old_timestamp();
    let fresh = walgit_proto::time::now();
    let live_seq = 30u64;
    let base_seq = 12u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = live_seq + 1;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(fresh.clone()),
            first_state_at: Some(fresh.clone()),
            as_of: Some(fresh.clone()),
        });
        m.log_segments.clear();
        m.packs = vec![PackRef {
            checksum: "a".repeat(40),
            seq: base_seq,
            tier: 2,
            kind: PackKind::Objects as i32,
            ..Default::default()
        }];
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version)
            )
        )?;
    }
    // 5: old dead weight. 12: the base's exact, complete witness. 30: live.
    step!(
        "old checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(5),
            checkpoint_bytes(5, &old),
            PutMode::Create,
        )
    )?;
    step!(
        "old checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(5),
            refs_snapshot_bytes(&old),
            PutMode::Create,
        )
    )?;
    step!(
        "base witness object",
        h.store().put_bytes(
            &keys::checkpoint_key(base_seq),
            checkpoint_bytes(base_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "base witness refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(base_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint object",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint_bytes(live_seq, &fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "live checkpoint refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs_snapshot_bytes(&fresh),
            PutMode::Create,
        )
    )?;
    step!(
        "folded log",
        h.store().put_bytes(
            &keys::log_segment_key(20),
            log_segment_bytes(20, &old),
            PutMode::Create,
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("assertions", async {
        assert!(
            h.store().head(&keys::checkpoint_key(5)).await?.is_none(),
            "with a complete witness the older checkpoint is reclaimable again"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(base_seq))
                .await?
                .is_some(),
            "the exact witness must stay"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(live_seq))
                .await?
                .is_some(),
            "the live checkpoint must stay"
        );
        assert!(
            h.store().head(&keys::log_segment_key(20)).await?.is_none(),
            "folded logs are reclaimable again once the witness is provable"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_fails_closed_when_the_base_witness_is_partial() -> anyhow::Result<()> {
    use prost::Message;
    use walgit_proto::keys;
    use walgit_proto::v1::{CheckpointRef, PackKind, PackRef};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync())?;

    let mut old = walgit_proto::time::now();
    old.seconds -= 8 * 24 * 3600;
    let live_seq = 30u64;
    let base_seq = 12u64;
    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = live_seq + 1;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(old),
            first_state_at: Some(old),
            as_of: Some(old),
        });
        m.log_segments.clear();
        m.packs = vec![PackRef {
            checksum: "a".repeat(40),
            seq: base_seq,
            tier: 2,
            kind: PackKind::Objects as i32,
            ..Default::default()
        }];
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version)
            )
        )?;
    }
    let refs = walgit_proto::v1::RefSnapshot::default().encode_to_vec();
    // 5 and 30: checkpoint objects. 12: a *partial* directory — the writer's
    // paired PUTs are not atomic, so `checkpoint.pb` can exist without
    // `refs.pb`. That is not a witness, and it must not switch fail-closed off.
    for (seq, complete) in [(5u64, false), (base_seq, false), (live_seq, true)] {
        step!(
            "checkpoint object",
            h.store()
                .put_bytes(&keys::checkpoint_key(seq), vec![0u8; 16], PutMode::Create)
        )?;
        if complete {
            step!(
                "checkpoint refs",
                h.store().put_bytes(
                    &keys::checkpoint_refs_key(seq),
                    refs.clone(),
                    PutMode::Create
                )
            )?;
        }
    }
    step!(
        "folded log",
        h.store()
            .put_bytes(&keys::log_segment_key(20), vec![0u8; 16], PutMode::Create)
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    step!("assertions", async {
        assert!(
            h.store().head(&keys::checkpoint_key(5)).await?.is_some(),
            "a half-written witness directory must not unlock reclamation"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(base_seq))
                .await?
                .is_some(),
            "the exact witness must stay"
        );
        assert!(
            h.store()
                .head(&keys::checkpoint_key(live_seq))
                .await?
                .is_some(),
            "the live checkpoint must stay"
        );
        assert!(
            h.store().head(&keys::log_segment_key(20)).await?.is_some(),
            "folded logs stay while the witness is unprovable"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// #175 / D24: a GC pass must not act on a per-repo retention setting that was
/// published after this instance's last refs sync. `sync_refs` can legally skip
/// the conditional GET under `wal.freshness_ttl`; the destructive GC path must
/// bypass that shortcut and use the manifest it fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_reads_retention_settings_past_the_freshness_ttl() -> anyhow::Result<()> {
    use prost::Message;
    use std::sync::Arc;
    use walgit_proto::keys;
    use walgit_proto::v1::{Checkpoint, CheckpointRef, LogSegmentRef, RefSnapshot};
    use walgit_server::maintain::{Unit, next_unit, run_pass};
    use walgit_store::{ObjectStore, ObjectStoreExt, PutMode};

    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.bundles.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
            c.maintenance.gc_interval = std::time::Duration::from_secs(3600);
            c.maintenance.retention_wal = std::time::Duration::from_secs(3600);
            c.wal.freshness_ttl = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let id = walgit_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;

    let live_seq = 20u64;
    let mut old = walgit_proto::time::now();
    old.seconds -= 8 * 24 * 3600;
    let object_format = h.manifest().object_format.clone();
    let refs = RefSnapshot {
        seq: live_seq,
        object_format: object_format.clone(),
        created_at: Some(old),
        ..Default::default()
    };
    step!(
        "live refs",
        h.store().put_bytes(
            &keys::checkpoint_refs_key(live_seq),
            refs.encode_to_vec(),
            PutMode::Create,
        )
    )?;
    let checkpoint = Checkpoint {
        seq: live_seq,
        object_format: object_format.clone(),
        refs_key: keys::checkpoint_refs_key(live_seq),
        created_at: Some(old),
        ..Default::default()
    };
    step!(
        "live checkpoint",
        h.store().put_bytes(
            &keys::checkpoint_key(live_seq),
            checkpoint.encode_to_vec(),
            PutMode::Create,
        )
    )?;

    {
        let (meta, bytes) =
            step!("manifest", h.store().get_bytes(keys::MANIFEST))?.expect("manifest");
        let mut m = walgit_proto::v1::Manifest::decode(bytes.as_ref())?;
        m.head_seq = live_seq;
        m.min_seq = 6;
        m.checkpoint = Some(CheckpointRef {
            seq: live_seq,
            key: keys::checkpoint_key(live_seq),
            created_at: Some(old),
            first_state_at: Some(old),
            as_of: Some(old),
        });
        m.log_segments = vec![LogSegmentRef {
            key: keys::log_segment_key(15),
            first_seq: 15,
            last_seq: 19,
            size: 0,
            sealed: true,
        }];
        m.packs.clear();
        m.revision += 1;
        step!(
            "rewrite manifest",
            h.store().put_bytes(
                keys::MANIFEST,
                m.encode_to_vec(),
                PutMode::Update(meta.version),
            )
        )?;
    }

    // A folded log/checkpoint pair that the host's 1h window would reclaim.
    step!(
        "old log",
        h.store()
            .put_bytes(&keys::log_segment_key(5), vec![0u8; 16], PutMode::Create)
    )?;
    step!(
        "live log",
        h.store()
            .put_bytes(&keys::log_segment_key(15), vec![0u8; 16], PutMode::Create)
    )?;
    for key in [
        keys::checkpoint_key(5),
        keys::checkpoint_refs_key(5),
    ] {
        step!(
            "old checkpoint",
            h.store().put_bytes(&key, vec![0u8; 16], PutMode::Create)
        )?;
    }

    // Load the rewritten manifest and record this instance's freshness TTL.
    step!("sync", h.sync())?;
    assert_eq!(
        h.manifest().checkpoint.as_ref().map(|c| c.seq),
        Some(live_seq)
    );

    // Another instance changes the repo to "keep forever" after the sync above.
    let mut cfg2 = (*server.state.cfg).clone();
    let cache2 = tempfile::tempdir()?;
    cfg2.cache.dir = cache2.path().to_path_buf();
    let registry2 =
        walgit_wal::Registry::new(server.state.store.clone(), Arc::new(cfg2));
    let h2 = step!("open second instance", registry2.open(&id))?;
    step!(
        "retention settings",
        h2.publish_settings(
            "[maintenance]\nretention_wal = \"0s\"\n",
            "tester",
            "keep WAL provenance forever",
        )
    )?;

    assert!(matches!(
        step!("plan", next_unit(&server.state, &id))?,
        Unit::Gc(_)
    ));
    step!("gc pass", run_pass(&server.state))?;

    assert!(
        h.store().head(&keys::log_segment_key(5)).await?.is_some(),
        "the fresh settings revision must be observed before deleting WAL objects"
    );
    assert!(
        h.store().head(&keys::checkpoint_key(5)).await?.is_some(),
        "retention_wal=0 must keep the old checkpoint"
    );
    assert!(
        h.store().head(&keys::log_segment_key(15)).await?.is_some(),
        "live log remains untouched"
    );
    Ok(())
}
