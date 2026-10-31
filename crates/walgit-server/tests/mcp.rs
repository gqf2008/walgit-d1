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
//! `/mcp` — the MCP Streamable-HTTP endpoint served by the host itself (D64).
//!
//! What is asserted here is the *transport and the wiring*, not the shape of a
//! repository: a client can `initialize`, list tools/resources/templates, read a
//! pushed repository through the tools and through a resource, hold one SSE
//! stream, be notified when a subscription's version moves, and end the session.
//! The write tools are exercised for the property that matters most — a key that
//! is not the caller's actor's key is refused *before* anything is published,
//! because the collab log is append-only — plus one real signed round trip that
//! lands in the thread a client then reads back as a resource.

mod harness;

use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use harness::{Server, TestRepo, git_in};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// One `POST /mcp`, returning (status, session header, body as JSON when it
/// parses).
async fn post(
    server: &Server,
    body: &Value,
    session: Option<&str>,
) -> Result<(StatusCode, Option<String>, Value)> {
    post_as(server, body, session, None).await
}

/// `post` with a bearer — the shape a `token`-mode host needs.
async fn post_as(
    server: &Server,
    body: &Value,
    session: Option<&str>,
    bearer: Option<&str>,
) -> Result<(StatusCode, Option<String>, Value)> {
    let mut req = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("Content-Type", "application/json")
        // MCP's Streamable HTTP requires the client to say it accepts both the
        // JSON answer and the SSE lane (`406` otherwise).
        .header("Accept", "application/json, text/event-stream");
    if let Some(s) = session {
        req = req.header("Mcp-Session-Id", s);
    }
    if let Some(b) = bearer {
        req = req.header("Authorization", format!("Bearer {b}"));
    }
    let resp = req.body(body.to_string()).send().await?;
    let status = resp.status();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let text = resp.text().await?;
    // Streamable HTTP lets a server answer a POST with a JSON body or with one
    // `text/event-stream` frame; rmcp answers the session lane with a frame, so
    // this helper accepts both and returns the message inside.
    let value = if content_type.contains("text/event-stream") {
        sse_payload(&text)
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    Ok((status, sid, value))
}

/// The last JSON-RPC message in an SSE body (`data: {...}` lines).
fn sse_payload(text: &str) -> Value {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .rfind(|v| v.get("result").is_some() || v.get("error").is_some())
        .unwrap_or(Value::Null)
}

/// One request, asserting a successful JSON-RPC result, returning it.
async fn call(server: &Server, session: &str, method: &str, params: Value) -> Result<Value> {
    call_as(server, session, method, params, None).await
}

async fn call_as(
    server: &Server,
    session: &str,
    method: &str,
    params: Value,
    bearer: Option<&str>,
) -> Result<Value> {
    let (status, _, body) = post_as(
        server,
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
        Some(session),
        bearer,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{method}: {body}");
    assert!(
        body.get("error").is_none(),
        "{method} failed: {}",
        body["error"]
    );
    Ok(body["result"].clone())
}

async fn initialize(server: &Server) -> Result<String> {
    initialize_as(server, None).await
}

async fn initialize_as(server: &Server, bearer: Option<&str>) -> Result<String> {
    let (status, sid, body) = post_as(
        server,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1.0.0"}},
        }),
        None,
        bearer,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["serverInfo"]["name"], "walgit");
    let sid = sid.expect("initialize must mint an Mcp-Session-Id");
    Ok(sid)
}

/// Push one synthetic repository `1 commit / 1 file` and return its source dir.
fn push_repo(server: &Server, owner: &str, repo: &str, commits: usize) -> Result<TestRepo> {
    let src = TestRepo::synthetic(commits, 1)?;
    git_in(&src, &["remote", "add", "origin", &server.repo_url(owner, repo)])?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    Ok(src)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_mints_a_session_and_advertises_what_it_serves() -> Result<()> {
    let server = Server::start().await?;
    let (status, sid, body) = post(
        &server,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1.0.0"}},
        }),
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    let sid = sid.expect("session id");
    // Subscriptions are advertised, because the endpoint really does push
    // `notifications/resources/updated` on its SSE stream.
    assert_eq!(body["result"]["capabilities"]["resources"]["subscribe"], true);
    assert_eq!(body["result"]["capabilities"]["resources"]["listChanged"], true);
    assert!(body["result"]["instructions"].is_string(), "{body}");
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");

    // The same session answers `ping` (a liveness check a host sends early).
    let pong = call(&server, &sid, "ping", json!({})).await?;
    assert_eq!(pong, json!({}));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_json_rpc_frame_is_held_to_the_contract() -> Result<()> {
    let server = Server::start().await?;
    let sid = initialize(&server).await?;
    let url = format!("{}/mcp", server.base_url);

    // The transport's own contract (rmcp's, [D64]): a client that does not
    // speak Streamable HTTP is told so on the HTTP line, never with a JSON-RPC
    // error it would have to parse out of a stream.
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &sid)
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE, "no Accept header");

    let resp = reqwest::Client::new()
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &sid)
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "no Content-Type");

    // Bytes that are not a JSON-RPC message at all: also the transport's job.
    let (status, _, _) = post(&server, &json!({"jsonrpc": "1.0", "id": 9, "method": "ping"}), Some(&sid)).await?;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // An unknown method is -32601 inside the protocol.
    let (status, _, body) = post(
        &server,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/nope"}),
        Some(&sid),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32601, "{body}");

    // A request the model can fix is -32602, and every tool schema is strict:
    // `bogus` is not an argument of `repo_refs` (D49: the surface a client can
    // reach is exactly the surface it was told about).
    let (_, _, body) = post(
        &server,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "repo_refs", "arguments": {"repo": "t/x", "bogus": 1}}}),
        Some(&sid),
    )
    .await?;
    assert_eq!(body["error"]["code"], -32602, "{body}");

    // A body with nothing to answer (a notification) is 202 — that is how a
    // client knows it landed.
    let (status, _, _) = post(
        &server,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        Some(&sid),
    )
    .await?;
    assert_eq!(status, StatusCode::ACCEPTED);

    // An unknown session is a real 404 — not a silently minted one, which would
    // make a client's subscriptions vanish without a word.
    let (status, _, _) = post(
        &server,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}),
        Some("not-a-session"),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_reads_a_pushed_repository_through_tools_and_resources() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "mcp").await?;
    let src = push_repo(&server, "t", "mcp", 2)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let sid = initialize(&server).await?;

    // tools/list carries the read surface and the one pass-through write tool
    // tool at all (D49: `tools/call` cannot reach what `tools/list` hides).
    let tools = call(&server, &sid, "tools/list", json!({})).await?;
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["repo_list", "repo_refs", "collab_board", "wal_ls"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    assert!(names.contains(&"collab_publish"), "{names:?}");

    // The repositories this host serves.
    let listed = call(&server, &sid, "tools/call", json!({"name": "repo_list", "arguments": {}})).await?;
    let text = listed["content"][0]["text"].as_str().expect("text content");
    assert!(text.contains("mcp"), "repo_list: {text}");

    // A read tool resolves through the host's own JSON API.
    let refs = call(
        &server,
        &sid,
        "tools/call",
        json!({"name": "repo_refs", "arguments": {"repo": "t/mcp", "kind": "branches"}}),
    )
    .await?;
    let text = refs["content"][0]["text"].as_str().expect("text content");
    assert!(text.contains(&head), "repo_refs: {text}");

    // The resource lane names the four kinds and their grammar.
    let templates = call(&server, &sid, "resources/templates/list", json!({})).await?;
    let templates = templates["resourceTemplates"].as_array().expect("templates");
    assert_eq!(templates.len(), 4, "{templates:?}");
    assert_eq!(templates[0]["uriTemplate"], "walgit://refs/{owner}/{repo}");

    // `resources/list` names the pushed repository; `resources/read` reads it.
    let list = call(&server, &sid, "resources/list", json!({})).await?;
    let uris: Vec<&str> = list["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .collect();
    assert!(uris.contains(&"walgit://refs/t/mcp"), "{uris:?}");
    assert!(uris.contains(&"walgit://collab/board/t/mcp"), "{uris:?}");

    let read = call(
        &server,
        &sid,
        "resources/read",
        json!({"uri": "walgit://refs/t/mcp"}),
    )
    .await?;
    let body: Value = serde_json::from_str(
        read["contents"][0]["text"].as_str().expect("contents text"),
    )?;
    assert_eq!(body["head"]["sha"], head, "{body}");
    assert_eq!(read["contents"][0]["mimeType"], "application/json");

    // The WAL resource is the retained log, newest page by default.
    let wal = call(
        &server,
        &sid,
        "resources/read",
        json!({"uri": "walgit://wal/t/mcp"}),
    )
    .await?;
    let wal: Value =
        serde_json::from_str(wal["contents"][0]["text"].as_str().expect("contents"))?;
    assert!(wal["head_seq"].as_u64().unwrap_or(0) >= 1, "{wal}");

    // A URI that is not one of the four kinds is a request error, not a panic.
    let (_, _, body) = post(
        &server,
        &json!({"jsonrpc": "2.0", "id": 6, "method": "resources/read", "params": {"uri": "http://example.com/x"}}),
        Some(&sid),
    )
    .await?;
    assert_eq!(body["error"]["code"], -32602, "{body}");
    Ok(())
}

/// Read `frames` SSE events, returning true when `notifications/resources/updated`
/// arrived within `budget`.
async fn wait_for_updated(url: &str, sid: &str, budget: Duration) -> bool {
    let Ok(resp) = reqwest::Client::new()
        .get(url)
        .header("Mcp-Session-Id", sid)
        .header("Accept", "text/event-stream")
        .send()
        .await
    else {
        return false;
    };
    let mut resp = resp;
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if buf.contains("notifications/resources/updated") {
                    return true;
                }
            }
            _ => return false,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_pushes_a_notification_when_the_refs_move() -> Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        // The floor a subscription may poll at (D55: one conditional manifest
        // read per tick, the same lane `walgit collab watch` uses).
        cfg.mcp.subscribe_interval = Duration::from_secs(1);
    })
    .await?;
    server.put_repo("t", "sub").await?;
    let src = push_repo(&server, "t", "sub", 1)?;
    let sid = initialize(&server).await?;

    // The stream belongs to a session; without one there is nothing to attach to.
    let no_session = reqwest::Client::new()
        .get(format!("{}/mcp", server.base_url))
        .header("Accept", "text/event-stream")
        .send()
        .await?;
    assert_eq!(no_session.status(), StatusCode::BAD_REQUEST);
    let unknown = reqwest::Client::new()
        .get(format!("{}/mcp", server.base_url))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", "not-a-session")
        .send()
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let uri = "walgit://refs/t/sub";
    // `resources/subscribe` answers an empty result ([D64]): the notification is
    // the whole answer, and the first version is not part of the protocol.
    let _ = call(&server, &sid, "resources/subscribe", json!({"uri": uri})).await?;
    // Idempotent: subscribing again is not a second poller, and it must not fail.
    let _ = call(&server, &sid, "resources/subscribe", json!({"uri": uri})).await?;

    let url = format!("{}/mcp", server.base_url);
    let reader_sid = sid.clone();
    let reader =
        tokio::spawn(async move { wait_for_updated(&url, &reader_sid, Duration::from_secs(30)).await });

    // Move the refs. Everything except the push itself is a re-read: the
    // notification says "look again", it never carries the delta.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(src.join("f0_0.txt"), b"second\n")?;
    git_in(&src, &["add", "-A"])?;
    git_in(&src, &["-c", "commit.gpgsign=false", "commit", "-qm", "second"])?;
    git_in(&src, &["push", "-q", "origin", "main"])?;

    let notified = tokio::time::timeout(Duration::from_secs(40), reader).await??;
    assert!(notified, "no notifications/resources/updated after a push");

    // Unsubscribe stops the poller, and DELETE ends the session with it.
    call(&server, &sid, "resources/unsubscribe", json!({"uri": uri})).await?;
    let deleted = reqwest::Client::new()
        .delete(format!("{}/mcp", server.base_url))
        .header("Mcp-Session-Id", &sid)
        .send()
        .await?;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let (status, _, _) = post(
        &server,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        Some(&sid),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_mode_answers_a_bearer_only_challenge() -> Result<()> {
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
    .await?;

    let resp = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("Content-Type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // Bearer only: `/mcp` is browser-reachable, and a Basic challenge there pops
    // the native password dialog (AGENTS.md §1.3).
    assert!(challenge.contains("Bearer"), "{challenge}");
    assert!(!challenge.contains("Basic"), "{challenge}");

    let resp = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("Authorization", "Bearer worker-s3cret")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1.0.0"}}})
            .to_string(),
        )
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // A foreign `Origin` cannot drive `/mcp` with the user's cookie.
    let resp = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("Authorization", "Bearer worker-s3cret")
        .header("Origin", "https://evil.example")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

// ------------------------------------------------------- the write surface ---

/// Register `principal` → `public_key` in the repository's collab registry
/// (`refs/collab/meta/principals/<p>`, a JSON blob — `docs/D1_PROTOCOL.md`
/// §4.3), exactly as `walgit collab join` does, and push it.
fn register_principal(repo: &std::path::Path, principal: &str, public_key: &str) -> Result<()> {
    let blob = serde_json::to_string_pretty(&json!({
        "version": 1,
        "principal": principal,
        "public_key": public_key,
        "registered_at": 1_700_000_000,
    }))?;
    let path = repo.join(format!("registration-{principal}.json"));
    std::fs::write(&path, blob)?;
    let oid = git_in(repo, &["hash-object", "-w", path.to_str().unwrap()])?
        .trim()
        .to_string();
    let ref_name = format!("refs/collab/meta/principals/{principal}");
    git_in(repo, &["update-ref", &ref_name, &oid])?;
    let refspec = format!("+{ref_name}:{ref_name}");
    git_in(repo, &["push", "-q", "origin", &refspec])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_is_an_entry_the_client_signed_and_the_host_only_forwards() -> Result<()> {
    // `token` mode, because a write is authorized as *the caller*: with
    // `mode = none` every request is `anon` and the API refuses any entry whose
    // actor is not the caller. The host holds no key: the entry arrives signed.
    let token = "worker-s3cret";
    let server = Server::start_with_tweak(move |cfg| {
        cfg.server.auth.mode = walgit_config::AuthMode::Token;
        cfg.server.auth.anonymous_read = false;
        cfg.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "worker".into(),
            token: token.into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await?;

    // Create + push as `worker` (the URL userinfo is how git answers the 401
    // challenge: §1.3 / issue #79).
    let put = reqwest::Client::new()
        .put(format!("{}/t/write", server.base_url))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?;
    assert!(put.status().is_success(), "{}", put.status());
    let url = server.repo_url_with_userinfo("t", "write", "worker", token);
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["remote", "add", "origin", &url])?;
    git_in(&src, &["push", "-q", "origin", "main"])?;

    // The identity is the client's all the way down: the test is the signer.
    let seed = [7u8; 32];
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
    register_principal(&src, "worker", &public_key)?;

    let sid = initialize_as(&server, Some(token)).await?;
    let bearer = Some(token);

    let tools = call_as(&server, &sid, "tools/list", json!({}), bearer).await?;
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    // One write tool, always advertised: whether it may write is the API's
    // answer (403 for a caller without write), not a property of the table.
    assert!(names.contains(&"collab_publish"), "{names:?}");
    assert!(!names.contains(&"collab_entry"), "{names:?}");

    // A real signed entry lands in the thread …
    let issue = signed_entry(&key, "issue", "card-1", "worker", json!({
        "title": "first card",
        "acceptance": "it reads back",
    }));
    let written = call_as(
        &server,
        &sid,
        "tools/call",
        json!({"name": "collab_publish", "arguments": {"repo": "t/write", "entry": &issue}}),
        bearer,
    )
    .await?;
    assert_ne!(written["isError"], true, "{written}");
    let text = written["content"][0]["text"].as_str().expect("text");
    // The answer is the API's own (`{oid, ref, seq}`), unchanged.
    let published: Value = serde_json::from_str(text)?;
    assert!(published["oid"].is_string(), "{text}");
    assert!(
        published["ref"]
            .as_str()
            .unwrap_or_default()
            .starts_with("refs/collab/inbox/worker/"),
        "{text}"
    );

    // … and is what a resource read returns, verified.
    let read = call_as(
        &server,
        &sid,
        "resources/read",
        json!({"uri": "walgit://collab/thread/t/write/card-1"}),
        bearer,
    )
    .await?;
    let thread: Value =
        serde_json::from_str(read["contents"][0]["text"].as_str().expect("contents"))?;
    let entries = thread["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "{thread}");
    assert_eq!(entries[0]["entry"]["kind"], "issue");
    assert_eq!(entries[0]["principal"], "worker");
    assert_eq!(entries[0]["verified"], true, "{thread}");
    let head_oid = entries[0]["oid"].as_str().expect("oid").to_string();

    // A claim is just another signed entry (the JSON-text form of `entry`),
    // parented on the thread head.
    let mut claim = signed_entry(&key, "card_claim", "card-1", "worker", json!({
        "ttl": 300,
        "worktree": "wt-1",
        "branch": "feat/card-1",
        "work": "read the tests",
    }));
    claim["parent"] = json!(head_oid);
    let claimed = call_as(
        &server,
        &sid,
        "tools/call",
        json!({"name": "collab_publish", "arguments": {
            "repo": "t/write",
            "entry": serde_json::to_string(&claim)?,
        }}),
        bearer,
    )
    .await?;
    assert_ne!(claimed["isError"], true, "{claimed}");

    // Writing as somebody else is refused by the host before anything is
    // appended: an entry whose actor is not the caller has no author.
    let forged = signed_entry(&key, "comment", "card-1", "someone-else", json!({"body": "hi"}));
    let refused = call_as(
        &server,
        &sid,
        "tools/call",
        json!({"name": "collab_publish", "arguments": {"repo": "t/write", "entry": &forged}}),
        bearer,
    )
    .await?;
    assert_eq!(refused["isError"], true, "{refused}");
    Ok(())
}

/// A signed collab entry, the shape `walgit collab entry` produces: the client
/// owns the key, the canonical bytes and the signature.
fn signed_entry(
    key: &ed25519_dalek::SigningKey,
    kind: &str,
    id: &str,
    actor: &str,
    body: Value,
) -> Value {
    use walgit_wal::collab::{sign_entry, Entry};
    let mut entry = Entry {
        version: 1,
        kind: kind.to_string(),
        id: id.to_string(),
        actor: actor.to_string(),
        ts: 1_760_000_000,
        parent: String::new(),
        refs: None,
        body,
        sig: String::new(),
    };
    entry.sig = sign_entry(&mut entry, key);
    serde_json::to_value(entry).expect("entry")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_that_does_not_exist_is_a_tool_error_not_a_crash() -> Result<()> {
    let server = Server::start().await?;
    let sid = initialize(&server).await?;
    let out = call(
        &server,
        &sid,
        "tools/call",
        json!({"name": "repo_refs", "arguments": {"repo": "nobody/nothing"}}),
    )
    .await?;
    assert_eq!(out["isError"], true, "{out}");
    let text = out["content"][0]["text"].as_str().unwrap_or_default();
    assert!(!text.is_empty(), "{out}");
    Ok(())
}

/// Every advertised read tool must actually answer for a real repository: the
/// tool table is the surface an agent trusts, and a name that only exists in
/// `tools/list` is a lie the suite would not otherwise catch (D49's rule — a
/// call cannot reach what the list hides, and the list must not hide a name that
/// has no implementation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_advertised_read_tool_answers_for_a_pushed_repository() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "all").await?;
    let src = push_repo(&server, "t", "all", 2)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let sid = initialize(&server).await?;

    let tools = call(&server, &sid, "tools/list", json!({})).await?;
    let names: Vec<String> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    // `collab_publish` needs a signed entry, so this read sweep skips it.
    assert!(names.iter().any(|n| n == "collab_publish"), "{names:?}");

    for name in &names {
        let arguments = match name.as_str() {
            // The write tool needs a signed entry; it has its own test.
            "collab_publish" => continue,
            "repo_list" => json!({}),
            "repo_owners" => json!({"owner": "t"}),
            "repo_refs" | "collab_board" | "collab_report" | "wal_ls" => json!({"repo": "t/all"}),
            "repo_resolve" => json!({"repo": "t/all", "rev": "main"}),
            "repo_tree" => json!({"repo": "t/all", "rev": &head}),
            "repo_blob" => json!({"repo": "t/all", "rev": &head, "path": "f0_0.txt"}),
            "repo_commits" => json!({"repo": "t/all", "n": 5}),
            "repo_diff" => json!({"repo": "t/all", "from": &head, "to": &head}),
            "collab_thread" => json!({"repo": "t/all", "id": "no-such-thread"}),
            other => anyhow::bail!("the tool table advertised `{other}` with no case in this test"),
        };
        let out = call(
            &server,
            &sid,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
        .await?;
        // A missing thread is a tool *error* by design; everything else answers.
        if name == "collab_thread" {
            continue;
        }
        assert_ne!(out["isError"], true, "{name}: {out}");
        let text = out["content"][0]["text"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{name} answered with nothing: {out}");
        // Every read tool's payload is JSON the model can parse.
        serde_json::from_str::<Value>(text)
            .unwrap_or_else(|e| panic!("{name} did not answer with JSON: {e}: {text}"));
    }
    Ok(())
}

/// Two sessions of the same principal may subscribe to the same URI: the
/// registry is keyed by (principal, session, uri), so the second one is not
/// silently swallowed by the first — a client that reconnected would otherwise
/// get a success and no notifications forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_of_one_caller_can_watch_the_same_resource() -> Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.mcp.subscribe_interval = Duration::from_secs(1);
    })
    .await?;
    server.put_repo("t", "twice").await?;
    let src = push_repo(&server, "t", "twice", 1)?;
    let one = initialize(&server).await?;
    let two = initialize(&server).await?;
    let uri = "walgit://refs/t/twice";
    for sid in [&one, &two] {
        call(
            &server,
            sid,
            "resources/subscribe",
            json!({"uri": uri}),
        )
        .await?;
    }
    // Both streams are held open before the refs move, so neither can be a
    // notification that only the first subscriber received.
    let url = format!("{}/mcp", server.base_url);
    let mut readers = Vec::new();
    for sid in [one.clone(), two.clone()] {
        let url = url.clone();
        readers.push(tokio::spawn(async move {
            wait_for_updated(&url, &sid, Duration::from_secs(20)).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(src.join("f0_0.txt"), b"second\n")?;
    git_in(&src, &["add", "-A"])?;
    git_in(&src, &["-c", "commit.gpgsign=false", "commit", "-qm", "second"])?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    for (i, reader) in readers.into_iter().enumerate() {
        let seen = reader.await?;
        assert!(seen, "session {i} was not notified");
    }
    Ok(())
}
