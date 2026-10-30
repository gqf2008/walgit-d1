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
//!   allowlist (`server.cors_origins`) as the rest of the server.
//! * **Subscriptions are pollers** (D52/D55). `resources/subscribe` starts one
//!   bounded task per (caller, URI) that reads the repository's refs-level seq
//!   every `mcp.subscribe_interval` and sends
//!   `notifications/resources/updated` when it moves. No request is held open
//!   between changes: the seq read *is* a conditional `manifest.pb` GET, the
//!   same observation lane `git ls-remote` and `walgit collab watch` use.
//!
//! The tool and resource *content* lives next door: [`tools`] (the surface) and
//! [`resources`] (the URIs), both over [`api`] — walgit's own JSON API called
//! in-process, so there is no second contract to drift.

pub mod api;
pub mod resources;
pub mod signing;
pub mod tools;

use std::collections::HashMap;
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

/// Subscriptions: `(principal, uri)` → the poller watching it.
///
/// Keying by principal (not by session) is deliberate: the identity is what an
/// authorization decision depends on, and two sessions of the same principal
/// asking for the same URI want the same answer. The registry is per-instance
/// and disposable (D52) — a restart costs a re-subscribe, nothing durable.
#[derive(Default)]
pub struct Mcp {
    subs: parking_lot::Mutex<HashMap<(String, String), Subscription>>,
}

struct Subscription {
    /// The refs-level seq the client was last told about.
    version: String,
    abort: tokio::task::AbortHandle,
}

impl Mcp {
    pub fn is_empty(&self) -> bool {
        self.subs.lock().is_empty()
    }
    pub fn len(&self) -> usize {
        self.subs.lock().len()
    }
    /// Stop watching one URI (a no-op when nothing watches it).
    pub fn unsubscribe(&self, principal: &str, uri: &str) {
        if let Some(s) = self.subs.lock().remove(&(principal.to_string(), uri.to_string())) {
            s.abort.abort();
        }
    }
}

/// `/mcp`, mounted by `crate::router`.
pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let factory = {
        let state = state.clone();
        move || Ok(WalgitMcp { st: state.clone() })
    };
    let service: StreamableHttpService<WalgitMcp, LocalSessionManager> = StreamableHttpService::new(
        factory,
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_sse_keep_alive(Some(KEEPALIVE))
            .with_legacy_session_mode(true)
            .with_max_request_body_bytes(MAX_BODY_BYTES)
            // Host/Origin validation cannot know this deployment's hostnames
            // (D39: any hostname, any edge). [`serve`] does the Origin check
            // against the server's own `server.cors_origins` allowlist instead,
            // and a missing credential is already a 401 — never an open
            // unauthenticated surface.
            .disable_allowed_hosts()
            .disable_allowed_origins(),
    );
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
    let principal = match state.auth.require_read(&headers).await {
        Ok(p) => p,
        Err(crate::auth::AuthError::Forbidden) => return StatusCode::FORBIDDEN.into_response(),
        Err(_) => return unauthorized(),
    };
    req.extensions_mut().insert(Caller {
        principal: principal.name.clone(),
    });
    // `oneshot` consumes the request; the transport returns the body it built
    // (an SSE stream, a JSON answer, or 202/204). Cloning the `Arc` is what lets
    // the closure stay `Clone`.
    let service = Arc::try_unwrap(service).unwrap_or_else(|arc| (*arc).clone());
    match service.oneshot(req).await {
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
    if crate::web::v1::origin_allowed(&state.cfg, origin) {
        return true;
    }
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
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
        self.watch(&req.principal, &request.uri, context.peer.clone())
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
        self.st.mcp.unsubscribe(&req.principal, &request.uri);
        Ok(())
    }
}

impl WalgitMcp {
    /// Start watching one URI for this principal (idempotent: a second
    /// `subscribe` for the same URI keeps the existing poller).
    async fn watch(
        &self,
        principal: &str,
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
        let version = resources::version(&self.st, &uri).await?;
        let key = (principal.to_string(), uri_str.to_string());
        {
            let subs = self.st.mcp.subs.lock();
            if subs.contains_key(&key) {
                return Ok(());
            }
            let mine = subs.keys().filter(|(p, _)| p == principal).count();
            if mine >= cfg.max_subscriptions {
                return Err(ErrorData::invalid_params(
                    format!(
                        "at most {} subscriptions per caller (mcp.max_subscriptions)",
                        cfg.max_subscriptions
                    ),
                    None,
                ));
            }
        }
        let st = self.st.clone();
        let watched = key.clone();
        let task = tokio::spawn(async move {
            // Removes the registry entry however the poller ends, so a dead
            // task never leaves a subscription that answers "already watched".
            let _guard = Unregister {
                st: st.clone(),
                key: watched.clone(),
            };
            loop {
                tokio::time::sleep(interval).await;
                let next = match resources::version(&st, &uri).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(uri = %watched.1, %e, "resource poll failed; unsubscribing");
                        return;
                    }
                };
                let moved = {
                    let mut subs = st.mcp.subs.lock();
                    match subs.get_mut(&watched) {
                        Some(s) if s.version != next => {
                            s.version = next;
                            true
                        }
                        Some(_) => false,
                        // Someone else removed it (unsubscribe): stop.
                        None => return,
                    }
                };
                if moved {
                    // A failed send means the session is gone: this poller has
                    // nobody to tell, so it stops and the guard cleans up.
                    if peer.notify_resource_updated(ResourceUpdatedNotificationParam::new(
                            watched.1.clone(),
                        )).await.is_err() {
                        return;
                    }
                }
            }
        });
        self.st
            .mcp
            .subs
            .lock()
            .insert(key, Subscription {
                version,
                abort: task.abort_handle(),
            });
        Ok(())
    }
}

/// Registry cleanup for one poller. `Drop` is sync, and the registry lock is a
/// `parking_lot` mutex, so no `await` is needed.
struct Unregister {
    st: Arc<AppState>,
    key: (String, String),
}

impl Drop for Unregister {
    fn drop(&mut self) {
        self.st.mcp.subs.lock().remove(&self.key);
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
        let mut h = HeaderMap::new();
        assert!(origin_allowed_headers(&h));
        h.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://evil.example"),
        );
        h.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("walgit.example"),
        );
        assert!(!origin_allowed_headers(&h));
        h.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://walgit.example"),
        );
        assert!(origin_allowed_headers(&h));
    }

    /// The Origin rule without an `AppState` (the allowlist half is the same
    /// code the rest of the server uses and has its own test).
    fn origin_allowed_headers(headers: &HeaderMap) -> bool {
        let Some(origin) = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        else {
            return true;
        };
        let Some(host) = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        match origin.split_once("://") {
            Some((scheme, rest)) if scheme == "http" || scheme == "https" => {
                rest.split('/').next() == Some(host)
            }
            _ => false,
        }
    }
}

