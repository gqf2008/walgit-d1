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
mod harness;

use anyhow::Result;
use harness::{Server, TestRepo, git_in};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_routes_serve_index_without_cache() -> Result<()> {
    let server = Server::start().await?;
    let client = reqwest::Client::new();
    let expected = client
        .get(format!("{}/", server.base_url))
        .send()
        .await?
        .text()
        .await?;
    for path in [
        "/",
        "/repos",
        "/commands",
        "/owner",
        "/owner/repo",
        "/owner/repo/tree/main/src/lib.rs",
        "/owner/repo/blob/main/src/lib.rs",
        "/owner/repo/commits",
        "/owner/repo/commits/main",
        "/owner/repo/commit/abc",
        "/owner/repo/wal",
        "/owner/repo/settings",
        "/api",
    ] {
        let response = client
            .get(format!("{}{}", server.base_url, path))
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert!(
            response.headers()["content-type"]
                .to_str()?
                .starts_with("text/html")
        );
        assert_eq!(response.text().await?, expected, "{path}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_md_is_open_markdown_with_etag() -> Result<()> {
    let server = Server::start().await?;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/SKILL.md", server.base_url))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()["content-type"]
            .to_str()?
            .starts_with("text/markdown")
    );
    assert_eq!(response.headers()["cache-control"], "no-cache");
    let etag = response.headers()["etag"].clone();
    let body = response.text().await?;
    assert!(body.contains("walgit"), "the guide mentions walgit");
    // Strong ETag revalidates: If-None-Match → 304.
    let response = client
        .get(format!("{}/SKILL.md", server.base_url))
        .header("if-none-match", &etag)
        .send()
        .await?;
    assert_eq!(response.status(), 304);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assets_have_content_type_and_immutable_cache() -> Result<()> {
    let server = Server::start().await?;
    let client = reqwest::Client::new();
    let index = client
        .get(format!("{}/", server.base_url))
        .send()
        .await?
        .text()
        .await?;
    let marker = "/_ui/assets/";
    let asset = index
        .split('"')
        .find(|part| {
            part.starts_with(marker)
                && part.rsplit('.').next().is_some_and(|e| e.eq_ignore_ascii_case("js"))
        })
        .expect("built index references a JavaScript asset");
    let response = client
        .get(format!("{}{}", server.base_url, asset))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert!(
        response.headers()["content-type"]
            .to_str()?
            .starts_with("text/javascript")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overview_reports_push_and_unknown_repo_is_text_404() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "web overview"])?;
    git_in(
        &src,
        &["push", &server.repo_url("o", "r"), "HEAD:refs/heads/main"],
    )?;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/o/r/api-browser/overview", server.base_url))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await?;
    assert_eq!(body["repo"], "o/r");
    assert!(body["manifest"]["next_seq"].as_u64().unwrap() >= 2);
    assert!(body["packs"]["pushes"].as_u64().unwrap() >= 1);
    assert!(body["bundles"].is_array());
    assert!(body["compactions"].is_array());

    let missing = client
        .get(format!("{}/no/such/api-browser/overview", server.base_url))
        .send()
        .await?;
    assert_eq!(missing.status(), 404);
    assert!(
        missing.headers()["content-type"]
            .to_str()?
            .starts_with("text/plain")
    );
    Ok(())
}

/// Every response carries `Server: walgit/<ver> (<kind>; <who>)` — errors too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_header_names_the_instance() -> anyhow::Result<()> {
    let server = harness::Server::start().await?;
    let c = reqwest::Client::new();
    for path in ["/readyz", "/services/api/owners", "/nope/nope/api-browser/refs"] {
        let r = c.get(format!("{}{path}", server.base_url)).send().await?;
        let v = r
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(v.starts_with("walgit/"), "{path}: Server header = {v:?}");
        assert!(
            v.contains("(dev;") || v.contains("(serverless;") || v.contains("(ssd;"),
            "{path}: {v}"
        );
        // Same value under a name Google's frontend does not rewrite (the edge re-emits it as Server).
        let x = r
            .headers()
            .get("x-walgit-server")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(x, v, "{path}: X-Walgit-Server must equal Server");
    }
    Ok(())
}

/// A `token`-mode host that refuses anonymous reads still has to hand a browser
/// the static shell — otherwise there is no page on which to enter the token
/// (the door with the handle on the inside, D68). Data lanes stay shut even when
/// the caller looks like a browser.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_only_host_still_serves_the_shell_so_a_browser_can_enter_a_token() -> Result<()> {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "boss".into(),
            token: "boss-token".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await?;
    // The loopback-host canonicalisation (a browser on 127.0.0.1 is redirected to
    // `walgit.localhost`) would make this test depend on the OS resolver — Windows
    // does not know `*.localhost`. Connect to the bound address, name the canonical
    // host, and never follow a redirect.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let port = server.base_url.rsplit(':').next().unwrap_or_default().to_string();
    let host = format!("walgit.localhost:{port}");
    let nav = |path: &str| {
        client
            .get(format!("{}{path}", server.base_url))
            .header("Accept", "text/html,application/xhtml+xml")
            .header("Host", host.clone())
            .send()
    };

    // The shell: a page navigation, no credential.
    let index = nav("/").await?;
    assert_eq!(index.status(), 200, "the shell is the token prompt's page");
    let html = index.text().await?;
    assert!(html.contains("id=\"root\""), "served the SPA document");
    assert_eq!(nav("/repos").await?.status(), 200, "a deep page too");

    // Its assets, whose requests are not `text/html`.
    for asset in ["/favicon.ico"] {
        let status = client
            .get(format!("{}{asset}", server.base_url))
            .header("Accept", "*/*")
            .header("Host", host.clone())
            .send()
            .await?
            .status();
        assert!(
            status == 200 || status == 404,
            "{asset} must not be a 401 (got {status})"
        );
    }

    // Data lanes: still 401, browser or not.
    for path in [
        "/api/v1/owners",
        "/api/v1/me",
        "/api/nowhere/api-browser/refs",
        "/mcp",
        "/metrics",
    ] {
        assert_eq!(
            nav(path).await?.status(),
            401,
            "{path} answers with data, so a browser gets no shell there"
        );
    }

    // And the token the prompt collects works.
    let with_token = client
        .get(format!("{}/api/v1/me", server.base_url))
        .header("Host", host.clone())
        .header("Authorization", "Bearer boss-token")
        .send()
        .await?;
    assert_eq!(with_token.status(), 200);
    Ok(())
}

/// The shell bypass is for hosts with **no** browser sign-in. An `oidc` host
/// keeps its redirect to `/_auth/login`: a stateless shell there would replace
/// the only path a browser has to a session with a token prompt the gate cannot
/// satisfy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oidc_host_still_sends_a_page_navigation_to_sign_in() -> Result<()> {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Oidc;
        c.server.auth.anonymous_read = false;
        c.server.auth.issuer = "https://login.example.com".into();
        c.server.auth.allowed_domains = vec!["example.com".into()];
        c.server.auth.oauth_client_id = Some("client".into());
        c.server.auth.oauth_client_secret = Some("secret".into());
        c.server.auth.session_secret = Some("0123456789abcdef0123456789abcdef".into());
    })
    .await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    // Name the canonical host so `canonical_browser_host` (a loopback-host
    // redirect of its own, 302) does not stand in for the sign-in redirect.
    let port = server.base_url.rsplit(':').next().unwrap_or_default().to_string();
    let r = client
        .get(format!("{}/", server.base_url))
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Host", format!("walgit.localhost:{port}"))
        .send()
        .await?;
    assert_eq!(r.status(), 307, "a page navigation goes to sign-in");
    let loc = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        loc.starts_with("/_auth/login?next="),
        "the redirect must be the sign-in flow, got {loc}"
    );
    Ok(())
}
