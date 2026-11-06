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
//! The surface is read-only (D70): the D1 collaboration tools and resources
//! moved to `wgl mcp`, and this suite asserts they are gone rather than
//! silently answering through an old path.

mod harness;

use std::time::Duration;

use anyhow::Result;
use harness::{Server, TestRepo, git_in};
use reqwest::StatusCode;
use serde_json::{Value, json};
use walgit_store::fault::FaultPlan;
use walgit_store::memory::MemoryStore;

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

    // tools/list carries the host's read surface. Collaboration moved to
    // `wgl mcp` (D70), so none of those names may reappear here (D49: a call
    // cannot reach what `tools/list` hides, and the list must not advertise a
    // removed tool).
    let tools = call(&server, &sid, "tools/list", json!({})).await?;
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in ["repo_list", "repo_refs", "wal_ls"] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    for gone in ["collab_board", "collab_thread", "collab_report", "ci_status", "collab_publish", "collab_register"] {
        assert!(!names.contains(&gone), "removed tool still advertised: {gone}: {names:?}");
    }

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

    // The resource lane names the two remaining kinds and their grammar.
    let templates = call(&server, &sid, "resources/templates/list", json!({})).await?;
    let templates = templates["resourceTemplates"].as_array().expect("templates");
    assert_eq!(templates.len(), 2, "{templates:?}");
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
    assert!(!uris.iter().any(|u| u.starts_with("walgit://collab/")), "{uris:?}");

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

    // A URI that is not one of the two kinds is a request error, not a panic.
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
        // read per tick, the same lane `wgl collab watch` uses).
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
    // the native password dialog (docs/ARCHITECTURE.md §1.3).
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
    for name in &names {
        let arguments = match name.as_str() {
            "repo_list" => json!({}),
            "repo_owners" => json!({"owner": "t"}),
            "repo_refs" | "wal_ls" | "repo_overview" | "repo_tasks" | "repo_ops"
            | "repo_policy" | "repo_settings" => {
                json!({"repo": "t/all"})
            }

            "repo_resolve" => json!({"repo": "t/all", "rev": "main"}),
            "repo_tree" => json!({"repo": "t/all", "rev": &head}),
            // Same arguments as `repo_blame`: both take a revision and a path.
            "repo_blob" | "repo_blame" => {
                json!({"repo": "t/all", "rev": &head, "path": "f0_0.txt"})
            }
            "repo_commits" => json!({"repo": "t/all", "n": 5}),
            "repo_commit" => json!({"repo": "t/all", "sha": &head}),
            // Same arguments as `repo_merge_base`: two revisions.
            "repo_diff" | "repo_merge_base" => json!({"repo": "t/all", "from": &head, "to": &head}),
            other => anyhow::bail!("the tool table advertised `{other}` with no case in this test"),
        };
        let out = call(
            &server,
            &sid,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
        .await?;
        assert_ne!(out["isError"], true, "{name}: {out}");
        let text = out["content"][0]["text"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{name} answered with nothing: {out}");
        // Every read tool's payload is JSON the model can parse …
        serde_json::from_str::<Value>(text)
            .unwrap_or_else(|e| panic!("{name} did not answer with JSON: {e}: {text}"));
        // … and for the tools whose *shape* is the contract, from the pushed
        // content: a tool that answers `{}` or hits a neighbouring endpoint
        // would otherwise pass this sweep.
        match name.as_str() {
            "repo_refs" => assert!(text.contains(&head), "{name}: {text}"),
            "repo_tree" | "repo_blame" => {
                assert!(text.contains("f0_0.txt"), "{name} lost the pushed path: {text}");
            }
            "repo_commits" | "repo_commit" | "repo_resolve" => {
                assert!(text.contains(&head), "{name} is not about the pushed tip: {text}");
            }
            "repo_settings" => {
                // The public projection: `[maintenance]` with the host-only
                // keys stripped, never the fold identity's path.
                assert!(text.contains("[maintenance]"), "{name}: {text}");
                assert!(!text.contains("collab_fold_key"), "a host-only key leaked: {text}");
                assert!(!text.contains("collab_fold_principal"), "a host-only key leaked: {text}");
            }
            "wal_ls" => assert!(text.contains("\"entries\""), "{name}: {text}"),
            // These answer an object; the sweep above already proved it parses.
            "repo_overview" | "repo_ops" | "repo_tasks" | "repo_policy" => {
                assert!(text.contains('{'), "{name}: {text}");
            }
            _ => {}
        }
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

/// A body that is not a JSON-RPC message at all is a *client framing* bug, and
/// the transport must say so instead of answering 200 with nothing. rmcp
/// reports it as `415` with the deserializer's reason (the stdio lane's
/// `-32700` has no HTTP equivalent); what matters is that it is refused, never
/// silently accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_that_is_not_json_is_refused_with_a_reason() -> Result<()> {
    let server = Server::start().await?;
    let resp = reqwest::Client::new()
        .post(format!("{}/mcp", server.base_url))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body("{ this is not json")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let text = resp.text().await?;
    assert!(
        text.contains("deserialize") || text.contains("key must be"),
        "{text}"
    );
    Ok(())
}

/// A terminated session takes its pollers with it: they have nobody to notify,
/// and each one costs a bucket read per interval. Without the reap they also
/// keep counting against `mcp.max_subscriptions`, so a reconnecting client is
/// eventually refused forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminating_a_session_drops_its_subscriptions() -> Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.mcp.subscribe_interval = Duration::from_secs(1);
    })
    .await?;
    server.put_repo("t", "reap").await?;
    push_repo(&server, "t", "reap", 1)?;
    let sid = initialize(&server).await?;
    call(
        &server,
        &sid,
        "resources/subscribe",
        json!({"uri": "walgit://refs/t/reap"}),
    )
    .await?;
    assert_eq!(server.state.mcp.len(), 1, "the poller must be registered");
    // `DELETE /mcp` ends the session (204), and the registry goes with it.
    let resp = reqwest::Client::new()
        .delete(format!("{}/mcp", server.base_url))
        .header("Mcp-Session-Id", &sid)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        server.state.mcp.len(),
        0,
        "a terminated session leaves no poller behind"
    );
    Ok(())
}

/// The cap is per caller, not per session: one caller cannot buy more pollers by
/// opening more sessions, and a refusal is a *request* error the client can act
/// on (`-32602`), not a silently dropped subscription.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_subscription_cap_is_enforced_per_caller() -> Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.mcp.subscribe_interval = Duration::from_secs(1);
        cfg.mcp.max_subscriptions = 1;
    })
    .await?;
    server.put_repo("t", "cap").await?;
    push_repo(&server, "t", "cap", 1)?;
    let first = initialize(&server).await?;
    call(
        &server,
        &first,
        "resources/subscribe",
        json!({"uri": "walgit://refs/t/cap"}),
    )
    .await?;
    // A second session of the same caller is a *second* subscription, not the
    // idempotent path: its notifications travel down its own stream (the
    // next-door test proves both sessions get them), so it needs its own poller
    // — and the budget is per caller across its sessions, not per stream.
    let second = initialize(&server).await?;
    let (status, _, body) = post_as(
        &server,
        &json!({"jsonrpc": "2.0", "id": 9, "method": "resources/subscribe",
                "params": {"uri": "walgit://refs/t/cap"}}),
        Some(&second),
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], json!(-32602), "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("max_subscriptions"),
        "{body}"
    );
    // The same session asking again for the same URI *is* idempotent (this is
    // what a reconnecting client does), so it still succeeds under the cap.
    call(
        &server,
        &first,
        "resources/subscribe",
        json!({"uri": "walgit://refs/t/cap"}),
    )
    .await?;
    assert_eq!(server.state.mcp.len(), 1, "one URI, one session, one poller");
    Ok(())
}

/// `wal_ls`'s `to` is a bound, not a hint: a window below the page the host
/// loaded answers empty rather than handing back log lines the caller excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wal_ls_honours_an_explicit_to_bound() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "log").await?;
    push_repo(&server, "t", "log", 3)?;
    let sid = initialize(&server).await?;
    let all = call(&server, &sid, "tools/call", json!({"name": "wal_ls", "arguments": {"repo": "t/log"}})).await?;
    let text = all["content"][0]["text"].as_str().unwrap_or_default();
    let body: Value = serde_json::from_str(text)?;
    let head = body["head_seq"].as_u64().expect("head_seq");
    assert!(head >= 1, "{body}");
    // `to` far below the loaded window: nothing in range, and it says so.
    let narrow = call(
        &server,
        &sid,
        "tools/call",
        json!({"name": "wal_ls", "arguments": {"repo": "t/log", "to": 0}}),
    )
    .await?;
    let body: Value = serde_json::from_str(narrow["content"][0]["text"].as_str().unwrap_or_default())?;
    assert_eq!(body["entries"].as_array().map(Vec::len), Some(0), "{body}");
    assert_eq!(body["to"].as_u64(), Some(0), "{body}");
    Ok(())
}

/// A production report ("cards have no seq") traced back to this: once a
/// checkpoint folds the log, `manifest.log_segments` no longer names the folded
/// segments, and `wal_ls` answered `{entries: [], more: true}` in every window —
/// so the WAL looked unreadable even though the segment objects were still in
/// the bucket for `maintenance.retention_wal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wal_ls_reads_a_window_a_checkpoint_folded() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "folded").await?;
    push_repo(&server, "t", "folded", 3)?;

    // Fold the whole log, exactly as the maintainer's checkpoint unit does.
    let id = walgit_git::RepoId::new("t", "folded")?;
    let handle = server
        .state
        .registry
        .open(&id)
        .await
        .expect("repo handle");
    let first = handle
        .read_log(1, None)
        .await
        .expect("log before the fold");
    let head = handle.manifest().head_seq;
    let cp = handle.write_checkpoint().await?;
    assert_eq!(cp.seq, head, "the checkpoint folds the whole log");
    assert!(
        handle
            .manifest()
            .log_segments
            .iter()
            .all(|s| s.first_seq > head),
        "the fold trims the manifest's segment refs"
    );

    let sid = initialize(&server).await?;
    let page = call(
        &server,
        &sid,
        "tools/call",
        json!({"name": "wal_ls", "arguments": {"repo": "t/folded", "from": 1}}),
    )
    .await?;
    let body: Value = serde_json::from_str(page["content"][0]["text"].as_str().unwrap_or_default())?;
    assert_eq!(body["folded_through"].as_u64(), Some(head), "{body}");
    assert_eq!(
        body["entries"].as_array().map(Vec::len),
        Some(first.len()),
        "a folded window is still served: {body}"
    );
    // The window is reported as asked for, and `more` is about newer entries.
    assert_eq!(body["from"].as_u64(), Some(1), "{body}");
    assert_eq!(body["head_seq"].as_u64(), Some(head), "{body}");
    assert_eq!(body["more"], json!(false), "{body}");

    // The resource lane is the same window: one definition, two surfaces.
    let resource = call(
        &server,
        &sid,
        "resources/read",
        json!({"uri": "walgit://wal/t/folded?from=1"}),
    )
    .await?;
    let text = resource["contents"][0]["text"].as_str().unwrap_or_default();
    let body: Value = serde_json::from_str(text)?;
    assert_eq!(
        body["entries"].as_array().map(Vec::len),
        Some(first.len()),
        "the resource agrees with the tool: {body}"
    );
    Ok(())
}

/// A manifest read that fails as `Retryable` (the shape a transport error,
/// timeout, upstream 5xx or throttle takes).
fn manifest_outage() -> FaultPlan {
    FaultPlan {
        p_err_before: 1.0,
        only_keys: Some(vec!["manifest".into()]),
        ..Default::default()
    }
}

/// Incident cc-ai-store-503: with the bucket unreachable, the git smart HTTP
/// lane and the MCP read lane answered 500, which git clients treat as
/// permanent. A retryable store failure must be 503 + Retry-After on both, and
/// the MCP tool must carry the same classification (it forwards the host's own
/// router, D64). The warm handle (`sync_refs`) and the cold registry
/// (`registry.open`) are different code paths and both are covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_store_failures_are_503_on_git_and_mcp_reads() -> Result<()> {
    let mem = MemoryStore::shared();

    // Seed through a healthy link and keep this instance warm: its repo handle
    // is cached, so arming the link exercises the `sync_refs` lane.
    let (warm, warm_link) = Server::start_with_link(mem.clone(), |_| {}).await?;
    warm.put_repo("t", "outage").await?;
    push_repo(&warm, "t", "outage", 1)?;
    warm_link.set(manifest_outage());

    let url = format!(
        "{}/t/outage.git/info/refs?service=git-upload-pack",
        warm.base_url
    );
    let resp = reqwest::Client::new().get(&url).send().await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "warm git read"
    );
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("15"),
        "a 503 must say when to retry"
    );

    // A cold instance over the same bucket (fresh registry and cache): the
    // first read is `registry.open`, the path that swallowed the coordinated
    // store error into a 500 before this fix.
    let (cold, cold_link) = Server::start_with_link(mem, |_| {}).await?;
    cold_link.set(manifest_outage());

    let url = format!(
        "{}/t/outage.git/info/refs?service=git-upload-pack",
        cold.base_url
    );
    let resp = reqwest::Client::new().get(&url).send().await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "cold git read"
    );
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("15")
    );

    // The JSON API lane `/mcp` forwards: same status and header.
    let url = format!("{}/t/outage/api-browser/refs?kind=branches", cold.base_url);
    let resp = reqwest::Client::new().get(&url).send().await?;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "api-browser read"
    );
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("15")
    );

    // And the tool a model actually calls reports the same classification:
    // a tool that ran and failed is `isError` content (the spec's split, not a
    // protocol error), and the status travels appended to the sentence —
    // `service unavailable: … (503)` is what tells the model a retry is
    // worthwhile.
    let sid = initialize(&cold).await?;
    let (status, _, body) = post(
        &cold,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "repo_refs", "arguments": {"repo": "t/outage", "kind": "branches"}},
        }),
        Some(&sid),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("service unavailable"), "{body}");
    assert!(text.contains("503"), "the status must travel with it: {body}");
    Ok(())
}
