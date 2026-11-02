#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::dbg_macro
)]
//! The four remote-reader 503 budget paths (merge-base / diff / blame /
//! archive), each shrunk via a `WALGIT_TEST_*` knob (see
//! `objects.rs::test_budget`) so a tiny fixture can trip them. Runs as its own
//! test binary: the env knobs are process-scoped and no other test binary
//! reads them, so they cannot leak into `web_api`.

mod harness;

use harness::{Server, git_in};

type TestResult<T = ()> = anyhow::Result<T>;

/// A repo with two commits on `main` (f.txt changed) plus a diverged `topic`
/// branch, pushed to `o/r` on `server`.
fn fixture(server: &Server) -> TestResult {
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("a.txt"), "a\n")?;
    std::fs::write(dir.join("f.txt"), "one\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(&dir, &["commit", "-q", "-m", "one"])?;
    std::fs::write(dir.join("f.txt"), "two\n")?;
    git_in(&dir, &["commit", "-q", "-am", "two"])?;
    git_in(&dir, &["checkout", "-q", "-b", "topic"])?;
    std::fs::write(dir.join("f.txt"), "three\n")?;
    git_in(&dir, &["commit", "-q", "-am", "three"])?;
    git_in(&dir, &["checkout", "-q", "main"])?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r")],
    )?;
    Ok(())
}

/// Start a big server (packs local) + a 1-byte-cache sibling (remote reader),
/// so the request below runs the budgeted remote path.
async fn remote_sibling() -> TestResult<(Server, Server)> {
    let big = Server::start().await?;
    big.put_repo("o", "r").await?;
    fixture(&big)?;
    let small = big
        .start_sibling_with(|cfg| {
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await?;
    Ok((big, small))
}

async fn expect_503(small: &Server, path: &str) -> TestResult {
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", small.base_url))
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "GET {path} should be a budget 503"
    );
    assert!(!resp.text().await?.is_empty(), "503 has a message");
    Ok(())
}

#[allow(unsafe_code)] // SAFETY: the set_var below is the process-scoped knob this test controls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_base_budget_503() -> TestResult {
    // SAFETY: process-scoped test knob; budgets.rs runs as its own test binary
    // and no other test in it reads this variable.
    unsafe { std::env::set_var("WALGIT_TEST_MERGE_BASE_BUDGET", "1") };
    let (_big, small) = remote_sibling().await?;
    expect_503(&small, "/o/r/api-browser/merge-base?from=main&to=topic").await
}

#[allow(unsafe_code)] // SAFETY: the set_var below is the process-scoped knob this test controls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diff_budget_503() -> TestResult {
    // SAFETY: see merge_base_budget_503.
    unsafe { std::env::set_var("WALGIT_TEST_DIFF_BUDGET", "1") };
    let (_big, small) = remote_sibling().await?;
    expect_503(
        &small,
        "/o/r/api-browser/diff?from=main&to=topic&format=name-status",
    )
    .await
}

#[allow(unsafe_code)] // SAFETY: the set_var below is the process-scoped knob this test controls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blame_budget_503() -> TestResult {
    // SAFETY: see merge_base_budget_503.
    unsafe { std::env::set_var("WALGIT_TEST_BLAME_BUDGET", "1") };
    let (_big, small) = remote_sibling().await?;
    expect_503(&small, "/o/r/api-browser/blame/main/f.txt").await
}

#[allow(unsafe_code)] // SAFETY: the set_var below is the process-scoped knob this test controls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_budget_503() -> TestResult {
    // SAFETY: see merge_base_budget_503.
    unsafe { std::env::set_var("WALGIT_TEST_ARCHIVE_BUDGET", "1") };
    let (_big, small) = remote_sibling().await?;
    expect_503(&small, "/o/r/api-browser/archive/main").await
}
