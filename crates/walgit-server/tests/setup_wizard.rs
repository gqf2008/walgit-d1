//! The first-run setup wizard (D43): setup-state gating, the open wizard
//! API, and save write-back into the running config file.

mod harness;

use harness::*;
use std::sync::Arc;

type TestResult = anyhow::Result<()>;

/// Setup state (memory without the deliberate-use flag): only the wizard
/// surface answers — the status API, `/` → `/setup`, liveness probes — and
/// everything else is a 503 that names `/setup`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_state_serves_only_the_wizard() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.store.memory_backend_intentional = false;
    })
    .await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // The wizard API is open and truthful.
    let r = client
        .get(format!("{}/api/v1/setup/status", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["needs_setup"], true, "{v}");
    assert_eq!(v["backend"], "memory", "{v}");
    assert_eq!(
        v["can_save"], false,
        "harness servers run without a config file"
    );

    // `/` redirects to the wizard page.
    let r = client.get(&server.base_url).send().await?;
    assert!(r.status().is_redirection(), "{}", r.status());
    assert!(
        r.headers()
            .get("location")
            .unwrap()
            .to_str()?
            .ends_with("/setup")
    );

    // The wizard page shell answers (assets present after `pnpm run build`).
    let r = client
        .get(format!("{}/setup", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());

    // Liveness probes stay open.
    let r = client
        .get(format!("{}/healthz", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());

    // Everything else: 503 with a pointer.
    for path in [
        "/t/x.git/info/refs?service=git-upload-pack",
        "/api/v1/owners",
        "/t/x/api-browser/refs",
    ] {
        let r = client
            .get(format!("{}{}", server.base_url, path))
            .send()
            .await?;
        assert_eq!(r.status(), 503, "{path}: {}", r.status());
        assert!(r.text().await?.contains("/setup"), "{path}");
    }
    Ok(())
}

/// Configured (the harness default): the setup API answers 404 and normal
/// serving is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_instance_answers_404_on_the_setup_api() -> TestResult {
    let server = Server::start().await?;
    let client = reqwest::Client::new();
    let r = client
        .get(format!("{}/api/v1/setup/status", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 404, "{}", r.status());
    // Normal serving works: the repo API answers (no repo → 404 on the repo,
    // not on the gateway).
    let r = client
        .get(format!("{}/api/v1/owners", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());
    Ok(())
}

/// Save writes the composed config back into the running file — comments and
/// unrelated keys survive, the result parses and validates, and the response
/// reports the manual restart (harness servers never exit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_writes_back_and_preserves_comments() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(
        &cfg_path,
        "# 顶注释保留\n[server]\nlisten = \"127.0.0.1:8081\"\n\n[server.auth]\nmode = \"none\"\n\n[store]\n# 内存后端\nbackend = \"memory\"\n",
    )?;
    let server = Server::start_setup(&cfg_path).await?;
    let client = reqwest::Client::new();

    let r = client
        .post(format!("{}/api/v1/setup/save", server.base_url))
        .json(&serde_json::json!({
            "store": {
                "backend": "s3",
                "bucket": "my-bucket",
                "endpoint": "https://acct.r2.cloudflarestorage.com",
                "region": "auto",
                "access_key": "AK",
                "secret_key": "SK",
                "force_path_style": true
            },
            "admin_token": "wgt-first-admin-token"
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let body: serde_json::Value = r.json().await?;
    assert_eq!(body["saved"], true, "{body}");
    assert_eq!(
        body["restart"], "manual",
        "harness servers never exit — {body}"
    );

    let text = std::fs::read_to_string(&cfg_path)?;
    assert!(text.contains("# 顶注释保留"), "{text}");
    assert!(text.contains("# 内存后端"), "{text}");
    assert!(text.contains("backend = \"s3\""), "{text}");
    assert!(text.contains("bucket = \"my-bucket\""), "{text}");
    assert!(text.contains("access_key = \"AK\""), "{text}");
    assert!(text.contains("mode = \"token\""), "{text}");
    assert!(text.contains("principal = \"admin\""), "{text}");
    let cfg: walgit_config::Config = toml::from_str(&text).unwrap();
    assert!(matches!(cfg.store.backend, walgit_config::StoreBackend::S3));
    assert_eq!(cfg.store.s3.access_key.as_deref(), Some("AK"));
    assert_eq!(cfg.server.auth.tokens.len(), 1);
    assert!(cfg.server.auth.tokens[0].admin);
    Ok(())
}

/// Save refuses an invalid composed config without touching the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_refuses_an_invalid_composed_config() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, "[store]\nbackend = \"memory\"\n")?;
    let before = std::fs::read_to_string(&cfg_path)?;
    let server = Server::start_setup(&cfg_path).await?;
    let client = reqwest::Client::new();

    let r = client
        .post(format!("{}/api/v1/setup/save", server.base_url))
        .json(&serde_json::json!({
            "store": { "backend": "s3", "bucket": "" } // empty bucket: validate fails
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 400, "{}", r.status());
    assert_eq!(
        std::fs::read_to_string(&cfg_path)?,
        before,
        "a refused save must not touch the file"
    );
    Ok(())
}

/// F1 regression: an auth mode that is already configured (`token`) makes
/// the save skip the admin step in the FILE too — no live token is written
/// while the response says "skipped".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_with_configured_auth_skips_admin_edits_in_the_file_too() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(
        &cfg_path,
        "# 已有 token 模式
[server.auth]
mode = \"token\"

[[server.auth.tokens]]
principal = \"existing\"
token = \"existing-token\"

[store]
backend = \"memory\"
",
    )?;
    let server = Server::start_setup_with_tweak(&cfg_path, |c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "existing".into(),
            token: "existing-token".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
    })
    .await?;
    let client = reqwest::Client::new();

    let r = client
        .post(format!("{}/api/v1/setup/save", server.base_url))
        .json(&serde_json::json!({
            "store": { "backend": "s3", "bucket": "b", "endpoint": "http://127.0.0.1:9000" },
            "admin_token": "must-not-land"
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let body: serde_json::Value = r.json().await?;
    assert_eq!(body["saved"], true, "{body}");
    assert!(
        body["warnings"][0].as_str().unwrap().contains("skipped"),
        "{body}"
    );

    let text = std::fs::read_to_string(&cfg_path)?;
    assert!(!text.contains("must-not-land"), "{text}");
    let cfg: walgit_config::Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.server.auth.mode, walgit_config::AuthMode::Token);
    assert_eq!(
        cfg.server.auth.tokens.len(),
        1,
        "the file's tokens stay as they were — {text}"
    );
    assert_eq!(cfg.server.auth.tokens[0].token, "existing-token");
    Ok(())
}

/// Setup state refuses a non-loopback bind (fail-closed, §1.3's convention).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_state_refuses_a_non_loopback_bind() -> TestResult {
    let store = walgit_store::memory::MemoryStore::shared();
    let mut cfg = walgit_config::Config::default();
    cfg.store.backend = walgit_config::StoreBackend::Memory;
    cfg.store.memory_backend_intentional = false;
    cfg.store.bucket = "test".into();
    cfg.server.listen = "0.0.0.0:0".parse().unwrap();
    let state = walgit_server::AppState::new(Arc::new(cfg), store).await?;
    let err = walgit_server::serve(state, std::future::pending::<()>())
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("loopback"), "{text}");
    Ok(())
}

// ---- configured-state admin store settings (issue #127) --------------------
//
// Same write-back mechanism as the wizard above, admin-gated and with the
// editor's "blank = keep" credential semantics. The instances here run from
// a real file in a tempdir (`start_configured_with_config`); the in-process
// store stays memory, which is fine — these endpoints read/validate the
// config, they never touch the bucket except through the test probe.

/// A configured file in the shape the wizard's own save produced: a comment
/// to prove `toml_edit` survives, and a credential pair to prove blanks keep.
fn configured_file() -> String {
    "# 顶注释保留
[server]
listen = \"127.0.0.1:8081\"

[store]
backend = \"s3\"
bucket = \"old-bucket\"

[store.s3]
# 凭据在文件里
endpoint = \"https://old.example.com\"
region = \"auto\"
access_key = \"old-ak\"
secret_key = \"old-sk\"
force_path_style = true
"
    .to_string()
}

/// While the wizard owns the instance, the admin store surface answers 404
/// (it passes the gate precisely so both entry points stay single-owned).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_surface_404s_while_the_wizard_owns_the_instance() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, "[store]\nbackend = \"memory\"\n")?;
    let server = Server::start_setup(&cfg_path).await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let body = serde_json::json!({ "store": { "backend": "s3", "bucket": "b" } });
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 404, "{}", r.status());
    let r = client
        .post(format!("{}/api/v1/store/test", server.base_url))
        .json(&body)
        .send()
        .await?;
    assert_eq!(r.status(), 404, "{}", r.status());
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&body)
        .send()
        .await?;
    assert_eq!(r.status(), 404, "{}", r.status());

    // The gate still owns everything else it ever owned.
    let r = client
        .get(format!("{}/api/v1/owners", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 503, "{}", r.status());
    assert!(r.text().await?.contains("/setup"));
    Ok(())
}

/// `GET /api/v1/store` on a configured instance: the parameters in clear,
/// the credentials only as presence bits — the secret text must not appear
/// anywhere in the response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_snapshot_is_redacted() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, configured_file())?;
    let server = Server::start_configured_with_config(&cfg_path, |c| {
        c.store.backend = walgit_config::StoreBackend::S3;
        c.store.bucket = "prod-bucket".into();
        c.store.prefix = "data/".into();
        c.store.s3.endpoint = "https://acct.r2.cloudflarestorage.com".into();
        c.store.s3.region = "auto".into();
        c.store.s3.access_key = Some("literal-ak-value".into());
        c.store.s3.secret_key = Some("literal-sk-value".into());
        c.store.s3.force_path_style = true;
    })
    .await?;
    let client = reqwest::Client::new();

    // mode `none` on loopback = admin: the surface answers without a credential.
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());
    assert_eq!(
        r.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let text = r.text().await?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(v["backend"], "s3", "{v}");
    assert_eq!(v["bucket"], "prod-bucket", "{v}");
    assert_eq!(v["prefix"], "data/", "{v}");
    assert_eq!(
        v["endpoint"], "https://acct.r2.cloudflarestorage.com",
        "{v}"
    );
    assert_eq!(v["region"], "auto", "{v}");
    assert_eq!(v["force_path_style"], true, "{v}");
    assert_eq!(v["has_access_key"], true, "{v}");
    assert_eq!(v["has_secret_key"], true, "{v}");
    assert_eq!(v["can_save"], true, "harness armed config_path");
    assert!(!text.contains("literal-ak-value"), "{text}");
    assert!(!text.contains("literal-sk-value"), "{text}");
    assert!(!text.contains("old-ak"), "{text}");
    Ok(())
}

/// `PUT /api/v1/store` with blank credential fields keeps what the file has
/// (comments too); a submitted pair overwrites; and the wizard-only
/// `admin_token` field is a 400 on this surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_edit_save_blank_credentials_keep_the_file_values() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, configured_file())?;
    let server = Server::start_configured_with_config(&cfg_path, |_| {}).await?;
    let client = reqwest::Client::new();

    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({
            "store": {
                "backend": "s3",
                "bucket": "new-bucket",
                "endpoint": "https://new.example.com",
                "region": "auto",
                "force_path_style": true
            }
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let body: serde_json::Value = r.json().await?;
    assert_eq!(body["saved"], true, "{body}");
    assert_eq!(
        body["restart"], "manual",
        "harness servers never exit — {body}"
    );

    let text = std::fs::read_to_string(&cfg_path)?;
    assert!(text.contains("# 顶注释保留"), "{text}");
    assert!(text.contains("# 凭据在文件里"), "{text}");
    assert!(text.contains("bucket = \"new-bucket\""), "{text}");
    assert!(
        text.contains("endpoint = \"https://new.example.com\""),
        "{text}"
    );
    assert!(text.contains("access_key = \"old-ak\""), "{text}");
    assert!(text.contains("secret_key = \"old-sk\""), "{text}");

    // A new pair overwrites.
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({
            "store": {
                "backend": "s3",
                "bucket": "new-bucket",
                "access_key": "fresh-ak",
                "secret_key": "fresh-sk"
            }
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let text = std::fs::read_to_string(&cfg_path)?;
    assert!(text.contains("access_key = \"fresh-ak\""), "{text}");
    assert!(text.contains("secret_key = \"fresh-sk\""), "{text}");
    assert!(!text.contains("old-ak"), "{text}");

    // The first-admin fields are wizard-only: sending them is refused, not
    // silently dropped.
    let before = text;
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({
            "store": { "backend": "s3", "bucket": "b2" },
            "admin_token": "must-not-land"
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 400, "{}", r.status());
    assert_eq!(std::fs::read_to_string(&cfg_path)?, before);
    Ok(())
}

/// A refused save (empty bucket) does not touch the file, and a
/// not-from-a-file instance refuses the save with a 503 that explains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_edit_save_refusals() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, configured_file())?;
    let server = Server::start_configured_with_config(&cfg_path, |_| {}).await?;
    let client = reqwest::Client::new();

    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({ "store": { "backend": "s3", "bucket": "" } }))
        .send()
        .await?;
    assert_eq!(r.status(), 400, "{}", r.status());
    assert_eq!(std::fs::read_to_string(&cfg_path)?, configured_file());

    // No config file → nothing to edit (library/test shape): 503, and the
    // snapshot says so via can_save.
    let server = Server::start().await?;
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["can_save"], false, "{v}");
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({ "store": { "backend": "s3", "bucket": "b" } }))
        .send()
        .await?;
    assert_eq!(r.status(), 503, "{}", r.status());
    assert!(r.text().await?.contains("--config"));
    Ok(())
}

/// `POST /api/v1/store/test` probes the *submitted* parameters overlaid on
/// the RUNNING config: blank credentials keep the running literals (a probe
/// that would otherwise have no way to sign succeeds), a reachable bucket
/// answers `{ok:true}` and an unreachable one `{ok:false}` with 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_edit_test_probes_the_candidate_with_kept_credentials() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A fake S3 endpoint: every HEAD answers 404, which the probe contract
    // counts as "bucket reachable with these credentials".
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let probe_addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                let _ = sock.flush().await;
            });
        }
    });

    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, configured_file())?;
    let server = Server::start_configured_with_config(&cfg_path, |c| {
        c.store.backend = walgit_config::StoreBackend::S3;
        c.store.bucket = "live-bucket".into();
        c.store.s3.access_key = Some("keep-ak".into());
        c.store.s3.secret_key = Some("keep-sk".into());
    })
    .await?;
    let client = reqwest::Client::new();

    // Blank credentials + the fake endpoint: the kept running credentials
    // sign the probe; the 404 reads as success.
    let r = client
        .post(format!("{}/api/v1/store/test", server.base_url))
        .json(&serde_json::json!({
            "backend": "s3",
            "bucket": "b",
            "endpoint": format!("http://{probe_addr}"),
            "region": "us-east-1",
            "force_path_style": true
        }))
        .send()
        .await?;
    let status = r.status();
    let v: serde_json::Value = r.json().await?;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["bucket"], "b", "{v}");

    // An unreachable endpoint is a surfaced failure, not a panic or a lie.
    let r = client
        .post(format!("{}/api/v1/store/test", server.base_url))
        .json(&serde_json::json!({
            "backend": "s3",
            "bucket": "b",
            "endpoint": "http://127.0.0.1:1",
            "region": "us-east-1",
            "force_path_style": true
        }))
        .send()
        .await?;
    assert_eq!(r.status(), 400, "{}", r.status());
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["ok"], false, "{v}");

    // Validation rejections answer the wizard's text shape.
    let r = client
        .post(format!("{}/api/v1/store/test", server.base_url))
        .json(&serde_json::json!({ "backend": "s3", "bucket": "" }))
        .send()
        .await?;
    assert_eq!(r.status(), 400, "{}", r.status());
    assert!(r.text().await?.contains("bucket is required"));
    Ok(())
}

/// The store surface is admin-gated with §1.3's channel semantics: a missing
/// credential challenges (401, Bearer-only), an authenticated non-admin is a
/// real 403 that never reaches the file, and the admin token gets through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_surface_requires_admin() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(&cfg_path, configured_file())?;
    let server = Server::start_configured_with_config(&cfg_path, |c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "root".into(),
                token: "t-admin".into(),
                token_env: None,
                write: true,
                admin: true,
            },
            walgit_config::StaticToken {
                principal: "reader".into(),
                token: "t-read".into(),
                token_env: None,
                write: false,
                admin: false,
            },
        ];
    })
    .await?;
    let client = reqwest::Client::new();

    // No credential: 401 (a challenge), Bearer-only (browser surface, #91).
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .send()
        .await?;
    assert_eq!(r.status(), 401, "{}", r.status());
    let www = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(www.starts_with("bearer") && !www.contains("basic"), "{www}");

    // Authenticated but not admin: a real 403 on every lane.
    for (method, path) in [
        (reqwest::Method::GET, "/api/v1/store"),
        (reqwest::Method::PUT, "/api/v1/store"),
        (reqwest::Method::POST, "/api/v1/store/test"),
    ] {
        let mut req = client.request(method.clone(), format!("{}{path}", server.base_url));
        if !matches!(method, reqwest::Method::GET) {
            req = req.json(&serde_json::json!({ "store": { "backend": "s3", "bucket": "evil" } }));
        }
        let r = req.header("authorization", "Bearer t-read").send().await?;
        assert_eq!(r.status(), 403, "{method} {path}: {}", r.status());
    }
    assert_eq!(std::fs::read_to_string(&cfg_path)?, configured_file());

    // Admin: through.
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .header("authorization", "Bearer t-admin")
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.status());
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .header("authorization", "Bearer t-admin")
        .json(&serde_json::json!({ "store": { "backend": "s3", "bucket": "moved" } }))
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    assert!(std::fs::read_to_string(&cfg_path)?.contains("bucket = \"moved\""));
    // `me` carries the admin bit so the SPA can gate its entry.
    let r = client
        .get(format!("{}/api/v1/me", server.base_url))
        .header("authorization", "Bearer t-admin")
        .send()
        .await?;
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["admin"], true, "{v}");
    let r = client
        .get(format!("{}/api/v1/me", server.base_url))
        .header("authorization", "Bearer t-read")
        .send()
        .await?;
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["admin"], false, "{v}");
    Ok(())
}

// ---- #129: storable config files and env-override warnings ---------------

/// Test-only environment mutation (edition-2024 makes `set_var`/`remove_var`
/// unsafe): both names are unique to this test (`WALGIT_TEST_129_*`) and no
/// other test in this binary reads them.
#[allow(unsafe_code)]
fn set_env(key: &str, value: &str) {
    // SAFETY: `key` is one of the uniquely named WALGIT_TEST_129_* variables;
    // no other test reads them, and each call is sequenced with its request.
    unsafe { std::env::set_var(key, value) };
}

#[allow(unsafe_code)]
fn unset_env(key: &str) {
    // SAFETY: same unique test-scoped names as `set_env`.
    unsafe { std::env::remove_var(key) };
}

/// `--config /dev/null` (D39's explicit defaults+env form) accepts writes
/// and discards them, so neither save surface may claim to have saved:
/// both report `can_save: false` and answer the save with the same 503 a
/// fileless instance gets.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_file_config_path_is_not_savable_on_either_surface() -> TestResult {
    let client = reqwest::Client::new();
    let payload = serde_json::json!({ "store": { "backend": "s3", "bucket": "b" } });

    // The wizard (setup state).
    let server = Server::start_setup(std::path::Path::new("/dev/null")).await?;
    let r = client
        .get(format!("{}/api/v1/setup/status", server.base_url))
        .send()
        .await?;
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["can_save"], false, "{v}");
    let r = client
        .post(format!("{}/api/v1/setup/save", server.base_url))
        .json(&payload)
        .send()
        .await?;
    assert_eq!(r.status(), 503, "{}", r.status());
    assert!(r.text().await?.contains("--config"));

    // The configured-state editor.
    let server = Server::start_configured_with_config(std::path::Path::new("/dev/null"), |c| {
        c.store.backend = walgit_config::StoreBackend::S3;
        c.store.bucket = "b".into();
    })
    .await?;
    let r = client
        .get(format!("{}/api/v1/store", server.base_url))
        .send()
        .await?;
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["can_save"], false, "{v}");
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&payload)
        .send()
        .await?;
    assert_eq!(r.status(), 503, "{}", r.status());
    assert!(r.text().await?.contains("--config"));
    Ok(())
}

/// A save whose file ends up without literal credentials while the
/// environment supplies them (here via the `*_env` names) must say so:
/// the running instance boots, a restart outside that environment will not.
/// (The wizard-era one-shot became a repeatable operation with the editor —
/// issue #129's B.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_edit_save_warns_when_the_file_would_hold_no_literal_credential() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("walgit.toml");
    std::fs::write(
        &cfg_path,
        "[store]\nbackend = \"s3\"\nbucket = \"b\"\n\n[store.s3]\nendpoint = \"http://127.0.0.1:9000\"\nregion = \"us-east-1\"\naccess_key_env = \"WALGIT_TEST_129_AK\"\nsecret_key_env = \"WALGIT_TEST_129_SK\"\nforce_path_style = true\n",
    )?;
    let server = Server::start_configured_with_config(&cfg_path, |c| {
        c.store.backend = walgit_config::StoreBackend::S3;
        c.store.bucket = "b".into();
        c.store.s3.access_key_env = "WALGIT_TEST_129_AK".into();
        c.store.s3.secret_key_env = "WALGIT_TEST_129_SK".into();
    })
    .await?;
    let client = reqwest::Client::new();
    let edit = serde_json::json!({
        "store": {
            "backend": "s3",
            "bucket": "new-b",
            "endpoint": "http://127.0.0.1:9000",
            "region": "us-east-1",
            "force_path_style": true
        }
    });

    // With the env vars set: blank credentials save (the Keep semantics) but
    // warn that the file holds nothing durable.
    set_env("WALGIT_TEST_129_AK", "env-ak");
    set_env("WALGIT_TEST_129_SK", "env-sk");
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&edit)
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let body: serde_json::Value = r.json().await?;
    assert_eq!(body["saved"], true, "{body}");
    let warnings = body["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 2, "{body}");
    let joined = warnings
        .iter()
        .map(|w| w.as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("WALGIT_TEST_129_AK"), "{joined}");
    assert!(joined.contains("WALGIT_TEST_129_SK"), "{joined}");
    let text = std::fs::read_to_string(&cfg_path)?;
    assert!(!text.contains("access_key = "), "no literal landed: {text}");

    // With the environment gone: nothing env-supplies the save, no warning.
    unset_env("WALGIT_TEST_129_AK");
    unset_env("WALGIT_TEST_129_SK");
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&edit)
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let body: serde_json::Value = r.json().await?;
    assert_eq!(
        body["warnings"].as_array().expect("warnings array").len(),
        0,
        "{body}"
    );

    // A submission that writes literal credentials silences the warning even
    // with the env set again.
    set_env("WALGIT_TEST_129_AK", "env-ak");
    set_env("WALGIT_TEST_129_SK", "env-sk");
    let r = client
        .put(format!("{}/api/v1/store", server.base_url))
        .json(&serde_json::json!({
            "store": {
                "backend": "s3",
                "bucket": "new-b",
                "endpoint": "http://127.0.0.1:9000",
                "access_key": "literal-ak",
                "secret_key": "literal-sk"
            }
        }))
        .send()
        .await?;
    let body: serde_json::Value = r.json().await?;
    assert_eq!(
        body["warnings"].as_array().expect("warnings array").len(),
        0,
        "{body}"
    );
    unset_env("WALGIT_TEST_129_AK");
    unset_env("WALGIT_TEST_129_SK");
    Ok(())
}
