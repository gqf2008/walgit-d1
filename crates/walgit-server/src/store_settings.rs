//! Admin storage settings for a configured instance (issue #127).
//!
//! The first-run wizard (D43, `setup_wizard`) only serves the unconfigured
//! state; once configured its surface 404s and changing the bucket, endpoint
//! or R2 credentials meant editing `walgit.toml` by hand. This module is the
//! admin surface for that edit — **one mechanism, hung twice**: the same
//! `SetupStore` payload, the same scratch-compose → validate → `toml_edit`
//! write-back (comments and unrelated structure survive), and the same
//! exit-75 supervisor handoff as the wizard's save.
//!
//! Three differences from the wizard, by design:
//! * **Redaction**: `GET /api/v1/store` answers the non-secret parameters in
//!   plaintext and credentials as *presence booleans only* (`has_access_key`
//!   / `has_secret_key`). A secret that left the file once is in someone's
//!   memory; this surface never echoes one.
//! * **Blank = keep**: since the form cannot pre-fill what it cannot read,
//!   an empty credential field on test or save means "leave the current
//!   value alone" (`BlankCreds::Keep`), where the wizard's empty meant
//!   "omit" (`Omit`).
//! * **Scratch from the running config**: the edit-state test overlays the
//!   submitted parameters onto a clone of the live config (not
//!   `Config::default()`), so an unmentioned field (and the `*_env` fallback)
//!   behaves exactly as the instance runs today.
//!
//! All three are admin-gated (`require_admin`, §1.3/D24's challenge shape:
//! missing credential 401, authenticated non-admin 403; `mode = none` on
//! loopback is admin). While the instance is *unconfigured* they answer 404 —
//! there the wizard's own open surface is the entry point.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;
use crate::error::ApiError;
use crate::setup_wizard::{
    BlankCreds, SetupStore, apply_store, edit_store_table, parse_setup_store, probe_store,
    storable_config_path, write_atomic,
};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/store", get(snapshot).put(save))
        .route("/api/v1/store/test", post(test_connection))
        .with_state(state)
}

/// The masked snapshot of the running `[store]` config. `endpoint` follows
/// the active backend (S3's URL or the GCS JSON API endpoint); `region` /
/// `force_path_style` / the credential bits are the S3 fields, false-ish for
/// other backends. Secrets are presence only.
#[derive(Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent fact the editor form needs (path-style + the two credential presence bits + save-ability); collapsing them would lie about the config"
)]
struct StoreSnapshot {
    backend: &'static str,
    bucket: String,
    prefix: String,
    endpoint: String,
    region: String,
    force_path_style: bool,
    has_access_key: bool,
    has_secret_key: bool,
    /// Whether a save here can persist (the instance must run from a file).
    can_save: bool,
}

fn backend_name(backend: walgit_config::StoreBackend) -> &'static str {
    match backend {
        walgit_config::StoreBackend::Gcs => "gcs",
        walgit_config::StoreBackend::S3 => "s3",
        walgit_config::StoreBackend::Memory => "memory",
    }
}

/// `GET /api/v1/store` — the redacted snapshot (admin; 404 in setup state).
async fn snapshot(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if st.needs_setup {
        return Err(ApiError::NotFound(
            "store settings (the first-run wizard owns this instance)".into(),
        ));
    }
    st.auth
        .require_admin(&headers)
        .await
        .map_err(ApiError::from)?;
    let cfg = &st.cfg;
    let s3 = &cfg.store.s3;
    let snapshot = StoreSnapshot {
        backend: backend_name(cfg.store.backend),
        bucket: cfg.store.bucket.clone(),
        prefix: cfg.store.prefix.clone(),
        endpoint: match cfg.store.backend {
            walgit_config::StoreBackend::Gcs => cfg.store.gcs.endpoint.clone(),
            _ => s3.endpoint.clone(),
        },
        region: s3.region.clone(),
        force_path_style: s3.force_path_style,
        has_access_key: s3.access_key.as_deref().is_some_and(|v| !v.is_empty()),
        has_secret_key: s3.secret_key.as_deref().is_some_and(|v| !v.is_empty()),
        can_save: storable_config_path(st.config_path.as_ref()),
    };
    let mut r = axum::Json(snapshot).into_response();
    // Mutable admin state over a credentialed lane: nothing to cache, and a
    // cache could outlive a token revocation (§1.3's spirit).
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(r)
}

/// The edit body: the same `SetupStore` the wizard takes, and nothing else
/// (the first-admin fields are wizard-only — sending them is a 400).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreEdit {
    store: SetupStore,
}

/// Overlay the submitted parameters on a clone of the *running* config —
/// the shared scratch of the test (never persisted) and the save (persisted
/// after validation). Blank credentials keep the running values.
fn scratch_from_running(
    st: &AppState,
    store: &SetupStore,
) -> Result<walgit_config::Config, ApiError> {
    let mut scratch = (*st.cfg).clone();
    apply_store(&mut scratch, store, BlankCreds::Keep)?;
    Ok(scratch)
}

/// Issue #129: the file is the credentials' **only durable carrier** — an
/// environment is not. The editor's GET shows `has_access_key`/`has_secret_key`
/// from the *running* config, which merges `WALGIT__STORE__S3__*` env
/// overrides, and a blank credential keeps whatever the running config had —
/// so a save can happily report "kept" while the file holds no literal
/// value. Both env shapes then boot today and fail to boot once the
/// environment moves on: an override merged into `running` (but absent from
/// `file`), or a credential resolved at store-open from the variable named by
/// `access_key_env`/`secret_key_env`. Say so in the response rather than let
/// the admin discover it at the next restart.
///
/// `lookup` resolves an env var (a seam so the check is unit-testable without
/// mutating the test process's environment).
fn credential_env_warnings(
    file: &walgit_config::Config,
    running: &walgit_config::Config,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !matches!(file.store.backend, walgit_config::StoreBackend::S3) {
        return warnings;
    }
    for slot in ["access_key", "secret_key"] {
        let (file_literal, running_literal, env_name) = match slot {
            "access_key" => (
                file.store.s3.access_key.as_deref().unwrap_or(""),
                running.store.s3.access_key.as_deref().unwrap_or(""),
                file.store.s3.access_key_env.as_str(),
            ),
            _ => (
                file.store.s3.secret_key.as_deref().unwrap_or(""),
                running.store.s3.secret_key.as_deref().unwrap_or(""),
                file.store.s3.secret_key_env.as_str(),
            ),
        };
        if !file_literal.is_empty() {
            continue; // the file carries it; nothing a restart can lose.
        }
        let env_override = !running_literal.is_empty();
        let env_named =
            !env_name.is_empty() && lookup(env_name).is_some_and(|v| !v.is_empty());
        if env_override || env_named {
            warnings.push(format!(
                "the saved config has no literal {slot}: this instance gets it from the environment ({}), which the file does not capture — a restart outside that environment will fail to open the store (put the credential in the form, or move it into the file)",
                if env_override {
                    format!("a WALGIT__STORE__S3__{} override", slot.to_uppercase())
                } else {
                    format!("the `{env_name}` variable named by {slot}_env")
                }
            ));
        }
    }
    warnings
}

/// `POST /api/v1/store/test` — probe a *candidate* (admin; 404 in setup
/// state). Same semantics as the wizard's test: HEAD of a key that cannot
/// exist; a `NotFound` is success; the answer is `{ok, message}`, nothing
/// persisted.
async fn test_connection(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if st.needs_setup {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(e) = st.auth.require_admin(&headers).await {
        return ApiError::from(e).into_response();
    }
    let store = match parse_setup_store(body, "store payload").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let scratch = match scratch_from_running(&st, &store) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    probe_store(&scratch).await
}

/// `PUT /api/v1/store` — persist the edit (admin; 404 in setup state).
/// Compose + validate before touching the file; on success rewrite the
/// running `walgit.toml` with `toml_edit` (blank credentials keep the file's
/// values) and arm the same exit-75 supervisor handoff as the wizard save.
async fn save(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    if st.needs_setup {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Err(e) = st.auth.require_admin(&headers).await {
        return ApiError::from(e).into_response();
    }
    let Some(config_path) = st
        .config_path
        .clone()
        .filter(|p| storable_config_path(Some(p)))
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this instance was not started from a config file (start with walgit serve --config walgit.toml); saved nothing",
        )
            .into_response();
    };
    let bytes = match crate::collect_body(body).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let req: StoreEdit = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid store payload: {e}"),
            )
                .into_response();
        }
    };
    let mut warnings = Vec::new();

    // 1. Compose + validate a scratch (running config ⊕ submission) first —
    //    a refusal must not touch the file.
    let scratch = match scratch_from_running(&st, &req.store) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = scratch.validate() {
        return (
            StatusCode::BAD_REQUEST,
            format!("the composed config does not validate: {e:#}"),
        )
            .into_response();
    }

    // 2. Edit the running file in place: [store] only, blank credentials
    //    keep the file's current values, everything else survives verbatim.
    let text = match std::fs::read_to_string(&config_path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config file {} is unreadable: {e}", config_path.display()),
            )
                .into_response();
        }
    };
    let mut doc: toml_edit::DocumentMut = match text.parse() {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "config file {} is not valid TOML: {e}",
                    config_path.display()
                ),
            )
                .into_response();
        }
    };
    if let Err(e) = edit_store_table(&mut doc, &req.store, BlankCreds::Keep) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("config edit failed: {e:#}"),
        )
            .into_response();
    }
    let edited_text = doc.to_string();

    // 3. The edited file must parse and validate as a whole (it may carry
    //    sections the scratch never saw).
    let parsed: walgit_config::Config = match toml::from_str(&edited_text) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the edited config does not parse: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = parsed.validate() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the edited config does not validate: {e:#}"),
        )
            .into_response();
    }

    // 4. Write atomically (same-dir temp + rename, 0600; #129), check the
    //    env-override shape into warnings, and arm the restart when the CLI
    //    serve path runs under a supervisor (D43).
    if let Err(e) = write_atomic(&config_path, &edited_text) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "config file {} could not be saved: {e}",
                config_path.display()
            ),
        )
            .into_response();
    }
    warnings.extend(credential_env_warnings(&parsed, &st.cfg, |name| {
        std::env::var(name).ok()
    }));
    if st.setup_exit {
        // The tray watcher treats exit 75 as "restart me" (D43). The response
        // must flush first, so the exit lands a beat later.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            std::process::exit(75);
        });
    }
    tracing::info!(path = %config_path.display(), "store settings saved");
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "saved": true,
            "restart": if st.setup_exit { "supervisor" } else { "manual" },
            "warnings": warnings,
            "file": config_path.display().to_string(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(backend: &str, access_key: &str, secret_key: &str) -> SetupStore {
        SetupStore {
            backend: backend.into(),
            bucket: "b".into(),
            endpoint: "http://127.0.0.1:9000".into(),
            region: "us-east-1".into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            force_path_style: true,
        }
    }

    #[test]
    fn blank_credentials_keep_the_running_values_in_the_scratch() {
        let mut cfg = walgit_config::Config::default();
        cfg.store.backend = walgit_config::StoreBackend::S3;
        cfg.store.s3.access_key = Some("keep-me".into());
        cfg.store.s3.secret_key = Some("keep-me-too".into());
        cfg.store.s3.endpoint = "https://old".into();
        cfg.store.bucket = "old-bucket".into();
        let submitted = store("s3", "", "   ");
        apply_store(&mut cfg, &submitted, BlankCreds::Keep).unwrap();
        // Bucket/endpoint are taken from the form; the blanks kept the file.
        assert_eq!(cfg.store.bucket, "b");
        assert_eq!(cfg.store.s3.endpoint, "http://127.0.0.1:9000");
        assert_eq!(cfg.store.s3.access_key.as_deref(), Some("keep-me"));
        assert_eq!(cfg.store.s3.secret_key.as_deref(), Some("keep-me-too"));
    }

    #[test]
    fn blank_credentials_omit_in_the_wizard_shape() {
        let mut cfg = walgit_config::Config::default();
        cfg.store.s3.access_key = Some("was".into());
        apply_store(&mut cfg, &store("s3", "", ""), BlankCreds::Omit).unwrap();
        assert_eq!(cfg.store.s3.access_key, None);
    }

    #[test]
    fn edit_store_table_blank_keeps_and_value_overwrites_in_the_file() {
        let text = "\
# comment survives
[store]
backend = \"s3\"
bucket = \"old\"

[store.s3]
endpoint = \"https://old\"
region = \"auto\"
access_key = \"old-ak\"
secret_key = \"old-sk\"
force_path_style = true
";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        // Blank pair: kept; new bucket: applied; comments: intact.
        edit_store_table(&mut doc, &store("s3", "", ""), BlankCreds::Keep).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# comment survives"), "{out}");
        assert!(out.contains("access_key = \"old-ak\""), "{out}");
        assert!(out.contains("secret_key = \"old-sk\""), "{out}");
        assert!(out.contains("bucket = \"b\""), "{out}");
        assert!(
            out.contains("endpoint = \"http://127.0.0.1:9000\""),
            "{out}"
        );
        // A submitted pair overwrites, of course.
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        edit_store_table(&mut doc, &store("s3", "new-ak", "new-sk"), BlankCreds::Keep).unwrap();
        let out = doc.to_string();
        assert!(out.contains("access_key = \"new-ak\""), "{out}");
        assert!(out.contains("secret_key = \"new-sk\""), "{out}");
    }

    #[test]
    fn snapshot_never_carries_a_secret() {
        let mut cfg = walgit_config::Config::default();
        cfg.store.backend = walgit_config::StoreBackend::S3;
        cfg.store.s3.access_key = Some("s3cr3t-ak".into());
        cfg.store.s3.secret_key = Some("s3cr3t-sk".into());
        let snapshot = StoreSnapshot {
            backend: backend_name(cfg.store.backend),
            bucket: cfg.store.bucket.clone(),
            prefix: cfg.store.prefix.clone(),
            endpoint: cfg.store.s3.endpoint.clone(),
            region: cfg.store.s3.region.clone(),
            force_path_style: cfg.store.s3.force_path_style,
            has_access_key: cfg
                .store
                .s3
                .access_key
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            has_secret_key: cfg
                .store
                .s3
                .secret_key
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            can_save: false,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"has_access_key\":true"), "{json}");
        assert!(json.contains("\"has_secret_key\":true"), "{json}");
        assert!(!json.contains("s3cr3t"), "{json}");
    }

    /// #129-B: the env-override warning — both env shapes (a merged
    /// `WALGIT__…` override visible in the running config, and a
    /// `*_env`-named variable resolved at store-open) warn when the saved
    /// file holds no literal value; a file with literals never warns.
    #[test]
    fn env_supplied_credentials_are_warned_and_literal_ones_are_not() {
        let mut file = walgit_config::Config::default();
        file.store.backend = walgit_config::StoreBackend::S3;
        file.store.s3.access_key_env = "TEST_AK_ENV".into();
        file.store.s3.secret_key_env = "TEST_SK_ENV".into();
        let mut running = file.clone();

        // Nothing anywhere: no warning (the wizard's own GCS/ADC shape).
        let none_lookup = |_: &str| None;
        assert_eq!(credential_env_warnings(&file, &running, none_lookup).len(), 0);

        // A WALGIT__-style override reached the running config only.
        running.store.s3.access_key = Some("from-override".into());
        let w = credential_env_warnings(&file, &running, none_lookup);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("access_key"), "{w:?}");
        assert!(w[0].contains("WALGIT__STORE__S3__ACCESS_KEY"), "{w:?}");

        // The `*_env`-named variable is set in the process environment.
        running.store.s3.access_key = None;
        let lookup = |name: &str| match name {
            "TEST_SK_ENV" => Some("from-env".into()),
            _ => None,
        };
        let w = credential_env_warnings(&file, &running, lookup);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("secret_key"), "{w:?}");
        assert!(w[0].contains("TEST_SK_ENV"), "{w:?}");

        // The file itself carries both: silence.
        file.store.s3.access_key = Some("ak".into());
        file.store.s3.secret_key = Some("sk".into());
        let w = credential_env_warnings(&file, &running, |name| {
            (!name.is_empty()).then_some("x".into())
        });
        assert!(w.is_empty(), "{w:?}");

        // A blank literal reads as absent (it must still warn).
        file.store.s3.access_key = Some(String::new());
        running.store.s3.access_key = Some("from-override".into());
        let w = credential_env_warnings(&file, &running, none_lookup);
        assert_eq!(w.len(), 1, "{w:?}");
    }
}
