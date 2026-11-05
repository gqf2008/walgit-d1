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
//!   (never `Basic` — `/mcp` is browser-reachable, docs/ARCHITECTURE.md §1.3), and the
//!   identity that lands in the handler is the one `Auth::require_read` named.
//!   The request's own headers are stuffed into a [`Caller`] extension so each
//!   tool call can replay them on the inner API request it makes
//!   ([`super::api`]) — a client can never exceed its own bearer.
//! * **CORS/Origin** — a foreign `Origin` is refused here, using the same
//!   allowlist (`server.cors_origins`) as the rest of the server, and `OPTIONS`
//!   is answered here too (the SDK transport registers no preflight, and a
//!   browser client must not be asked for a credential before it may ask).
//! * **Host validation** — `Host` must name this deployment: `localhost`, any
//!   `*.localhost` name, a loopback address, the `server.public_url` authority
//!   or the bound listen address (see [`host_allowed`]). This is the
//!   DNS-rebinding guard, and a name never claimed by this deployment is
//!   refused. rmcp's own matcher is off because it compares **exact** names:
//!   it refused `walgit.localhost`, the name `walgit.standalone.toml` tells
//!   people to open.
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
mod subscriptions;
mod tools;

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
        ReadResourceResult, ServerCapabilities, ServerConfig,
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

/// The subscription registry lives in [`subscriptions`] (the pollers go with
/// it); re-exported here because `AppState` owns one.
pub use subscriptions::Mcp;

/// The session header rmcp's Streamable HTTP transport uses.
const SESSION_HEADER: &str = "mcp-session-id";

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
        // Host validation is [`host_allowed`]'s, not rmcp's: the SDK's matcher
        // takes exact names and would reject `walgit.localhost` (a name that
        // always resolves to loopback, RFC 6761 §6.3) and every `127.x` alias.
        .disable_allowed_hosts();
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

/// Whether the request's `Host` names this deployment (DNS-rebinding guard).
///
/// Accepted: `localhost`, any `*.localhost` name (RFC 6761 §6.3 — such a name
/// resolves to loopback by definition, so it cannot be rebound), any loopback
/// address, the authority `server.public_url` advertises, and the bound listen
/// IP. A deployment that named nothing (no `public_url`, a wildcard bind) is
/// permissive, because nothing is being spoofed by a name it never claimed.
///
/// The port is ignored on both sides: a `Host` header carries one, the
/// configured authority may not, and a port alone never survives a rebind.
fn host_allowed(cfg: &walgit_config::Config, host: Option<&str>) -> bool {
    let Some(host) = host else {
        // No `Host`/`:authority` at all (HTTP/1.0, a hand-rolled client): a
        // browser always sends one, so there is nothing to rebind.
        return true;
    };
    let name = host_name(host);
    if name.is_empty() {
        return false;
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = name.parse::<std::net::IpAddr>()
        && is_loopback(ip)
    {
        return true;
    }
    if let Some(public_url) = &cfg.server.public_url
        && let Some(authority) = authority_of(public_url)
        && host_name(&authority) == name
    {
        return true;
    }
    let listen = cfg.server.listen;
    if !listen.ip().is_unspecified() && host_name(&listen.ip().to_string()) == name {
        return true;
    }
    cfg.server.public_url.is_none() && listen.ip().is_unspecified()
}

/// The host part of a `Host` header or URL authority: no port, no brackets,
/// lowercased. An IPv6 literal is checked *before* the port split — `::1` and
/// `[::1]:8080` both carry colons, and cutting at the first one would turn a
/// configured IPv6 bind into the empty name, i.e. a guard that matches nothing.
fn host_name(host: &str) -> String {
    let host = host.trim();
    let inner = host.strip_prefix('[').map_or(host, |rest| {
        rest.split_once(']').map_or(rest, |(inside, _)| inside)
    });
    let name = if inner.parse::<std::net::IpAddr>().is_ok() {
        inner
    } else {
        inner.split(':').next().unwrap_or(inner)
    };
    name.to_ascii_lowercase()
}

fn is_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4().is_some_and(|v4| v4.is_loopback()),
    }
}

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
    // `OPTIONS` never reaches here: `crate::web::v1`'s CORS layer answers every
    // preflight for `is_cors_api_path` (which `/mcp` is part of, D23/D64). One
    // policy, one implementation — a second copy here was a divergence waiting
    // to happen.
    let session = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !origin_allowed(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // DNS-rebinding guard. rmcp's own matcher is off (see `router`): it compares
    // exact names, so `walgit.localhost` — the name `walgit.standalone.toml`
    // tells people to open — was refused unless `server.public_url` happened to
    // name it. `host_allowed` accepts every name that can only mean this
    // machine and, when nothing is advertised, stays permissive like the SDK was.
    // HTTP/2 clients may send `:authority` and no `Host` header; the SDK folded
    // both into `req.uri()`'s authority, so read that before giving up.
    let host_owned = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
    let host = host_owned.as_deref();
    if !host_allowed(&state.cfg, host) {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "this host does not answer to `{}`; set `server.public_url` to the name clients use",
                host.unwrap_or("")
            ),
        )
            .into_response();
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
            if is_delete {
                if resp.status() == StatusCode::ACCEPTED {
                    *resp.status_mut() = StatusCode::NO_CONTENT;
                }
                // A terminated session's pollers have nobody to notify and each
                // one costs a bucket read per interval: drop them with it (the
                // registry key carries the session id for exactly this).
                if let Some(session) = session {
                    state.mcp.forget_session(&session);
                }
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

/// The HTTP request an SDK handler was called for: headers to replay, the
/// principal to act as, and the MCP session (the default `session` a
/// a client may record in a `card_claim` entry).
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
        let items = tools::defs()
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
        match tools::call(&self.st, &req.headers, &req.principal, &request.name, &args).await
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
        let contents =
            resources::read(&self.st, &req.headers, &req.principal, &request.uri).await?;
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
        subscriptions::watch(
            &self.st,
            &req.headers,
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

const INSTRUCTIONS: &str = "\
walgit is an object-store-backed git host. This endpoint serves its tools and
resources over MCP; the same facts are readable over HTTP (`GET /SKILL.md`
documents them for agents).

Rules that cost a wrong call if skipped:
* Bytes never travel over MCP. To move a repository, `git clone` its URL
  (`https://<host>/<owner>/<repo>.git`); this endpoint only reads and writes
  metadata.
* `collab_publish` takes an entry you have already signed with your own key
  (`walgit collab entry` builds and signs one) and refuses `actor` != the
  authenticated caller; it never signs for you and holds no key. The signature
  is checked when entries are aggregated, so one signed with a key that is not
  registered for its `actor` is accepted here and then never counts — publish
  as the principal you actually hold.
* Register your public key once (`collab_register`, or `walgit collab join`):
  an entry signed with a key the repository does not name never verifies.
* Claiming a card is three steps, in this order: read `collab_board` (is it
  open, and does it already carry a live claim?), publish a `card_claim` entry
  whose body is `{ttl?, worktree?, branch?, work?, session?}`, then re-read the
  board. Two agents may publish a claim for the same card — duplicate claims are
  legal, duplicate *execution* is not — so the board's `owner` after your
  publish is the verdict: if it names someone else with a live claim, stand
  down and do not start work. A claim expires on its own (TTL), so a crashed
  agent does not hold a card forever.
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
        assert!(INSTRUCTIONS.contains("already signed"));
        assert!(INSTRUCTIONS.contains("holds no key"));
        assert!(INSTRUCTIONS.contains("stand"));
        assert!(INSTRUCTIONS.contains("collab_register"));
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
    fn the_host_header_must_name_this_deployment() {
        let mut cfg = walgit_config::Config::default();
        // Default: loopback bind, no public_url — every loopback name is this
        // machine, whatever the port (a `Host` header always carries one).
        for host in [
            "localhost",
            "localhost:8081",
            "walgit.localhost:8081",
            "127.0.0.1:8081",
            "127.0.0.2",
            "[::1]:8081",
            "LOCALHOST:8081",
        ] {
            assert!(host_allowed(&cfg, Some(host)), "{host} should be allowed");
        }
        for host in ["walgit.example.com", "10.0.0.5", "[2001:db8::1]:80", ""] {
            assert!(!host_allowed(&cfg, Some(host)), "{host} must be refused");
        }
        // A name that was never claimed is not trusted either.
        assert!(!host_allowed(&cfg, Some("walgit.internal:8081")));
        // Advertise it and it is.
        cfg.server.public_url = Some("https://walgit.internal:8081/".to_string());
        assert!(host_allowed(&cfg, Some("walgit.internal")));
        assert!(host_allowed(&cfg, Some("walgit.internal:8081")));
        assert!(host_allowed(&cfg, Some("localhost:8081")));
        // A wildcard bind with nothing named: rmcp's permissive default.
        cfg.server.public_url = None;
        cfg.server.listen = "0.0.0.0:8080".parse().unwrap();
        assert!(host_allowed(&cfg, Some("anything.example")));
        // No header at all is not a browser.
        assert!(host_allowed(&cfg, None));
        assert_eq!(authority_of("https://h/x"), Some("h".to_string()));
        assert_eq!(authority_of("https://"), None);
    }
}
