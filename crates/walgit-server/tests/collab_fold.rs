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
            "{}/{repo}/api/refs/collab",
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
        "/t/fold/api/collab/principal",
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
            "/t/fold/api/collab/entries",
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
    let (_, board_before) = http(&server, "GET", "/t/fold/api/collab/board", None).await;

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
    let (_, board_after) = http(&server, "GET", "/t/fold/api/collab/board", None).await;
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
        "/t/refuse/api/collab/principal",
        Some(json!({"principal": "worker", "public_key": public_key_b64(&registered)})),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = http(
        &server,
        "POST",
        "/t/refuse/api/collab/entries",
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
