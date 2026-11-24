#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::dbg_macro
)]
//! The collab fold as the maintainer's unit (D66): it must prune **exactly**
//! what its snapshot carries, leave the ledger an aggregation reads unchanged,
//! and refuse to sign as a principal the registry does not name.

mod harness;

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use harness::Server;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use walgit_store::fault::FaultStore;
use walgit_store::memory::MemoryStore;
use walgit_git::RepoId;
use walgit_wal::collab::{Entry, sign_entry};

fn seed(byte: u8) -> SigningKey {
    SigningKey::from_bytes(&[byte; 32])
}

fn public_key_b64(key: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
}

fn entry(key: &SigningKey, actor: &str, kind: &str, id: &str, body: Value) -> Entry {
    let mut entry = Entry {
        version: 1,
        kind: kind.into(),
        id: id.into(),
        actor: actor.into(),
        ts: 1_760_000_000,
        parent: String::new(),
        refs: None,
        body,
        sig: String::new(),
    };
    entry.sig = sign_entry(&mut entry, key);
    entry
}

async fn http(
    server: &Server,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let client = reqwest::Client::new();
    let url = format!("{}{path}", server.base_url);
    let mut req = match method {
        "PUT" => client.put(&url),
        "POST" => client.post(&url),
        _ => client.get(&url),
    }
    .header("Authorization", "Bearer worker-s3cret");
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.expect("request");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, value)
}

async fn collab_refs(server: &Server, repo: &str) -> Vec<String> {
    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!(
            "{}/{repo}/api-browser/refs/collab",
            server.base_url
        ))
        .header("Authorization", "Bearer worker-s3cret")
        .send()
        .await
        .expect("refs")
        .json()
        .await
        .expect("refs json");
    body["refs"]
        .as_array()
        .map(|refs| {
            refs.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fold_prunes_exactly_what_its_snapshot_carries() {
    let key = seed(7);
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: "worker-s3cret".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await
    .unwrap();

    // A repo whose only content is collab entries: a fold reads refs and blobs,
    // never pack data, so it must work on exactly this shape.
    let (status, _) = http(&server, "PUT", "/t/fold", None).await;
    assert!(status.is_success(), "{status}");
    let (status, _) = http(
        &server,
        "POST",
        "/t/fold/api-browser/collab/principal",
        Some(json!({"principal": "worker", "public_key": public_key_b64(&key)})),
    )
    .await;
    assert!(status.is_success(), "register principal: {status}");

    for (kind, id) in [("issue", "card-1"), ("comment", ""), ("comment", "")] {
        let e = entry(
            &key,
            "worker",
            kind,
            if id.is_empty() { "c" } else { id },
            json!({"body": format!("entry {kind}")}),
        );
        let (status, answer) = http(
            &server,
            "POST",
            "/t/fold/api-browser/collab/entries",
            Some(json!({"entry": e})),
        )
        .await;
        assert!(status.is_success(), "publish {kind}: {status} {answer}");
    }

    let before = collab_refs(&server, "t/fold").await;
    let inbox_before: Vec<String> = before
        .iter()
        .filter(|n| n.starts_with("refs/collab/inbox/"))
        .cloned()
        .collect();
    assert_eq!(inbox_before.len(), 3, "{before:?}");

    // The aggregation the fold must not change.
    let (_, board_before) = http(&server, "GET", "/t/fold/api-browser/collab/board", None).await;

    let id = RepoId::new("t", "fold").unwrap();
    let signer = walgit_server::collab_fold::Signer {
        principal: "worker".into(),
        key: key.clone(),
    };
    let log = |_: String| {};
    let outcome = walgit_server::collab_fold::fold(&server.state, &id, &signer, &log)
        .await
        .expect("fold");
    assert_eq!(outcome.folded, 3, "{outcome:?}");
    assert!(outcome.rebuilt);
    assert!(outcome.seq > 0);

    let after = collab_refs(&server, "t/fold").await;
    assert!(
        after
            .iter()
            .any(|n| n == walgit_server::collab_fold::SNAPSHOT_REF),
        "the fold must publish its snapshot: {after:?}"
    );
    for name in &inbox_before {
        assert!(
            !after.contains(name),
            "{name} was folded and must be pruned: {after:?}"
        );
    }

    // Reader equality: snapshot ∪ inbox aggregates to the same board.
    let (_, board_after) = http(&server, "GET", "/t/fold/api-browser/collab/board", None).await;
    assert_eq!(
        board_before["columns"], board_after["columns"],
        "a fold must not change what the board projects\nbefore: {board_before}\nafter: {board_after}"
    );

    // Idempotent: nothing left to fold, nothing published.
    let again = walgit_server::collab_fold::fold(&server.state, &id, &signer, &log)
        .await
        .expect("second fold");
    assert_eq!(again.folded, 0, "{again:?}");
    assert_eq!(again.seq, 0, "a no-op fold must not publish: {again:?}");
    assert!(
        again.snapshot.is_some(),
        "the repository still points at its snapshot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fold_refuses_a_key_the_registry_does_not_name() {
    let registered = seed(7);
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: "worker-s3cret".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await
    .unwrap();
    let (status, _) = http(&server, "PUT", "/t/refuse", None).await;
    assert!(status.is_success(), "{status}");
    let (status, _) = http(
        &server,
        "POST",
        "/t/refuse/api-browser/collab/principal",
        Some(json!({"principal": "worker", "public_key": public_key_b64(&registered)})),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = http(
        &server,
        "POST",
        "/t/refuse/api-browser/collab/entries",
        Some(json!({"entry": entry(&registered, "worker", "issue", "card-1", json!({}))})),
    )
    .await;
    assert!(status.is_success(), "{status}");

    let id = RepoId::new("t", "refuse").unwrap();
    let log = |_: String| {};
    // A different key under the same principal: the snapshot it signs would
    // never verify, so the fold refuses before publishing anything.
    let wrong = walgit_server::collab_fold::Signer {
        principal: "worker".into(),
        key: seed(9),
    };
    let err = walgit_server::collab_fold::fold(&server.state, &id, &wrong, &log)
        .await
        .expect_err("a key the registry does not name must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("not the key registered"), "{text}");
    assert!(
        !collab_refs(&server, "t/refuse")
            .await
            .iter()
            .any(|n| n == walgit_server::collab_fold::SNAPSHOT_REF),
        "a refused fold must not have published a snapshot"
    );

    // And an unregistered principal is named in the error, with the fix.
    let stranger = walgit_server::collab_fold::Signer {
        principal: "stranger".into(),
        key: seed(7),
    };
    let err = walgit_server::collab_fold::fold(&server.state, &id, &stranger, &log)
        .await
        .expect_err("an unregistered principal must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("is not registered"), "{text}");
    assert!(text.contains("principal-register"), "{text}");
}

/// C25: after a fold the parent lives **only** inside the snapshot; appending
/// to that chain must still pass the thin API's parent check (the check honours
/// the snapshot's validated records — the reason a refs-only check was refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folded_parent_stays_appendlable() {
    let key = seed(11);
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: "worker-s3cret".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await
    .unwrap();
    let (status, _) = http(&server, "PUT", "/t/c25fold", None).await;
    assert!(status.is_success(), "{status}");
    let (status, _) = http(
        &server,
        "POST",
        "/t/c25fold/api-browser/collab/principal",
        Some(json!({"principal": "worker", "public_key": public_key_b64(&key)})),
    )
    .await;
    assert!(status.is_success(), "register principal: {status}");

    let root = entry(&key, "worker", "issue", "c25", json!({"title": "root"}));
    let (status, answer) = http(
        &server,
        "POST",
        "/t/c25fold/api-browser/collab/entries",
        Some(json!({"entry": root})),
    )
    .await;
    assert!(status.is_success(), "publish root: {status} {answer}");
    let root_oid = answer["oid"].as_str().expect("root oid").to_string();

    let mut child = entry(&key, "worker", "comment", "c25", json!({"note": "child"}));
    child.parent = root_oid.clone();
    let (status, answer) = http(
        &server,
        "POST",
        "/t/c25fold/api-browser/collab/entries",
        Some(json!({"entry": child})),
    )
    .await;
    assert!(status.is_success(), "publish child: {status} {answer}");

    let id = RepoId::new("t", "c25fold").unwrap();
    let signer = walgit_server::collab_fold::Signer {
        principal: "worker".into(),
        key: key.clone(),
    };
    let log = |_: String| {};
    let outcome = walgit_server::collab_fold::fold(&server.state, &id, &signer, &log)
        .await
        .expect("fold");
    assert_eq!(outcome.folded, 2, "{outcome:?}");
    let after = collab_refs(&server, "t/c25fold").await;
    assert!(
        after.iter().any(|n| n == walgit_server::collab_fold::SNAPSHOT_REF),
        "{after:?}"
    );
    assert!(
        !after.iter().any(|n| n.starts_with("refs/collab/inbox/")),
        "both entries folded and pruned: {after:?}"
    );

    // The parent now exists only in the snapshot. A refs-only check would reject
    // this append; the snapshot-aware check must accept it.
    let mut grandchild = entry(&key, "worker", "comment", "c25", json!({"note": "after fold"}));
    grandchild.parent = root_oid;
    let (status, answer) = http(
        &server,
        "POST",
        "/t/c25fold/api-browser/collab/entries",
        Some(json!({"entry": grandchild})),
    )
    .await;
    assert!(
        status.is_success(),
        "a folded parent is still visible: {status} {answer}"
    );
}
/// POST one entry to the thin API on `repo` ("/t/<name>" or "/o/<name>").
async fn publish(server: &Server, repo: &str, entry: Entry) -> (reqwest::StatusCode, Value) {
    http(
        server,
        "POST",
        &format!("{repo}/api-browser/collab/entries"),
        Some(json!({"entry": entry})),
    )
    .await
}

/// C25 measurement, backing the `Collab publish, parent check` row of
/// `docs/ROUNDTRIPS.md`. It reads the same `FaultStore` counter the budget sim
/// uses (`healthy_request_round_trip_budgets`); the sim itself cannot reach
/// this path — it drives the WAL, not the HTTP thin API.
///
/// Returns `(tail hit, empty-parent control, snapshot miss)` store-request
/// deltas of three publishes on the same warm repo.
async fn measure_parent_check(
    server: &Server,
    link: &Arc<FaultStore>,
    key: &SigningKey,
    repo: &str,
) -> (u64, u64, u64) {
    let ops = || link.stats().ops.load(Ordering::Relaxed);
    let (status, _) = http(server, "PUT", repo, None).await;
    assert!(status.is_success(), "create {repo}: {status}");
    let (status, _) = http(
        server,
        "POST",
        &format!("{repo}/api-browser/collab/principal"),
        Some(json!({"principal": "worker", "public_key": public_key_b64(key)})),
    )
    .await;
    assert!(status.is_success(), "register principal: {status}");

    // Warm the publish path (repo creation, the first refs sync + publish).
    let root = entry(key, "worker", "issue", "c25rt", json!({"title": "root"}));
    let (status, answer) = publish(server, repo, root).await;
    assert!(status.is_success(), "publish root: {status} {answer}");
    let root_oid = answer["oid"].as_str().expect("root oid").to_string();
    let mut warm = entry(key, "worker", "comment", "c25rt", json!({"note": "warm"}));
    warm.parent = root_oid.clone();
    let (status, answer) = publish(server, repo, warm).await;
    assert!(status.is_success(), "publish warm child: {status} {answer}");

    // Tail hit (parent visible in the inbox) vs a root publish (no parent to
    // check at all): the difference is what the check itself costs.
    let mut tail = entry(key, "worker", "comment", "c25rt", json!({"note": "tail"}));
    tail.parent = root_oid.clone();
    let before = ops();
    let (status, answer) = publish(server, repo, tail).await;
    assert!(status.is_success(), "publish tail child: {status} {answer}");
    let d_tail = ops() - before;

    let root2 = entry(key, "worker", "issue", "c25rt2", json!({"title": "control"}));
    let before = ops();
    let (status, answer) = publish(server, repo, root2).await;
    assert!(status.is_success(), "publish control root: {status} {answer}");
    let d_root = ops() - before;

    // Fold: `root_oid` now lives only inside the snapshot. Appending to it
    // exercises the snapshot branch (`object_size` + fault + batched read).
    let (owner, name) = repo
        .trim_start_matches('/')
        .split_once('/')
        .expect("repo path is /owner/name");
    let id = RepoId::new(owner, name).unwrap();
    let signer = walgit_server::collab_fold::Signer {
        principal: "worker".into(),
        key: key.clone(),
    };
    let log = |_: String| {};
    let outcome = walgit_server::collab_fold::fold(&server.state, &id, &signer, &log)
        .await
        .expect("fold");
    assert!(outcome.folded >= 3, "{outcome:?}");
    let after = collab_refs(server, &repo[1..]).await;
    assert!(
        !after.iter().any(|n| n.starts_with("refs/collab/inbox/")),
        "the fold pruned the inbox: {after:?}"
    );

    let mut grandchild = entry(key, "worker", "comment", "c25rt", json!({"note": "after fold"}));
    grandchild.parent = root_oid;
    let before = ops();
    let (status, answer) = publish(server, repo, grandchild).await;
    assert!(
        status.is_success(),
        "a folded parent must stay appendlable: {status} {answer}"
    );
    let d_snap = ops() - before;
    (d_tail, d_root, d_snap)
}

/// Hard assertion: a parent visible in the inbox tail costs **no store request
/// beyond a root publish** — the check answers from the refs index already in
/// hand, so a tail hit is +0 by construction, not a tuned number.
///
/// The snapshot-miss delta is printed, not pinned: on this harness it is the
/// local path (`git cat-file -s` + one batched local read, no bucket request).
/// The remote-reader delta — the number `docs/ROUNDTRIPS.md` quotes — is the
/// second test below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_publish_parent_check_round_trips() {
    let key = seed(19);
    let (server, link) = Server::start_with_link(MemoryStore::shared(), |cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: "worker-s3cret".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await
    .unwrap();
    let (d_tail, d_root, d_snap) = measure_parent_check(&server, &link, &key, "/t/c25rt").await;
    eprintln!(
        "parent check store ops (local): tail={d_tail} root={d_root} snapshot={d_snap}"
    );
    assert_eq!(
        d_tail, d_root,
        "a tail parent must cost no request beyond a root publish (tail={d_tail}, root={d_root})"
    );
}

/// The remote-reader measurement behind the docs' "miss +2". The seeding host
/// keeps its packs local; the measured **sibling** has a fresh cache that can
/// never hold them, so its object reads go through the remote reader and the
/// snapshot folded by the seeding host is genuinely not local — the state a
/// real remote front is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collab_parent_check_remote_round_trips() {
    // A plain fn item (not a capturing closure): it is passed to the seeding
    // host and again to the sibling tweak, and fn items are Copy.
    fn auth(cfg: &mut walgit_config::Config) {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: "worker-s3cret".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    }
    let key = seed(23);
    let big = Server::start_with_tweak(auth).await.unwrap();
    let repo = "/t/c25rt";
    let (status, _) = http(&big, "PUT", repo, None).await;
    assert!(status.is_success(), "create {repo}: {status}");
    let (status, _) = http(
        &big,
        "POST",
        &format!("{repo}/api-browser/collab/principal"),
        Some(json!({"principal": "worker", "public_key": public_key_b64(&key)})),
    )
    .await;
    assert!(status.is_success(), "register principal: {status}");
    let root = entry(&key, "worker", "issue", "c25rt", json!({"title": "root"}));
    let (status, answer) = publish(&big, repo, root).await;
    assert!(status.is_success(), "publish root: {status} {answer}");
    let root_oid = answer["oid"].as_str().expect("root oid").to_string();
    let mut child = entry(&key, "worker", "comment", "c25rt", json!({"note": "child"}));
    child.parent = root_oid.clone();
    let (status, answer) = publish(&big, repo, child).await;
    assert!(status.is_success(), "publish child: {status} {answer}");
    let signer = walgit_server::collab_fold::Signer {
        principal: "worker".into(),
        key: key.clone(),
    };
    let log = |_: String| {};
    let id = RepoId::new("t", "c25rt").unwrap();
    let outcome = walgit_server::collab_fold::fold(&big.state, &id, &signer, &log)
        .await
        .expect("fold");
    assert!(outcome.folded >= 2, "{outcome:?}");

    let (small, link) = big
        .start_sibling_with_link(|cfg| {
            auth(cfg);
            // Never enough room for the packs: every object read is remote.
            cfg.cache.max_bytes = bytesize::ByteSize::b(1);
        })
        .await
        .unwrap();
    let ops = || link.stats().ops.load(Ordering::Relaxed);

    // Warm the sibling (cold open, first refs sync + publish).
    let warm_root = entry(&key, "worker", "issue", "c25rt", json!({"title": "warm"}));
    let (status, answer) = publish(&small, repo, warm_root).await;
    assert!(status.is_success(), "publish warm root: {status} {answer}");
    let warm_oid = answer["oid"].as_str().expect("warm oid").to_string();

    let mut tail = entry(&key, "worker", "comment", "c25rt", json!({"note": "tail"}));
    tail.parent = warm_oid;
    let before = ops();
    let (status, answer) = publish(&small, repo, tail).await;
    assert!(status.is_success(), "publish tail child: {status} {answer}");
    let d_tail = ops() - before;

    let control = entry(&key, "worker", "issue", "c25rt2", json!({"title": "control"}));
    let before = ops();
    let (status, answer) = publish(&small, repo, control).await;
    assert!(status.is_success(), "publish control root: {status} {answer}");
    let d_root = ops() - before;

    let mut grandchild = entry(&key, "worker", "comment", "c25rt", json!({"note": "after fold"}));
    grandchild.parent = root_oid;
    let before = ops();
    let (status, answer) = publish(&small, repo, grandchild).await;
    assert!(
        status.is_success(),
        "a folded parent must stay appendlable on a remote front: {status} {answer}"
    );
    let d_snap = ops() - before;
    eprintln!("parent check store ops (remote): tail={d_tail} root={d_root} snapshot={d_snap}");
    assert_eq!(
        d_tail, d_root,
        "a tail parent must cost no request beyond a root publish (tail={d_tail}, root={d_root})"
    );
    assert!(
        d_snap.saturating_sub(d_tail) >= 1,
        "the snapshot miss must pay at least one remote metadata round (tail={d_tail}, snapshot={d_snap})"
    );
}
