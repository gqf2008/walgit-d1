//! MCP over HTTP Streamable (`/mcp`) — the server side of D64.
//!
//! **The transport is `rmcp`'s**, not ours: sessions, `Mcp-Session-Id`, the
//! `text/event-stream` lane a server uses to push notifications, `DELETE`
//! termination and protocol-version negotiation are the SDK's job, and this
//! module only answers the four questions MCP asks a server —
//! `tools/list`+`tools/call`, `resources/list`+`resources/read`,
//! `resources/templates/list`, and `resources/subscribe`+`unsubscribe`.
//!
//! What is *not* the SDK's job, and is this module's:
//!
//! * **Authentication is HTTP-level, before any JSON-RPC is parsed.** A caller
//!   without a credential gets a real `401` with `WWW-Authenticate: Bearer`
//!   (never `Basic` — `/mcp` is browser-reachable, AGENTS.md §1.3), and the
//!   identity that lands in the handler is the one `Auth::require_read` named.
//!   The request's own headers are stuffed into a [`Caller`] extension so each
//!   tool call can replay them on the inner API request it makes
//!   ([`super::api`]) — a client can never exceed its own bearer.
//! * **CORS/Origin** — a foreign `Origin` is refused here, using the same
//!   allowlist (`server.cors_origins`) as the rest of the server, and `OPTIONS`
//!   is answered here too (the SDK transport registers no preflight, and a
//!   browser client must not be asked for a credential before it may ask).
//! * **Host validation** — rmcp's anti-DNS-rebinding check stays on and is fed
//!   the authorities this deployment answers to (see [`trusted_hosts`]);
//!   disabling it is the documented fallback for a deployment that never said
//!   where it lives (`server.public_url`).
//! * **Subscriptions are pollers** (D52/D55). `resources/subscribe` starts one
//!   bounded task per (caller, session, URI) that reads the repository's
//!   refs-level seq every `mcp.subscribe_interval` and sends
//!   `notifications/resources/updated` when it moves. No request is held open
//!   between changes: the seq read *is* a conditional `manifest.pb` GET, the
//!   same observation lane `git ls-remote` and `walgit collab watch` use. A
//!   watched repository that disappears sends `notifications/resources/list_changed`
//!   (the one thing that capability means) and ends the subscription.
//!
//! The subscription lane rides rmcp's **legacy** surface
//! (`resources/subscribe`/`unsubscribe`, `with_legacy_session_mode(true)`) on
//! purpose: `subscriptions/listen` (2026-07-28) is a long request we do not
//! offer — D55 rules out a held request, so the polling lane is the only one
//! walgit implements. That is why both methods carry `#[allow(deprecated)]`.
//!
//! The tool and resource *content* lives next door: [`tools`] (the surface) and
//! [`resources`] (the URIs), both over [`api`] — walgit's own JSON API called
//! in-process, so there is no second contract to drift.

pub mod api;
pub mod resources;
pub mod signing;
pub mod tools;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig,
        SubscribeRequestParams, Tool as McpTool, UnsubscribeRequestParams,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData,
};
use serde_json::Value;
use tower::ServiceExt;

use crate::AppState;

/// The MCP endpoint (D64). One route, three methods: `POST` a message, `GET`
/// the server→client stream, `DELETE` the session.
pub const ENDPOINT: &str = "/mcp";

/// SSE keep-alive between the server and one client.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// A POST body larger than this is refused before it is parsed. A tool call is
/// a few KiB; 8 MiB is the ceiling that keeps `body: "<a whole release note>"`
/// possible without letting one request exhaust memory.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The caller's identity, resolved by [`serve`] before anything else runs and
/// carried on the request's extensions into every handler.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: String,
}

/// Subscriptions: `(principal, session, uri)` → the poller watching it.
///
/// All three parts are in the key because all three are part of the answer. The
/// principal, because authorization decides it. The session, because the
/// notification travels down *that* stream: keying without it made a reconnect
/// get `ok` for a poller still bound to the old, dead peer (silently no
/// notifications), and made one session's `unsubscribe` abort another's poller.
/// And the URI, because that is what is watched.
///
/// One lock covers the cap check, the reservation and the abort-handle install,
/// and every entry carries a generation so a stale handle can never remove the
/// entry that replaced it. The registry is per-instance and disposable (D52).
#[derive(Default)]
pub struct Mcp {
    subs: parking_lot::Mutex<HashMap<Key, Subscription>>,
    generation: AtomicU64,
}

/// `(principal, session, uri)`.
type Key = (String, String, String);

struct Subscription {
    /// The refs-level seq the client was last told about.
    version: String,
    /// Identifies this poller, so `unsubscribe` and the poller's own guard only
    /// ever touch the entry they own.
    generation: u64,
    /// `None` only between "slot reserved" and "task spawned" (both under the
    /// same lock, so no other request can observe it).
    abort: Option<tokio::task::AbortHandle>,
}

impl Mcp {
    pub fn is_empty(&self) -> bool {
        self.subs.lock().is_empty()
    }
    pub fn len(&self) -> usize {
        self.subs.lock().len()
    }
    /// Stop watching one URI for one session (a no-op when nothing watches it).
    pub fn unsubscribe(&self, principal: &str, session: &str, uri: &str) {
        let key: Key = (principal.to_string(), session.to_string(), uri.to_string());
        if let Some(abort) = self.subs.lock().remove(&key).and_then(|s| s.abort) {
            abort.abort();
        }
    }
}

/// `/mcp`, mounted by `crate::router`.
pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let factory = {
        let state = state.clone();
        move || Ok(WalgitMcp { st: state.clone() })
    };
    // `mcp.session_ttl` is rmcp's session inactivity timeout: a session whose
    // stream died (a crashed client, an HTTP/2 `RST_STREAM`) is dropped instead
    // of living on forever as a zombie. It is a field, not a struct literal —
    // `LocalSessionManager`/`SessionConfig` are `#[non_exhaustive]`.
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(state.cfg.mcp.session_ttl);
    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(KEEPALIVE))
        .with_legacy_session_mode(true)
        .with_max_request_body_bytes(MAX_BODY_BYTES)
        .with_allowed_hosts(trusted_hosts(&state.cfg));
    // Origin is checked by [`serve`] against the server's own allowlist, so the
    // SDK's own origin list stays off (it cannot see `server.cors_origins`'s
    // wildcard entries).
    let service: StreamableHttpService<WalgitMcp, LocalSessionManager> =
        StreamableHttpService::new(factory, Arc::new(sessions), config.disable_allowed_origins());
    let service = Arc::new(service);
    Router::new().route(
        ENDPOINT,
        any(move |req: Request| {
            let state = state.clone();
            let service = service.clone();
            async move { serve(state, service, req).await }
        }),
    )
}

/// The authorities this deployment answers to: loopback, this host's own bind
/// address, and `server.public_url`'s authority. rmcp rejects a `Host` outside
/// this set, which is what stops a DNS-rebinding page from talking to a
/// loopback server through its own domain.
///
/// A deployment that never said where it lives (`public_url` unset, wildcard
/// bind) gets rmcp's permissive default instead — pinning it to loopback there
/// would break every edge-fronted deployment that never set `public_url`.
/// `[mcp]` and D64 say so; setting `server.public_url` turns the check on.
fn trusted_hosts(cfg: &walgit_config::Config) -> Vec<String> {
    let mut hosts: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let listen = cfg.server.listen;
    if let Some(public_url) = &cfg.server.public_url
        && let Some(authority) = authority_of(public_url)
    {
        hosts.push(authority);
    }
    if !listen.ip().is_unspecified() {
        hosts.push(listen.ip().to_string());
    }
    hosts.sort();
    hosts.dedup();
    if cfg.server.public_url.is_none() && listen.ip().is_unspecified() {
        // Nothing names this host: an empty list means "any Host" in rmcp.
        return Vec::new();
    }
    hosts
}

/// `https://walgit.example.com:8080/x` → `walgit.example.com:8080`.
fn authority_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_ascii_lowercase())
    }
}

/// Authenticate, check `Origin`, then hand the request to the SDK transport.
async fn serve(
    state: Arc<AppState>,
    service: Arc<StreamableHttpService<WalgitMcp, LocalSessionManager>>,
    mut req: Request,
) -> Response {
    let headers = req.headers().clone();
    let is_delete = req.method() == Method::DELETE;
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // A CORS preflight carries no credential by definition: answering it with a
    // 401 would leave a browser client unable to ask at all.
    if req.method() == Method::OPTIONS {
        return preflight(&headers);
    }
    let principal = match state.auth.require_read(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_failure(e),
    };
    req.extensions_mut().insert(Caller {
        principal: principal.name.clone(),
    });
    // `oneshot` consumes the request; the transport returns the body it built
    // (an SSE stream, a JSON answer, or 202/204). A clone is cheap — the service
    // holds the session manager behind an `Arc`, which is why the old
    // `Arc::try_unwrap(..).unwrap_or_else(..)` never unwrapped either.
    match (*service).clone().oneshot(req).await {
        Ok(resp) => {
            let mut resp = resp.map(Body::new);
            // A terminated session is `204 No Content` in the spec; rmcp answers
            // `202 Accepted`, which a client reads as "still running".
            if is_delete && resp.status() == StatusCode::ACCEPTED {
                *resp.status_mut() = StatusCode::NO_CONTENT;
            }
            resp.into_response()
        }
        Err(never) => match never {},
    }
}

/// The credential is missing, stale or unreadable — one answer for all three
/// (a client that may retry must be told to, with a challenge). `Forbidden` is
/// *not* one of them (a retry cannot fix it), and a verifier that is down is a
/// `503`: answering 401 would make the client discard a credential that is
/// still good (`Auth::` maps it the same way, §1.3).
fn auth_failure(e: crate::auth::AuthError) -> Response {
    match e.status() {
        StatusCode::FORBIDDEN => StatusCode::FORBIDDEN.into_response(),
        StatusCode::SERVICE_UNAVAILABLE => {
            let mut resp = StatusCode::SERVICE_UNAVAILABLE.into_response();
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("15"),
            );
            resp
        }
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    // Bearer only: a `Basic` challenge on a browser-reachable surface pops the
    // browser's native password dialog (§1.3).
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    resp.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer realm=\"walgit\""),
    );
    resp
}

/// `OPTIONS /mcp`: the CORS answer a browser needs before it may send the
/// bearer at all. Only an allowed `Origin` gets headers.
fn preflight(headers: &HeaderMap) -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let Some(origin) = headers.get(axum::http::header::ORIGIN).cloned() else {
        return resp;
    };
    let h = resp.headers_mut();
    h.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        axum::http::HeaderValue::from_static("true"),
    );
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("POST, GET, DELETE, OPTIONS"),
    );
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static(
            "authorization, content-type, accept, mcp-session-id, mcp-protocol-version, last-event-id, x-walgit-authorization, x-walgit-capabilities",
        ),
    );
    h.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        axum::http::HeaderValue::from_static("600"),
    );
    h.insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("origin"),
    );
    resp
}

/// A browser `Origin` must be one this server serves: either it is in
/// `server.cors_origins`, or it is this request's own origin. Non-browser
/// clients send no `Origin` and pass.
fn origin_allowed(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    crate::web::v1::origin_allowed(&state.cfg, origin) || same_origin(origin, host_of(headers))
}

fn host_of(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
}

/// The half of the rule that needs no configuration: `Origin` names this very
/// request's authority. The test below drives *this* function, so what it pins
/// is what production runs.
fn same_origin(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    match origin.split_once("://") {
        Some((scheme, rest)) if scheme == "http" || scheme == "https" => {
            rest.split('/').next() == Some(host)
        }
        _ => false,
    }
}

/// The MCP server: stateless except for the subscriptions in [`AppState::mcp`].
#[derive(Clone)]
pub struct WalgitMcp {
    st: Arc<AppState>,
}

impl WalgitMcp {
    fn write(&self) -> bool {
        self.st.cfg.mcp.key_dir.is_some()
    }
}

/// The HTTP request an SDK handler was called for: headers to replay, the
/// principal to act as, and the MCP session (the default `session` a
/// `collab_claim` records, so a resumed session recognises its own claim).
struct RequestIn {
    headers: HeaderMap,
    principal: String,
    session: String,
}

fn request_in(ctx: &RequestContext<RoleServer>) -> Result<RequestIn, ErrorData> {
    let parts = ctx
        .extensions
        .get::<http::request::Parts>()
        .ok_or_else(|| ErrorData::internal_error("the request carried no HTTP parts", None))?;
    let principal = parts
        .extensions
        .get::<Caller>()
        .map(|c| c.principal.clone())
        .ok_or_else(|| ErrorData::internal_error("the request carried no caller", None))?;
    let session = parts
        .headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    Ok(RequestIn {
        headers: parts.headers.clone(),
        principal,
        session,
    })
}

impl ServerHandler for WalgitMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(
            "walgit",
            crate::instance::build_version(),
        ))
        .with_instructions(INSTRUCTIONS)
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the ServerHandler contract is an async trait; this answer needs no await"
    )]
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // `tools/list` filters by tier: a write tool is advertised only when
        // this host can sign for a caller (`mcp.key_dir`), so `tools/call` can
        // never reach what the list hides (D49).
        let items = tools::defs(self.write())
            .into_iter()
            .map(|t| {
                let schema: JsonObject = match t.schema.as_object() {
                    Some(m) => m.clone(),
                    None => JsonObject::new(),
                };
                McpTool::new(t.name, t.description, Arc::new(schema))
            })
            .collect();
        Ok(ListToolsResult::with_all_items(items))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let req = request_in(&context)?;
        let args = request
            .arguments
            .map_or_else(|| Value::Object(JsonObject::new()), Value::Object);
        match tools::call(
            &self.st,
            &req.headers,
            &req.principal,
            &req.session,
            &request.name,
            &args,
        )
        .await
        {
            Ok(value) => Ok(CallToolResponse::Complete(CallToolResult::success(
                vec![ContentBlock::text(tools::as_text(&value))],
            ))),
            // A wrong *request* is a protocol error the model can fix; a tool
            // that ran and failed is content the model must read (this is the
            // distinction the MCP spec draws, and the adapter already makes).
            Err(tools::ToolError::InvalidParams(m)) => Err(ErrorData::invalid_params(m, None)),
            Err(tools::ToolError::Execution(m)) => Ok(CallToolResponse::Complete(
                CallToolResult::error(vec![ContentBlock::text(m)]),
            )),
        }
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the ServerHandler contract is an async trait; this answer needs no await"
    )]
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let req = request_in(&context)?;
        let items = resources::list(&self.st, &req.principal).await?;
        Ok(ListResourcesResult::with_all_items(items))
    }

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the ServerHandler contract is an async trait; this answer needs no await"
    )]
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            resources::templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let req = request_in(&context)?;
        let contents = resources::read(&self.st, &req.headers, &request.uri).await?;
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(vec![
            contents,
        ])))
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let req = request_in(&context)?;
        self.watch(
            &req.principal,
            &req.session,
            &request.uri,
            context.peer.clone(),
        )
        .await
    }

    #[allow(
        deprecated,
        clippy::unused_async_trait_impl,
        reason = "the ServerHandler contract is an async trait; dropping a subscription needs no await"
    )]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let req = request_in(&context)?;
        self.st
            .mcp
            .unsubscribe(&req.principal, &req.session, &request.uri);
        Ok(())
    }
}

impl WalgitMcp {
    /// Start watching one URI for this caller's session (idempotent: a second
    /// `subscribe` for the same session and URI keeps the existing poller).
    async fn watch(
        &self,
        principal: &str,
        session: &str,
        uri_str: &str,
        peer: rmcp::service::Peer<RoleServer>,
    ) -> Result<(), ErrorData> {
        let uri = resources::parse(uri_str)?;
        let cfg = &self.st.cfg.mcp;
        let interval = cfg.subscribe_interval;
        if interval.is_zero() {
            return Err(ErrorData::internal_error(
                "mcp.subscribe_interval is zero",
                None,
            ));
        }
        // The version is read *outside* the registry lock: it is a bucket
        // round trip, and subscribing must not serialize every other client.
        let version = resources::version(&self.st, &uri)
            .await
            .map_err(resources::PollError::into_error_data)?;
        let key: Key = (principal.to_string(), session.to_string(), uri_str.to_string());
        // From here to the end of the function is one critical section: the cap
        // check, the reservation and the abort handle. Two racing `subscribe`s
        // can then never both pass the cap, and no `unsubscribe` can slip
        // between "reserved" and "watching".
        let mut subs = self.st.mcp.subs.lock();
        if subs.contains_key(&key) {
            return Ok(());
        }
        // Counted per caller across its sessions: the budget is what one caller
        // costs the bucket per interval, not one stream's.
        let mine = subs.keys().filter(|k| k.0 == principal).count();
        if mine >= cfg.max_subscriptions {
            return Err(ErrorData::invalid_params(
                format!(
                    "at most {} subscriptions per caller (mcp.max_subscriptions)",
                    cfg.max_subscriptions
                ),
                None,
            ));
        }
        let generation = self.st.mcp.generation.fetch_add(1, Ordering::Relaxed) + 1;
        subs.insert(
            key.clone(),
            Subscription {
                version,
                generation,
                abort: None,
            },
        );
        let task = tokio::spawn(poll(
            self.st.clone(),
            key.clone(),
            generation,
            uri,
            interval,
            peer,
        ));
        match subs.get_mut(&key) {
            Some(slot) if slot.generation == generation => {
                slot.abort = Some(task.abort_handle());
            }
            // Unreachable while the entry cannot be removed under this lock;
            // aborting is the only safe answer if that ever stops being true.
            _ => task.abort(),
        }
        Ok(())
    }
}

/// One subscription's poller. Bounded by `interval`, ended by `unsubscribe`, by
/// a session that went away, or by its own failure — and [`Unregister`] is what
/// keeps the registry honest however it ends.
async fn poll(
    st: Arc<AppState>,
    key: Key,
    generation: u64,
    uri: resources::Uri,
    interval: Duration,
    peer: rmcp::service::Peer<RoleServer>,
) {
    let _guard = Unregister {
        st: st.clone(),
        key: key.clone(),
        generation,
    };
    loop {
        tokio::time::sleep(interval).await;
        let next = match resources::version(&st, &uri).await {
            Ok(v) => v,
            Err(resources::PollError::Gone) => {
                // The repository no longer exists: the resource is gone, which
                // is the one thing `notifications/resources/list_changed`
                // means. This is where that capability earns its place — it is
                // never sent "just because".
                let _ = peer.notify_resource_list_changed().await;
                return;
            }
            Err(resources::PollError::Other(e)) => {
                tracing::debug!(uri = %key.2, error = %e, "resource poll failed; unsubscribing");
                return;
            }
        };
        let moved = {
            let mut subs = st.mcp.subs.lock();
            match subs.get_mut(&key) {
                Some(s) if s.generation == generation && s.version != next => {
                    s.version = next;
                    true
                }
                Some(_) => false,
                // Someone else removed it (unsubscribe, or a newer poller): stop.
                None => return,
            }
        };
        if moved
            && peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(key.2.clone()))
                .await
                .is_err()
        {
            // A failed send means the session is gone: this poller has nobody
            // to tell, so it stops and the guard cleans up.
            return;
        }
    }
}

/// Registry cleanup for one poller. `Drop` is sync, and the registry lock is a
/// `parking_lot` mutex, so no `await` is needed. The generation is what stops a
/// dying poller from deleting the entry that replaced it.
struct Unregister {
    st: Arc<AppState>,
    key: Key,
    generation: u64,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        let mut subs = self.st.mcp.subs.lock();
        if subs
            .get(&self.key)
            .is_some_and(|s| s.generation == self.generation)
        {
            subs.remove(&self.key);
        }
    }
}

/// What a model is told once, at `initialize`.
const INSTRUCTIONS: &str = "\
walgit is an object-store-backed git host. This endpoint serves its tools and
resources over MCP; the same facts are readable over HTTP (`GET /SKILL.md`
documents them for agents).

Rules that cost a wrong call if skipped:
* Bytes never travel over MCP. To move a repository, `git clone` its URL
  (`https://<host>/<owner>/<repo>.git`); this endpoint only reads and writes
  metadata.
* To write a collaboration entry (`collab_entry`, `collab_claim`) you must be
  the actor you name: the entry is signed with the key registered for it. The
  server refuses `actor != caller`.
* `collab_claim` before starting work on a card, re-read its result before you
  trust it: another agent may hold the card. A \"stand-down\" error means stop,
  not retry.
* Ownership of a card is carried by `status` entries, not comments.
* Subscriptions are polls (`resources/subscribe`): an update notification says
  `re-read`, never `here is the diff`.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_name_the_rules_a_first_write_would_break() {
        // The instructions are the one place a model learns that bytes do not
        // travel here and that a write must be its own actor; losing either
        // sentence would make the surface look like a normal MCP tool set.
        assert!(INSTRUCTIONS.contains("Bytes never travel over MCP"));
        assert!(INSTRUCTIONS.contains("actor != caller"));
        assert!(INSTRUCTIONS.contains("stand-down"));
        assert!(INSTRUCTIONS.contains("status"));
    }

    #[test]
    fn an_origin_must_be_this_servers_own_or_an_allowed_one() {
        // No Origin at all: a non-browser client, which has no origin to check.
        assert!(!same_origin("https://evil.example", Some("walgit.example")));
        assert!(same_origin("https://walgit.example", Some("walgit.example")));
        assert!(same_origin("https://walgit.example/path", Some("walgit.example")));
        // Only the authority is compared: `Host` carries no scheme, so a
        // scheme mismatch is invisible here — a deployment that must pin the
        // scheme does it in `server.cors_origins` (checked first, by
        // `web::v1::origin_allowed`). A different port is a different origin,
        // and so is a missing Host.
        assert!(!same_origin("https://walgit.example:8443", Some("walgit.example")));
        assert!(same_origin("https://walgit.example:8443", Some("walgit.example:8443")));
        assert!(!same_origin("https://walgit.example", None));
        assert!(!same_origin("null", Some("walgit.example")));
    }

    #[test]
    fn trusted_hosts_name_this_deployment_or_nothing() {
        let mut cfg = walgit_config::Config::default();
        // Default: loopback bind, no public_url — the set names the host.
        assert!(trusted_hosts(&cfg).contains(&"127.0.0.1".to_string()));
        cfg.server.public_url = Some("https://walgit.example.com:8443/x".to_string());
        assert!(trusted_hosts(&cfg).contains(&"walgit.example.com:8443".to_string()));
        // A wildcard bind with nothing named: rmcp's "any Host" (documented).
        cfg.server.public_url = None;
        cfg.server.listen = "0.0.0.0:8080".parse().unwrap();
        assert!(trusted_hosts(&cfg).is_empty(), "nothing is trusted without a public_url or a bound address");
        assert_eq!(authority_of("https://h/x"), Some("h".to_string()));
        assert_eq!(authority_of("https://"), None);
    }
}

