//! The first-run setup wizard (D43, issue #70).
//!
//! A `memory` store without `memory_backend_intentional` means "unconfigured"
//! (the installer's initial config ships exactly that shape): the instance
//! serves only the wizard — `/setup` (a client route of the SPA shell) and its
//! `/_ui/*` assets, the data-free `/api/v1/setup/*` API (same open rule as
//! `/_auth/*`), and the liveness probes — while every other path answers 503
//! with a pointer to `/setup`. Setup state additionally requires loopback
//! listening (refused at startup otherwise, fail-closed like §1.3).
//!
//! The wizard configures object storage (S3/R2 — R2 is S3-compatible — or
//! GCS with ADC), tests the bucket, and writes everything — store section and
//! the first admin token — back into `walgit.toml` via `toml_edit`, which
//! preserves the file's comments and unrelated structure. The CLI serve path
//! then exits 75 for the supervisor (the tray watcher, D43) to restart into
//! the configured instance.
//!
//! Once configured (`backend != memory` or the flag set), `needs_setup` is
//! false, the gate is not mounted and `/api/v1/setup/*` answers 404.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::web::ui;
use crate::{AppState, error::ApiError};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/setup/status", get(status))
        .route("/api/v1/setup/test", post(test_connection))
        .route("/api/v1/setup/save", post(save))
        .with_state(state)
}

/// The outermost gate while the instance is in setup state (mounted only when
/// `needs_setup`, so the configured path never pays for it). Everything the
/// wizard needs is answered here, ahead of the auth and compression layers;
/// the setup API passes through to its own router; all else is a 503 that
/// names `/setup`.
pub async fn gate(req: Request<Body>, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    // Liveness probes stay open (a supervisor must tell the wizard instance is alive).
    if path == "/healthz" || path == "/readyz" {
        return next.run(req).await;
    }
    if path == "/api/v1/setup" || path.starts_with("/api/v1/setup/") {
        return next.run(req).await;
    }
    // The configured-state store surface (#127) is not the wizard's: let it
    // reach its handler so it answers 404 here (the wizard owns setup state),
    // rather than the gate's 503.
    if path == "/api/v1/store" || path.starts_with("/api/v1/store/") {
        return next.run(req).await;
    }
    // The wizard page is a client route of the SPA shell; its assets ride the
    // normal immutable/no-cache asset rules. Served here — the auth layer
    // would 401: setup state has no credentials yet.
    if path == "/setup" {
        return ui::shell(&method, req.headers());
    }
    if let Some(asset_path) = path.strip_prefix("/_ui/") {
        return ui::asset_bytes(asset_path, &method, req.headers());
    }
    if path == "/" {
        return Redirect::temporary("/setup").into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "walgit: setup required — object storage is not configured yet; open /setup to run the first-run wizard",
    )
        .into_response()
}

/// `GET /api/v1/setup/status` — why the wizard owns this instance and what it
/// can do. 404 once configured.
#[derive(Serialize)]
struct Status {
    needs_setup: bool,
    backend: &'static str,
    auth_mode: &'static str,
    can_save: bool,
}

async fn status(State(st): State<Arc<AppState>>) -> Response {
    if !st.needs_setup {
        return StatusCode::NOT_FOUND.into_response();
    }
    let auth_mode = match st.cfg.server.auth.mode {
        walgit_config::AuthMode::None => "none",
        walgit_config::AuthMode::Token => "token",
        walgit_config::AuthMode::Oidc => "oidc",
    };
    axum::Json(Status {
        needs_setup: true,
        backend: "memory",
        auth_mode,
        can_save: storable_config_path(st.config_path.as_ref()),
    })
    .into_response()
}

/// What the wizard offers the user to configure (steps ① + ③ of the issue).
/// Shared with the configured-state admin surface (#127): the same shape is
/// the body of `POST /api/v1/store/test` and of the `PUT /api/v1/store` edit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupStore {
    /// `s3` (R2 is S3-compatible) or `gcs`.
    pub(crate) backend: String,
    pub(crate) bucket: String,
    #[serde(default)]
    pub(crate) endpoint: String,
    #[serde(default)]
    pub(crate) region: String,
    /// Literal credentials (D43). Empty = the `*_env` names in the config win
    /// (kept as-is) — the S3-only fields are ignored for `gcs`.
    #[serde(default)]
    pub(crate) access_key: String,
    #[serde(default)]
    pub(crate) secret_key: String,
    #[serde(default = "default_true")]
    pub(crate) force_path_style: bool,
}

fn default_true() -> bool {
    true
}

/// Whether a save can persist to the `--config` path (issue #129, shared by
/// the wizard's status/save and the configured-state editor's snapshot/save,
/// D44). Only a regular file: `--config /dev/null` is D39's explicit
/// defaults+env form (`NUL` plays that role on Windows), and a character
/// device or FIFO accepts every write and discards all of them — "saved"
/// would be a lie, so both surfaces refuse it exactly like a missing file.
pub(crate) fn storable_config_path(path: Option<&std::path::PathBuf>) -> bool {
    let Some(path) = path else {
        return false;
    };
    path.metadata().is_ok_and(|m| m.is_file())
}

/// A unique sibling temp name (same directory, so the rename is atomic on
/// the filesystem that holds the config).
fn temp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("walgit.toml");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    path.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), nanos))
}

/// Write the config file **atomically** (issue #129): a same-directory temp
/// file created 0600, filled, fsynced, then renamed over the target. The
/// wizard save and the admin editor save (D44) share this one path.
///
/// `walgit.toml` is the only durable carrier of the literal credentials, and
/// a lost file strands the instance (a missing `--config` file is exit 2,
/// D39). `std::fs::write` truncates first — a crash or ENOSPC mid-write
/// destroyed it. Rename is the linearization point here, like the manifest
/// CAS for the bucket: readers see either the old file or the new one, never
/// a half-written config.
///
/// Hardened by the #133 audit (issue #135): the post-rename directory fsync
/// makes the rename itself durable ([`sync_parent_dir`]), the temp cleanup
/// only ever touches a file this call created, and the 0600 is a chmod, not
/// a `mode()` request the umask can mask away.
pub(crate) fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    write_atomic_via(path, &temp_sibling(path), text)
}

/// The one write; `tmp` is a separate argument so the "cleanup deletes only
/// our own file" contract is testable (issue #135: `temp_sibling` embeds a
/// nanosecond, so its collision path is otherwise unreachable in a unit
/// test).
fn write_atomic_via(path: &std::path::Path, tmp: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    // Only a temp this call created is this call's to remove. If
    // `create_new` failed the name belonged to someone else — a concurrent
    // save in this process that lost the nanosecond lottery, or any file at
    // that path — and removing it here would destroy the *winner's* in
    // flight write (#135).
    let mut ours = false;
    let result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(tmp)?;
        ours = true;
        #[cfg(unix)]
        {
            // `mode(0o600)` is a request the process umask masks: a
            // pathological umask (any owner bit in it, e.g. 0o277) would
            // land a config the next boot cannot read or write — the file
            // that holds the only copy of the credentials locking itself
            // out. chmod is immune to the umask (#135).
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(tmp, path)?;
        #[cfg(unix)]
        sync_parent_dir(path).map_err(|e| {
            // The rename has landed: the config IS on disk. A plain io error
            // here would surface through the callers' shared
            // "config file … could not be saved" arm reading as if nothing
            // was written — a lie (PR #136 review B1 ②). Say what is true:
            // written; only the durability confirmation failed, and a retry
            // of the save is harmless.
            std::io::Error::other(format!(
                "written, but its durability could not be confirmed: \
                 parent-directory fsync failed: {e}"
            ))
        })?;
        Ok(())
    })();
    if result.is_err() && ours {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Persist the **rename** itself: the new file's bytes reached the disk with
/// the temp's fsync, but the directory entry swap is metadata the filesystem
/// keeps separately — after a power loss the parent directory can come back
/// holding the old mapping, silently reverting the config (#135). Opening
/// the parent and `sync_all`-ing it flushes that entry. A rename is atomic
/// either way, so the worst failure mode stays "old config", never a torn
/// one. On Windows this is skipped: `std` cannot open a directory without
/// `FILE_FLAG_BACKUP_SEMANTICS`, and NTFS journals metadata, so a completed
/// rename survives a crash on its own.
#[cfg(unix)]
fn sync_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    // `Path::new("walgit.toml").parent()` is `Some("")`, not `None` — the
    // empty component must resolve to the working directory (PR #136 review
    // B1). Without this, every bare relative `--config` name (the documented
    // shape, AGENTS.md §0: `--config walgit.standalone.toml`) failed the
    // post-rename sync on `File::open("")` — after the rename had already
    // landed, so the save was reported 500 while the file was written.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::OpenOptions::new().read(true).open(parent)?.sync_all()
}

/// What a **blank** credential field means, which differs by surface:
/// the first-run wizard omits blank credentials (the `*_env` names win),
/// while the admin editor (#127) keeps what the config already holds —
/// a GET never echoes the secrets, so the edit form can only pre-fill
/// blanks, and a blank must not read as "clear them".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlankCreds {
    /// Empty field ⇒ the literal credential is absent (env names win).
    Omit,
    /// Empty field ⇒ leave the existing literal credential untouched.
    Keep,
}

/// Apply the submitted store to a scratch config — shared by the test
/// connection (never persisted) and the save (persisted after validation).
pub(crate) fn apply_store(
    cfg: &mut walgit_config::Config,
    store: &SetupStore,
    blank: BlankCreds,
) -> Result<(), ApiError> {
    if store.bucket.trim().is_empty() {
        return Err(ApiError::BadRequest("bucket is required".into()));
    }
    cfg.store.bucket = store.bucket.trim().to_string();
    match store.backend.as_str() {
        "s3" => {
            cfg.store.backend = walgit_config::StoreBackend::S3;
            let s3 = &mut cfg.store.s3;
            s3.endpoint = store.endpoint.trim().to_string();
            s3.region = if store.region.trim().is_empty() {
                "auto".to_string()
            } else {
                store.region.trim().to_string()
            };
            set_credential(&mut s3.access_key, &store.access_key, blank);
            set_credential(&mut s3.secret_key, &store.secret_key, blank);
            s3.force_path_style = store.force_path_style;
        }
        "gcs" => {
            cfg.store.backend = walgit_config::StoreBackend::Gcs;
            // Credentials ride ADC/IAM; only the JSON API endpoint (an
            // emulator, or empty = real GCS) and the bucket are wizard
            // concerns.
            cfg.store.gcs.endpoint = store.endpoint.trim().to_string();
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "store backend must be \"s3\" or \"gcs\", got {other:?}"
            )));
        }
    }
    Ok(())
}

/// One credential line of `apply_store`: a submitted value always wins; a
/// blank one is omitted (wizard) or kept (editor) per `blank`.
fn set_credential(slot: &mut Option<String>, submitted: &str, blank: BlankCreds) {
    let submitted = submitted.trim();
    if !submitted.is_empty() {
        *slot = Some(submitted.to_string());
    } else if blank == BlankCreds::Omit {
        *slot = None;
    }
}

/// `POST /api/v1/setup/test` — build a store from the submitted params (never
/// persisted) and probe the bucket. A `memory`-rooted scratch is the wizard's
/// shape; the configured-state editor (#127) probes a scratch rooted at the
/// running config instead.
async fn test_connection(State(st): State<Arc<AppState>>, body: Body) -> Response {
    if !st.needs_setup {
        return StatusCode::NOT_FOUND.into_response();
    }
    let store = match parse_setup_store(body, "setup payload").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let mut scratch = walgit_config::Config::default();
    if let Err(e) = apply_store(&mut scratch, &store, BlankCreds::Omit) {
        return e.into_response();
    }
    probe_store(&scratch).await
}

/// Parse a `SetupStore` JSON body, answering the wizard's 400 shape on failure
/// (the boxed `Response` keeps the `Result` under clippy's `result_large_err`).
pub(crate) async fn parse_setup_store(body: Body, what: &str) -> Result<SetupStore, Box<Response>> {
    let bytes = match crate::collect_body(body).await {
        Ok(b) => b,
        Err(e) => return Err(Box::new(e.into_response())),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        Box::new((StatusCode::BAD_REQUEST, format!("invalid {what}: {e}")).into_response())
    })
}

/// Open a store on the scratch config and probe the bucket: HEAD of a key
/// that cannot exist yet. A `NotFound` *is* success (credentials + bucket
/// reachable, no such object); a success is success too; anything else is
/// the surfaced error. Nothing is persisted either way.
pub(crate) async fn probe_store(scratch: &walgit_config::Config) -> Response {
    let backend = match scratch.store.backend {
        walgit_config::StoreBackend::Gcs => "gcs",
        walgit_config::StoreBackend::S3 => "s3",
        walgit_config::StoreBackend::Memory => "memory",
    };
    match walgit_store::open_store(scratch).await {
        Ok(opened) => {
            let probe_key = format!("{}setup-probe", scratch.store_prefix());
            match opened.head(&probe_key).await {
                Ok(_) | Err(walgit_store::StoreError::NotFound { .. }) => (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "ok": true,
                        "backend": backend,
                        "bucket": scratch.store.bucket,
                        "message": "bucket reachable with these credentials",
                    })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "backend": backend,
                        "message": format!("{e:#}"),
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "ok": false,
                "backend": backend,
                "message": format!("{e:#}"),
            })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupSave {
    store: SetupStore,
    /// The first admin (step ③). Written only when the current auth mode is
    /// `none` — an already-locked-down config is left to its own admin tooling.
    #[serde(default)]
    admin_token: Option<String>,
    #[serde(default)]
    admin_principal: Option<String>,
}

/// `POST /api/v1/setup/save` — validate the composed config, then edit the
/// running `walgit.toml` in place with `toml_edit` (comments and unrelated
/// keys survive), and — when the CLI serve path armed it — exit 75 so the
/// supervisor restarts into the configured instance.
async fn save(State(st): State<Arc<AppState>>, body: Body) -> Response {
    if !st.needs_setup {
        return StatusCode::NOT_FOUND.into_response();
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
    let req: SetupSave = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid setup payload: {e}"),
            )
                .into_response();
        }
    };
    let mut warnings = Vec::new();

    // 1. Compose + validate a scratch config *before* touching the file.
    let mut scratch = walgit_config::Config::default();
    if let Err(e) = apply_store(&mut scratch, &req.store, BlankCreds::Omit) {
        return e.into_response();
    }
    // The admin step applies only when the running auth is `none` (the wizard
    // bootstraps the *first* identity). An already-configured auth is left to
    // its own admin tooling — the same decision must govern the file edit,
    // not just this scratch validation (a divergent edit would write a live
    // admin token while telling the user it was skipped).
    let write_admin = req
        .admin_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
        && st.cfg.server.auth.mode == walgit_config::AuthMode::None;
    if req
        .admin_token
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
        && !write_admin
    {
        warnings.push(
            "auth is already configured (not `none`); the admin step was skipped".to_string(),
        );
    }
    if write_admin {
        scratch.server.auth.mode = walgit_config::AuthMode::Token;
        scratch.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: req
                .admin_principal
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .unwrap_or("admin")
                .to_string(),
            token: req
                .admin_token
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string(),
            token_env: None,
            write: true,
            admin: true,
        }];
    }
    if let Err(e) = scratch.validate() {
        return (
            StatusCode::BAD_REQUEST,
            format!("the composed config does not validate: {e:#}"),
        )
            .into_response();
    }

    // 2. Edit the running file in place, preserving everything else.
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
    if let Err(e) = edit_document(&mut doc, &req, write_admin) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("config edit failed: {e:#}"),
        )
            .into_response();
    }
    let edited_text = doc.to_string();

    // 3. The edited file must parse and validate as a whole (independent of
    //    the scratch run above — the file may carry sections the scratch
    //    config did not).
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

    // 4. Write atomically (same-dir temp + rename, 0600; #129 — the file is
    //    the literal credentials' only carrier, a torn write strands the
    //    instance) and arm the restart when the CLI armed it.
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
    if st.setup_exit {
        // The tray watcher treats exit 75 as "restart me" (D43). The response
        // must flush first, so the exit lands a beat later.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            std::process::exit(75);
        });
    }
    tracing::info!(path = %config_path.display(), "setup wizard saved the config");
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "saved": true,
            "restart": if st.setup_exit { "supervisor" } else { "manual" },
            "warnings": warnings,
            "file": config_path.display().to_string(),
        })),
    )
        .into_response()
}

/// Edit the TOML document: `[store]` (+ `[store.s3]` / `[store.gcs]`) and,
/// for a first admin, `[server.auth]` mode + `[[server.auth.tokens]]`.
fn edit_document(
    doc: &mut toml_edit::DocumentMut,
    req: &SetupSave,
    write_admin: bool,
) -> anyhow::Result<()> {
    edit_store_table(doc, &req.store, BlankCreds::Omit)?;

    if write_admin && let Some(token) = req.admin_token.as_deref().filter(|t| !t.trim().is_empty())
    {
        use toml_edit::{Item, Table, value};
        let principal = req
            .admin_principal
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or("admin");
        let server_entry = doc.entry("server").or_insert(Item::Table(Table::new()));
        let server = server_entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[server] is not a table"))?;
        let auth_entry = server.entry("auth").or_insert(Item::Table(Table::new()));
        let auth = auth_entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[server.auth] is not a table"))?;
        auth["mode"] = value("token");
        let tokens_entry = auth
            .entry("tokens")
            .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
        let aot = tokens_entry
            .as_array_of_tables_mut()
            .ok_or_else(|| anyhow::anyhow!("server.auth.tokens is not an array of tables"))?;
        let mut first = Table::new();
        first.insert("principal", value(principal));
        first.insert("token", value(token.trim()));
        first.insert("write", value(true));
        first.insert("admin", value(true));
        aot.push(first);
    }
    Ok(())
}

/// Edit only the `[store]` section (+ `[store.s3]` / `[store.gcs]`) of the
/// TOML document, preserving every comment and unrelated key. Shared by the
/// first-run save (blank ⇒ clear) and the configured-state editor (blank ⇒
/// keep, #127). `blank` controls the literal credential fields only.
pub(crate) fn edit_store_table(
    doc: &mut toml_edit::DocumentMut,
    store: &SetupStore,
    blank: BlankCreds,
) -> anyhow::Result<()> {
    use toml_edit::{Item, Table, value};

    let store_entry = doc.entry("store").or_insert(Item::Table(Table::new()));
    let store_tbl = store_entry
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[store] is not a table"))?;
    store_tbl["backend"] = value(&store.backend);
    store_tbl["bucket"] = value(store.bucket.trim());
    store_tbl.remove("memory_backend_intentional");
    match store.backend.as_str() {
        "s3" => {
            store_tbl.remove("gcs");
            let s3_entry = store_tbl.entry("s3").or_insert(Item::Table(Table::new()));
            let s3 = s3_entry
                .as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("[store.s3] is not a table"))?;
            s3["endpoint"] = value(store.endpoint.trim());
            s3["region"] = value(if store.region.trim().is_empty() {
                "auto"
            } else {
                store.region.trim()
            });
            s3["force_path_style"] = value(store.force_path_style);
            // One pair, two policies: the wizard treats the two fields as a
            // unit (blank access key clears both), the editor keeps each
            // field it was not given (#127) — an echo-back of "keep" must
            // never read as "clear".
            match blank {
                BlankCreds::Omit => {
                    if store.access_key.trim().is_empty() {
                        s3.remove("access_key");
                        s3.remove("secret_key");
                    } else {
                        s3["access_key"] = value(store.access_key.trim());
                        s3["secret_key"] = value(store.secret_key.trim());
                    }
                }
                BlankCreds::Keep => {
                    if !store.access_key.trim().is_empty() {
                        s3["access_key"] = value(store.access_key.trim());
                    }
                    if !store.secret_key.trim().is_empty() {
                        s3["secret_key"] = value(store.secret_key.trim());
                    }
                }
            }
        }
        "gcs" => {
            store_tbl.remove("s3");
            let gcs_entry = store_tbl.entry("gcs").or_insert(Item::Table(Table::new()));
            let gcs = gcs_entry
                .as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("[store.gcs] is not a table"))?;
            gcs["endpoint"] = value(store.endpoint.trim());
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_document_keeps_comments_and_sets_store_and_admin() {
        let text = "\
# 顶注释保留
[server]
listen = \"127.0.0.1:8081\"

[server.auth]
# 安装器初始配置:本机回环
mode = \"none\"

[store]
# 内存后端:数据不落盘
backend = \"memory\"
";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let req = SetupSave {
            store: SetupStore {
                backend: "s3".into(),
                bucket: "my-bucket".into(),
                endpoint: "https://acct.r2.cloudflarestorage.com".into(),
                region: "auto".into(),
                access_key: "AK".into(),
                secret_key: "SK".into(),
                force_path_style: true,
            },
            admin_token: Some("wgt-first-admin-token".into()),
            admin_principal: None,
        };
        edit_document(&mut doc, &req, true).unwrap();
        let out = doc.to_string();
        // Comments survive; the store keys changed; the auth gained a token.
        assert!(out.contains("# 顶注释保留"), "{out}");
        assert!(out.contains("# 内存后端:数据不落盘"), "{out}");
        assert!(out.contains("backend = \"s3\""), "{out}");
        assert!(out.contains("bucket = \"my-bucket\""), "{out}");
        assert!(
            out.contains("endpoint = \"https://acct.r2.cloudflarestorage.com\""),
            "{out}"
        );
        assert!(out.contains("access_key = \"AK\""), "{out}");
        assert!(!out.contains("mode = \"none\""), "{out}");
        assert!(out.contains("mode = \"token\""), "{out}");
        assert!(out.contains("principal = \"admin\""), "{out}");
        assert!(out.contains("admin = true"), "{out}");
        // And the whole thing still parses as a config.
        let cfg: walgit_config::Config = toml::from_str(&out).unwrap();
        assert!(matches!(cfg.store.backend, walgit_config::StoreBackend::S3));
        assert_eq!(cfg.store.s3.access_key.as_deref(), Some("AK"));
        assert_eq!(cfg.server.auth.tokens.len(), 1);
        assert!(cfg.server.auth.tokens[0].admin);
    }

    #[test]
    fn literal_creds_absent_clears_them() {
        let text = "[store]\nbackend = \"s3\"\nbucket = \"b\"\n[store.s3]\naccess_key = \"old\"\nsecret_key = \"old\"\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let req = SetupSave {
            store: SetupStore {
                backend: "s3".into(),
                bucket: "b".into(),
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                access_key: String::new(),
                secret_key: String::new(),
                force_path_style: false,
            },
            admin_token: None,
            admin_principal: None,
        };
        edit_document(&mut doc, &req, true).unwrap();
        let out = doc.to_string();
        assert!(!out.contains("access_key = \"old\""), "{out}");
        let cfg: walgit_config::Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.store.s3.access_key, None);
    }

    // ---- #129: atomic write, storable config paths ----------------------

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(&path, "backend = \"memory\"\n").unwrap();
        write_atomic(&path, "backend = \"s3\"\nbucket = \"b\"\n").unwrap();
        // The target holds the new text…
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "backend = \"s3\"\nbucket = \"b\"\n"
        );
        // …and the directory holds exactly the config: no `.tmp` sibling
        // survived the rename. (The crash semantics of temp+rename cannot be
        // tested here; this pins the visible contract of the fix.)
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("walgit.toml")], "{entries:?}");
        #[cfg(unix)]
        {
            // The file that carries literal credentials is 0600 (D43).
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        // Repeated saves (the editor surface made this a repeated operation)
        // still leave a single file.
        write_atomic(&path, "# second\n").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# second\n");
    }

    #[test]
    fn a_failed_atomic_write_keeps_the_old_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(&path, "old = true\n").unwrap();
        // A missing parent directory fails the temp creation — the target
        // must still hold the old bytes (the truncating `fs::write` of the
        // same name would instead have destroyed a writable path, and neither
        // may leave a half-file behind).
        let deep = dir.path().join("nope").join("walgit.toml");
        assert!(write_atomic(&deep, "new = true\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old = true\n");
    }

    /// #135: a `create_new` collision (the same nanosecond in two
    /// concurrent saves, or any file already at the temp path) means the
    /// temp belongs to the *other* writer. Losing the race must leave that
    /// file — and the target — exactly as they were; the old cleanup
    /// deleted the winner's in-flight temp.
    #[test]
    fn write_atomic_removes_only_a_temp_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        std::fs::write(&path, "old = true\n").unwrap();
        let other_temp = dir.path().join("walgit.toml.someone-else.tmp");
        std::fs::write(&other_temp, "in-flight payload\n").unwrap();
        // The fixed-name temp stands in for the winner of a collision —
        // `temp_sibling` embeds a nanosecond, so the collision itself is
        // not reachable from a test without pinning the name.
        assert!(matches!(
            write_atomic_via(&path, &other_temp, "new = true\n"),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            std::fs::read_to_string(&other_temp).unwrap(),
            "in-flight payload\n",
            "the other writer's temp must survive our defeat"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old = true\n");
    }

    /// #135: `OpenOptionsExt::mode` is masked by the process umask, so a
    /// pathological umask used to be able to land the credentials carrier
    /// read-only or unreadable to its owner. The post-open `set_permissions`
    /// is a chmod and immune. The mask is process-global, so the assertion
    /// runs in a dedicated single-test child (where setting it is safe) —
    /// this binary's threads never see it, and CI asserts the real thing.
    /// (Crash durability of the post-rename directory fsync cannot be
    /// asserted from userspace — that step's contract is pinned below, and
    /// the property it buys is the filesystem's.)
    #[cfg(unix)]
    #[test]
    fn write_atomic_pins_permissions_past_a_pathological_umask() {
        let self_exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(self_exe)
            .args([
                "--ignored",
                "--exact",
                "setup_wizard::tests::write_atomic_under_pathological_umask_child",
            ])
            .env("WALGIT_UMASK_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "the umask child failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The child half of the test above — `#[ignore]`d so it never runs in
    /// the parent's concurrent threads, and gated on `WALGIT_UMASK_CHILD`
    /// so even a deliberate `--ignored` sweep (the test-slow tier) refuses
    /// to mutate the shared mask there.
    #[cfg(unix)]
    #[test]
    #[ignore = "spawned single-test by write_atomic_pins_permissions_past_a_pathological_umask"]
    #[allow(unsafe_code)]
    fn write_atomic_under_pathological_umask_child() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var_os("WALGIT_UMASK_CHILD").is_none() {
            // A bare `--ignored` sweep (the test-slow tier): the mask must
            // not be set next to other threads, and the spawning test
            // covers this assertion — skip quietly.
            eprintln!("skipped: not spawned by the parent test");
            return;
        }
        let dir = tempfile::tempdir().unwrap(); // under the normal mask
        let path = dir.path().join("walgit.toml");
        // SAFETY: `umask` is process-global; this child is one test on one
        // thread, created solely for this assertion, and the mask is
        // restored on every path below. 0o277 keeps owner-read but strips
        // owner-write and all group/other bits — enough to defeat a bare
        // `mode(0o600)` request.
        let previous = unsafe { libc::umask(0o277) };
        let wrote = write_atomic(&path, "backend = \"s3\"\n");
        // SAFETY: restore the captured mask immediately after the masked
        // write; nothing else runs in this process.
        unsafe { libc::umask(previous) };
        wrote.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o} — the chmod must beat the umask");
        // And the next save still works — the file is writable by its owner.
        write_atomic(&path, "# second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# second\n");
    }

    /// B1 regression (PR #136 review): `--config walgit.toml` — a **bare
    /// relative name** — is a documented shape (AGENTS.md §0:
    /// `--config walgit.standalone.toml`). Its `Path::parent()` is
    /// `Some("")`, not `None`; before the fix the post-rename parent fsync
    /// failed on `File::open("")` with ENOENT *after the rename had
    /// landed*, so both save surfaces answered 500 "config file … is not
    /// writable" for a config that was in fact written. cwd is
    /// process-global, so the child runs single-test like the umask one.
    #[cfg(unix)]
    #[test]
    fn write_atomic_survives_a_bare_relative_config_name() {
        let self_exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(self_exe)
            .args([
                "--ignored",
                "--exact",
                "setup_wizard::tests::write_atomic_bare_relative_name_child",
            ])
            .env("WALGIT_BARE_NAME_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "the bare-relative-name child failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The child half of the test above — `#[ignore]`d and env-gated like
    /// the umask child (chdir is process-global too).
    #[cfg(unix)]
    #[test]
    #[ignore = "spawned single-test by write_atomic_survives_a_bare_relative_config_name"]
    fn write_atomic_bare_relative_name_child() {
        if std::env::var_os("WALGIT_BARE_NAME_CHILD").is_none() {
            eprintln!("skipped: not spawned by the parent test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let rel = std::path::Path::new("walgit.toml");
        write_atomic(rel, "backend = \"s3\"\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(rel).unwrap(),
            "backend = \"s3\"\n",
            "the bare relative config must be written and readable"
        );
    }

    /// #135: the fsync-after-rename is a real durability step. The helper's
    /// own contract: a live parent syncs; a vanished one errors. Two things
    /// this deliberately does NOT pin, both settled elsewhere (PR #136
    /// review B1): a **bare relative name** must never reach `File::open("")`
    /// — `write_atomic` normalizes the empty parent to "." (the child test
    /// above) — and a post-rename sync failure is wrapped by
    /// `write_atomic` as "written, but durability unconfirmed", so the save
    /// surfaces never report a written config as "not writable". (What the
    /// fsync buys — the rename surviving a power loss — is a filesystem
    /// property no userspace test can witness.)
    #[cfg(unix)]
    #[test]
    fn sync_parent_dir_flushes_a_live_directory_and_reports_a_dead_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walgit.toml");
        write_atomic(&path, "a = 1\n").unwrap(); // includes the parent sync
        assert!(sync_parent_dir(&path).is_ok());
        let gone = dir.path().join("gone");
        std::fs::create_dir(&gone).unwrap();
        let inside = gone.join("walgit.toml");
        std::fs::remove_dir(&gone).unwrap();
        assert!(
            sync_parent_dir(&inside).is_err(),
            "a vanished parent must fail, not silently pass"
        );
    }

    #[test]
    fn only_a_regular_config_file_is_storable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("walgit.toml");
        std::fs::write(&file, "").unwrap();
        assert!(storable_config_path(Some(&file)));
        assert!(!storable_config_path(None), "no --config file at all");
        assert!(
            !storable_config_path(Some(&dir.path().join("absent.toml"))),
            "a path that does not exist is not a config file"
        );
        assert!(
            !storable_config_path(Some(&dir.path().to_path_buf())),
            "a directory is not a file"
        );
        #[cfg(unix)]
        assert!(
            !storable_config_path(Some(&std::path::PathBuf::from("/dev/null"))),
            "`--config /dev/null` (D39) accepts writes and discards them"
        );
        #[cfg(windows)]
        assert!(
            !storable_config_path(Some(&std::path::PathBuf::from("NUL"))),
            "`--config NUL` (D39) accepts writes and discards them"
        );
    }
}
