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
//! `/api/v1` (D20): the versioned programmatic surface, its browser-lane alias
//! (`/api-browser`), CORS for foreign origins, discovery, `me`, repo summary and
//! admin, and the SDK artefact route.

mod harness;

use harness::{Server, git_in};
use serde_json::Value;

type TestResult = anyhow::Result<()>;

async fn req(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    extra: &[(&str, &str)],
) -> anyhow::Result<(reqwest::StatusCode, String, reqwest::header::HeaderMap)> {
    let mut r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .request(method, format!("{}{path}", server.base_url))
        .header("Accept", "application/json");
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let resp = r.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    Ok((status, resp.text().await?, headers))
}
fn hdr(h: &reqwest::header::HeaderMap, k: &str) -> String {
    h.get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}
async fn json(server: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, text, _) = req(server, reqwest::Method::GET, path, &[]).await?;
    anyhow::ensure!(st.is_success(), "GET {path} -> {st}: {text}");
    Ok(serde_json::from_str(&text)?)
}

fn fixture(server: &Server) -> anyhow::Result<String> {
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("README.md"), "# v1\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(
        &dir,
        &[
            "commit",
            "-q",
            "-m",
            "initial\n\nSee https://github.com/o/r/pull/7 for context.\n\nMerge-Queue-Phase: target-publish\nMerge-Queue-Pull-Request: 7\nCo-authored-by: Jane <jane@example.com>",
        ],
    )?;
    git_in(
        &dir,
        &[
            "-c",
            "tag.forceSignAnnotated=false",
            "-c",
            "tag.gpgsign=false",
            "tag",
            "v1",
        ],
    )?;
    git_in(&dir, &["branch", "feature/x"])?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r")],
    )?;
    Ok(git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v1_surface_and_browser_lane() -> TestResult {
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.cors_origins = vec!["https://*.docs.example.com".into()];
    })
    .await?;
    server.put_repo("o", "r").await?;
    let head = fixture(&server)?;

    // discovery
    let disco = json(&server, "/api/v1").await?;
    assert_eq!(disco["version"], 1);
    assert!(disco["sdk"].as_str().unwrap().ends_with("/repos.js"));
    assert!(
        disco["browser_base"]
            .as_str()
            .unwrap()
            .ends_with("/api-browser/v1")
    );
    assert!(
        disco["auth"]["authenticate"]
            .as_str()
            .unwrap()
            .ends_with("/api-browser/v1/authenticate")
    );

    // me (auth mode none in tests → anonymous principal)
    let (st, text, hdrs) = req(&server, reqwest::Method::GET, "/api/v1/me", &[]).await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&hdrs, "cache-control"), "no-store");
    // #127: `admin` travels on `me` (the SPA's 存储配置 entry keys off it);
    // mode `none` on loopback is admin (§1.3).
    let me: Value = serde_json::from_str(&text)?;
    assert_eq!(me["admin"], true, "{me}");
    assert_eq!(me["write"], true, "{me}");

    // owners
    assert_eq!(
        json(&server, "/api/v1/owners").await?,
        serde_json::json!(["o"])
    );
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["r"])
    );
    assert_eq!(
        json(&server, "/api/v1/owners/nobody/repos").await?,
        serde_json::json!([])
    );

    // repo summary: SWR + ETag on head
    let (st, text, hdrs) = req(&server, reqwest::Method::GET, "/o/r/api", &[]).await?;
    assert_eq!(st, 200, "{text}");
    let ov: Value = serde_json::from_str(&text)?;
    assert_eq!(ov["full_name"], "o/r");
    assert_eq!(ov["head"]["name"], "main");
    assert_eq!(ov["head"]["sha"], head);
    assert_eq!(ov["branches"], 2);
    assert_eq!(ov["tags"], 1);
    assert!(ov["clone_url"].as_str().unwrap().ends_with("/o/r.git"));
    assert!(ov["api_url"].as_str().unwrap().ends_with("/o/r/api"));
    assert_eq!(hdr(&hdrs, "etag"), format!("\"{head}\""));
    assert!(hdr(&hdrs, "cache-control").contains("stale-while-revalidate"));
    let (st, _, _) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api",
        &[("If-None-Match", &format!("\"{head}\""))],
    )
    .await?;
    assert_eq!(st, 304);
    assert_eq!(
        req(&server, reqwest::Method::GET, "/o/nope/api", &[])
            .await?
            .0,
        404
    );

    // the repo-scoped read endpoints are the same handlers as /{o}/{r}/api/…
    let refs = json(&server, "/o/r/api/refs").await?;
    assert_eq!(refs["head"]["sha"], head);
    let res = json(&server, "/o/r/api/resolve/feature/x").await?;
    assert_eq!(res["kind"], "branch");
    let tree = json(&server, &format!("/o/r/api/tree/{head}")).await?;
    assert_eq!(tree["entries"][0]["name"], "README.md");
    let blob = json(&server, &format!("/o/r/api/blob/{head}/README.md")).await?;
    assert_eq!(blob["contents"], "# v1\n");
    let clist = json(&server, &format!("/o/r/api/commits?ref={head}")).await?;
    assert_eq!(clist["commits"][0]["subject"], "initial");
    let cmt = json(&server, &format!("/o/r/api/commit/{head}")).await?;
    assert_eq!(cmt["commit"]["sha"], head);
    // Trailers split off the body (git interpret-trailers rules); body keeps the prose + URL.
    assert_eq!(
        cmt["commit"]["body"],
        "See https://github.com/o/r/pull/7 for context."
    );
    assert_eq!(
        cmt["commit"]["trailers"][1]["key"],
        "Merge-Queue-Pull-Request"
    );
    assert_eq!(cmt["commit"]["trailers"][1]["value"], "7");
    assert_eq!(cmt["commit"]["trailers"].as_array().unwrap().len(), 3);
    let tags = json(&server, "/o/r/api/refs/tags").await?;
    assert_eq!(tags["refs"][0]["name"], "v1");
    let tasks = json(&server, "/o/r/api/tasks").await?;
    assert!(tasks["running"].is_array());
    let (st, _, _) = req(&server, reqwest::Method::GET, "/o/r/api/overview", &[]).await?;
    assert_eq!(st, 200);

    // browser lane: /{o}/{r}/api-browser/… is the same surface (query preserved)
    let (st, text, _) = req(
        &server,
        reqwest::Method::GET,
        &format!("/o/r/api-browser/commits?ref={head}&n=1"),
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{text}");
    let blist: Value = serde_json::from_str(&text)?;
    assert_eq!(blist["commits"].as_array().unwrap().len(), 1);

    // CORS: allowed wildcard origin gets credentials; foreign origin gets nothing; preflight is open.
    let (st, _, hdrs) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api/refs",
        &[("Origin", "https://wiki.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(
        hdr(&hdrs, "access-control-allow-origin"),
        "https://wiki.docs.example.com"
    );
    assert_eq!(hdr(&hdrs, "access-control-allow-credentials"), "true");
    assert!(hdr(&hdrs, "access-control-expose-headers").contains("ETag"));
    assert!(
        hdrs.get_all("vary")
            .iter()
            .any(|hv| hv.to_str().unwrap().contains("Origin"))
    );
    let (st, _, hdrs) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api/refs",
        &[("Origin", "https://evil.example")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&hdrs, "access-control-allow-origin"), "");
    let (st, _, hdrs) = req(
        &server,
        reqwest::Method::OPTIONS,
        "/o/r/api-browser/refs",
        &[
            ("Origin", "https://x.docs.example.com"),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Headers", "authorization"),
        ],
    )
    .await?;
    assert_eq!(st, 204);
    assert!(hdr(&hdrs, "access-control-allow-methods").contains("GET"));
    assert!(
        hdr(&hdrs, "access-control-allow-headers")
            .to_ascii_lowercase()
            .contains("authorization")
    );
    // a state-changing call from a foreign origin is refused before it reaches a handler
    let (st, _, _) = req(
        &server,
        reqwest::Method::DELETE,
        "/o/r/api/policy",
        &[("Origin", "https://evil.example")],
    )
    .await?;
    assert_eq!(st, 403);
    // non-API paths never get CORS headers
    let (_, _, hdrs) = req(
        &server,
        reqwest::Method::GET,
        "/o/r.git/info/refs?service=git-upload-pack",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(hdr(&hdrs, "access-control-allow-origin"), "");
    // the browser lane is the same surface under /api-browser (D27)
    let (st, _, hdrs) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api-browser/refs",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(
        hdr(&hdrs, "access-control-allow-origin"),
        "https://x.docs.example.com"
    );
    // the lane-first forms are gone (banner: no aliases)
    for gone in [
        "/api/v1/repos/o/r",
        "/api/v1/repos/o/r/refs",
        "/api-browser/v1/repos/o/r/refs",
        "/services/api/o/r/refs",
    ] {
        assert_eq!(
            req(&server, reqwest::Method::GET, gone, &[]).await?.0,
            404,
            "{gone} must be gone"
        );
    }

    // policy + repo admin under the repo prefix
    let (st, _, _) = req(&server, reqwest::Method::GET, "/o/r/api/policy", &[]).await?;
    assert_eq!(st, 200);
    let (st, _, _) = req(&server, reqwest::Method::PUT, "/o/new/api", &[]).await?;
    assert!(st.is_success(), "{st}");
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["new", "r"])
    );
    let (st, _, _) = req(&server, reqwest::Method::DELETE, "/o/new/api", &[]).await?;
    assert!(st.is_success(), "{st}");
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["r"])
    );

    // authenticate: anonymous mode is "signed in" → the popup page
    let (st, text, h) = req(
        &server,
        reqwest::Method::GET,
        "/api-browser/v1/authenticate",
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{text}");
    assert!(hdr(&h, "content-type").starts_with("text/html"));
    assert!(text.contains("repos:authenticated"));

    // the SDK artefacts (built into web/dist by `pnpm run build`) at their permanent URLs
    for name in ["/repos.js", "/repos.mjs"] {
        let (st, body, h) = req(&server, reqwest::Method::GET, name, &[]).await?;
        assert_eq!(st, 200, "{name}");
        assert!(
            hdr(&h, "content-type").starts_with("text/javascript"),
            "{name}"
        );
        assert_eq!(hdr(&h, "cache-control"), "no-cache");
        assert!(!hdr(&h, "etag").is_empty());
        // D27: the SDK puts the lane after the repository (`/o/r/api` | `/o/r/api-browser`) and
        // opens `/api-browser/v1/authenticate`; it never emits the deleted lane-first forms.
        assert!(
            body.contains("/api-browser/v1/authenticate") && body.contains("repos:authenticated"),
            "{name} is not the SDK"
        );
        assert!(
            !body.contains("/v1/repos") && !body.contains("services/api/"),
            "{name} emits a deleted lane-first form"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_cors_without_config() -> TestResult {
    let server = Server::start().await?;
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/owners",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    let (st, _, h) = req(
        &server,
        reqwest::Method::OPTIONS,
        "/api/v1/owners",
        &[
            ("Origin", "https://x.docs.example.com"),
            ("Access-Control-Request-Method", "GET"),
        ],
    )
    .await?;
    assert_eq!(st, 204);
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    Ok(())
}

/// D26/D27: everything of a repository under its own prefix — the
/// admin/settings surface at `/{o}/{r}/api[/policy|/settings…]`, and the
/// same under the browser lane `/{o}/{r}/api-browser/…`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d26_prefix_form_matches_v1_alias() -> TestResult {
    let server = Server::start().await?;
    let client = reqwest::Client::new();
    // create via the prefix form
    assert_eq!(
        client.put(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        201
    );
    let repo_api: serde_json::Value = client
        .get(format!("{}/t/pfx/api", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    let repo_browser: serde_json::Value = client
        .get(format!("{}/t/pfx/api-browser", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(repo_api["full_name"], "t/pfx");
    assert_eq!(repo_api["full_name"], repo_browser["full_name"]);
    // settings + policy through the prefix form
    let resp = client
        .put(format!(
            "{}/t/pfx/api/settings?message=via+prefix",
            server.base_url
        ))
        .body("[bundles]\nmin_commits = 7\n")
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "{}", resp.text().await?);
    let settings: serde_json::Value = client
        .get(format!("{}/t/pfx/api-browser/settings", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(settings["revision"], 1);
    assert_eq!(settings["message"], "via prefix");
    let describe: serde_json::Value = client
        .get(format!("{}/t/pfx/api/settings/describe", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(describe["bundles"]["min_commits"], 7);
    let policy: serde_json::Value = client
        .get(format!("{}/t/pfx/api/policy", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert!(policy.is_object());
    let validation: serde_json::Value = client
        .post(format!("{}/t/pfx/api/policy/validate", server.base_url))
        .body("{}")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(validation["ok"], true);
    // refs etc. (already D15)
    assert_eq!(
        client.get(format!("{}/t/pfx/api/refs", server.base_url))
            .send()
            .await?
            .status(),
        200
    );
    // delete via the prefix form
    assert_eq!(
        client.delete(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        204
    );
    assert_eq!(
        client.get(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        404
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_delete_requires_admin() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "writer".into(),
                token: "writer-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            walgit_config::StaticToken {
                principal: "admin".into(),
                token: "admin-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await?;

    let writer = [("Authorization", "Bearer writer-token")];
    let admin = [("Authorization", "Bearer admin-token")];

    assert_eq!(
        req(&server, reqwest::Method::PUT, "/secure/delete/api", &writer,)
            .await?
            .0,
        201,
        "write permission still creates repositories"
    );

    for path in [
        "/secure/delete",
        "/secure/delete/api",
        "/secure/delete/api-browser",
    ] {
        assert_eq!(
            req(&server, reqwest::Method::DELETE, path, &writer)
                .await?
                .0,
            403,
            "non-admin deletion through {path}"
        );
    }
    assert_eq!(
        req(&server, reqwest::Method::GET, "/secure/delete/api", &writer,)
            .await?
            .0,
        200,
        "forbidden deletion must leave the repository intact"
    );

    assert_eq!(
        req(
            &server,
            reqwest::Method::DELETE,
            "/secure/delete/api",
            &admin,
        )
        .await?
        .0,
        204
    );
    assert_eq!(
        req(&server, reqwest::Method::GET, "/secure/delete/api", &admin,)
            .await?
            .0,
        404
    );
    Ok(())
}

async fn call(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    auth: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<reqwest::StatusCode> {
    let mut r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .request(method, format!("{}{path}", server.base_url))
        .header("Accept", "application/json")
        .header("Authorization", auth);
    if let Some(b) = body {
        r = r.json(&b);
    }
    Ok(r.send().await?.status())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_principal_registry_self_only_and_verifiable() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "alice".into(),
                token: "alice-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            walgit_config::StaticToken {
                principal: "bob".into(),
                token: "bob-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
        ];
    })
    .await?;

    let alice = "Bearer alice-token";
    let bob = "Bearer bob-token";

    // Anonymous reads are denied in this deployment.
    assert_eq!(
        req(&server, reqwest::Method::GET, "/api/v1/principals", &[])
            .await?
            .0,
        reqwest::StatusCode::UNAUTHORIZED
    );

    // Self-registration: alice writes her own key.
    assert_eq!(
        call(
            &server,
            reqwest::Method::PUT,
            "/api/v1/principals/alice",
            alice,
            Some(serde_json::json!({ "public_key": "alice-key" })),
        )
        .await?,
        reqwest::StatusCode::OK
    );

    // Bob may not impersonate alice (self-only).
    assert_eq!(
        call(
            &server,
            reqwest::Method::PUT,
            "/api/v1/principals/alice",
            bob,
            Some(serde_json::json!({ "public_key": "bob-key" })),
        )
        .await?,
        reqwest::StatusCode::FORBIDDEN
    );

    // The registry lists alice's key.
    let (st, text, _) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/principals",
        &[("Authorization", alice)],
    )
    .await?;
    assert_eq!(st, reqwest::StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(v["alice"], "alice-key");

    // Bob may not revoke alice; alice may.
    assert_eq!(
        call(&server, reqwest::Method::DELETE, "/api/v1/principals/alice", bob, None).await?,
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&server, reqwest::Method::DELETE, "/api/v1/principals/alice", alice, None).await?,
        reqwest::StatusCode::NO_CONTENT
    );

    let (st, text, _) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/principals",
        &[("Authorization", alice)],
    )
    .await?;
    assert_eq!(st, reqwest::StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(v, serde_json::json!({}));

    Ok(())
}

/// #63: the batch mints upload URLs, so it must require write; an unknown
/// operation is a client error, never silently a download.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lfs_upload_batch_requires_write_and_rejects_unknown_operations() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "reader".into(),
            token: "read-only".into(),
            token_env: None,
            write: false,
            admin: false,
        }];
    })
    .await?;
    let url = format!("{}/o/r.git/info/lfs/objects/batch", server.base_url);
    let client = reqwest::Client::new();
    for (op, status) in [("upload", 403), ("unknown", 400)] {
        let r = client
            .post(&url)
            .bearer_auth("read-only")
            .json(&serde_json::json!({"operation": op, "objects": []}))
            .send()
            .await?;
        assert_eq!(r.status(), status, "operation {op}");
    }
    Ok(())
}

/// #63: the popup names the signed-in principal, so it may only post to an
/// allowlisted opener (or its own origin), and the identity is JSON-encoded
/// with the script the HTML parser sees in mind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn popup_identity_only_targets_an_allowed_opener_and_is_script_safe() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.cors_origins = vec!["https://review.example".into()];
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "</script><script>untrusted()</script>".into(),
            token: "test-token".into(),
            token_env: None,
            write: false,
            admin: false,
        }];
    })
    .await?;
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/authenticate", server.base_url);
    for origin in ["https://untrusted.example", "https://review.example/path"] {
        let r = client
            .get(&url)
            .query(&[("origin", origin)])
            .bearer_auth("test-token")
            .send()
            .await?;
        assert_eq!(r.status(), 403, "origin {origin}");
    }
    let body = client
        .get(&url)
        .query(&[("origin", "https://review.example")])
        .bearer_auth("test-token")
        .send()
        .await?
        .text()
        .await?;
    assert!(!body.contains("</script><script>"));
    assert!(body.contains("postMessage(msg, \"https://review.example\")"));
    assert!(!body.contains("postMessage(msg, \"*\")"));
    let body = client
        .get(&url)
        .bearer_auth("test-token")
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("postMessage(msg, window.location.origin)"));
    Ok(())
}
