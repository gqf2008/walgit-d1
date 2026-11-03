pub mod api;
pub mod login;
pub mod objects;
pub mod trailers;
pub mod ui;
pub mod v1;

use std::fmt::Write as _;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::AppState;

/// Send a browser on `localhost` / `127.0.0.1` to `walgit.localhost` (same port).
/// Keep `/_auth/*` on the literal loopback host so an issuer's registered callback remains exact.
/// Git, curl, and the installer are not browsers and are not redirected.
pub async fn canonical_browser_host(
    State(st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let skip = path.starts_with("/_auth/")
        || path == "/healthz"
        || path == "/readyz"
        || path.starts_with("/services/public");
    let browser = is_browser(req.headers());
    let get = req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD;
    if get && browser && !skip
        && let Some(dest) = walgit_localhost_host(
            req.headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok()),
        ) {
            let scheme = if st.cfg.tls_enabled() {
                "https"
            } else {
                "http"
            };
            let pq = req
                .uri()
                .path_and_query()
                .map_or("/", http::uri::PathAndQuery::as_str);
            let loc = format!("{scheme}://{dest}{pq}");
            return (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response();
        }
    next.run(req).await
}

/// A browser navigating to a **page route** asks for the SPA shell: a static
/// document plus static assets, carrying no repository data, whose own reads go
/// through the JSON API and are gated like any other call.
///
/// Serving it without a credential is what makes a `token`-mode host with
/// `anonymous_read = false` usable from a browser at all: the page has to load
/// before it can put a token in `sessionStorage`, and a dead 401 at `/` is a
/// door with the handle on the inside (D68). Data lanes stay closed — the
/// browser gets the shell, never a repository.
fn shell_request(req: &Request<Body>) -> bool {
    if req.method() != axum::http::Method::GET {
        return false;
    }
    let path = req.uri().path();
    if is_shell_asset(path) {
        return true;
    }
    is_browser(req.headers()) && !is_data_lane(path)
}

/// Compiled artefacts of the shell (`/_ui/assets/…`, the icon). They contain no
/// data, and a browser fetching them sends `Accept: */*` or `text/css`, never
/// `text/html`.
fn is_shell_asset(path: &str) -> bool {
    path.starts_with("/_ui/") || path == "/favicon.ico" || path == "/index.html"
}

/// Paths that answer with data (or with a credential-shaped answer) even when
/// the caller looks like a browser. The shell rule must never reach these.
fn is_data_lane(path: &str) -> bool {
    if path.starts_with("/api/")
        || path.starts_with("/services/api/")
        || path.starts_with("/_auth/")
        || path.starts_with("/mcp/")
    {
        return true;
    }
    if matches!(path, "/mcp" | "/metrics") {
        return true;
    }
    // A repository's own surface, every lane: `/{o}/{r}/api…`, `…/api-browser…`.
    if path
        .split('/')
        .any(|seg| seg == "api" || seg == "api-browser")
    {
        return true;
    }
    // git smart HTTP
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
}

fn is_browser(headers: &axum::http::HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("text/html") {
        return true;
    }
    if headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("document") {
        return true;
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.contains("Mozilla"))
}

fn walgit_localhost_host(host: Option<&str>) -> Option<String> {
    let host = host?.trim();
    let (name, port) = match host.rsplit_once(':') {
        Some((n, p)) if !n.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        _ => (host, None),
    };
    let name = name.trim_matches(|c| c == '[' || c == ']');
    if !matches!(name, "localhost" | "127.0.0.1" | "::1") {
        return None;
    }
    Some(match port {
        Some(p) => format!("walgit.localhost:{p}"),
        None => "walgit.localhost".into(),
    })
}

/// Authentication gate for everything that is not a git endpoint or the login
/// flow itself: the SPA shell and assets, the installer and credential helper,
/// health and metrics. A browser without identity is sent through
/// `/_auth/login?next=…` when app-owned sign-in is configured; otherwise
/// 401 + `WWW-Authenticate: Bearer`.
pub async fn require_auth(
    State(st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // A browser navigating to a page route asks for the shell, not for data:
    // the shell is static, and the page's own reads are gated below. A host
    // with app-owned sign-in (`oidc`) keeps its redirect to `/_auth/login`: the
    // shell would replace the one path a browser has to a session, and the gate
    // offers a token instead.
    if !st.auth.browser_login_enabled() && shell_request(&req) {
        return next.run(req).await;
    }
    match st.auth.require_read(req.headers()).await {
        Ok(_) => next.run(req).await,
        Err(e) => {
            let accepts_html = req
                .headers()
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| a.contains("text/html"));
            let has_bearer = req.headers().contains_key(header::AUTHORIZATION);
            let is_get = req.method() == axum::http::Method::GET;
            if is_get && accepts_html && !has_bearer && st.auth.browser_login_enabled() {
                let next_url = req
                    .uri()
                    .path_and_query().map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
                let q = url_encode(&next_url);
                return Redirect::temporary(&format!("/_auth/login?next={q}")).into_response();
            }
            let status = match e {
                crate::auth::AuthError::Forbidden => StatusCode::FORBIDDEN,
                crate::auth::AuthError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::UNAUTHORIZED,
            };
            let why = match e {
                crate::auth::AuthError::Forbidden => {
                    "your account is not in an allowed domain".to_string()
                }
                crate::auth::AuthError::Unavailable => {
                    "the token verifier is temporarily unavailable; retry".to_string()
                }
                _ => match crate::setup::recipes(
                    &st.cfg,
                    &crate::smart::request_base_url(&st, req.headers()),
                    None,
                )
                .token_url
                {
                    Some(u) => format!(
                        "sign in, or send `Authorization: Bearer <token>` (create one at {u})"
                    ),
                    None => "send `Authorization: Bearer <token>`".to_string(),
                },
            };
            let mut resp =
                (status, format!("walgit: authentication required: {why}\n")).into_response();
            if status == StatusCode::UNAUTHORIZED {
                resp.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"walgit\""),
                );
            }
            resp
        }
    }
}

pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::walgit_localhost_host;

    #[test]
    fn localhost_becomes_walgit_localhost_same_port() {
        assert_eq!(
            walgit_localhost_host(Some("localhost:8080")).as_deref(),
            Some("walgit.localhost:8080"),
        );
        assert_eq!(
            walgit_localhost_host(Some("127.0.0.1:8080")).as_deref(),
            Some("walgit.localhost:8080"),
        );
        assert_eq!(walgit_localhost_host(Some("walgit.localhost:8080")), None);
        assert_eq!(walgit_localhost_host(Some("git.example.com")), None);
    }
}

#[cfg(test)]
mod shell_tests {
    use super::{is_data_lane, is_shell_asset, shell_request};

    fn req(method: &str, path: &str, accept: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(axum::http::header::ACCEPT, accept)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn a_page_navigation_gets_the_shell_and_a_data_path_never_does() {
        for path in ["/", "/repos", "/commands", "/acme/mono", "/acme/mono/tree/HEAD"] {
            assert!(
                shell_request(&req("GET", path, "text/html")),
                "{path} is a page route"
            );
        }
        for path in [
            "/api/v1/owners",
            "/api/v1/me",
            "/acme/mono/api-browser/refs",
            "/acme/mono/api/tree/HEAD",
            "/acme/mono.git/info/refs",
            "/acme/mono.git/git-upload-pack",
            "/mcp",
            "/metrics",
        ] {
            assert!(
                is_data_lane(path) && !shell_request(&req("GET", path, "text/html")),
                "{path} answers with data, never with the shell"
            );
        }
    }

    #[test]
    fn only_a_browser_navigation_counts_as_the_shell() {
        assert!(!shell_request(&req("GET", "/", "application/json")));
        assert!(!shell_request(&req("GET", "/repos", "*/*")));
        assert!(!shell_request(&req("POST", "/repos", "text/html")));
    }

    #[test]
    fn the_shell_assets_are_open_without_the_html_accept_header() {
        for path in ["/_ui/assets/index-abc.js", "/favicon.ico", "/index.html"] {
            assert!(is_shell_asset(path));
            assert!(shell_request(&req("GET", path, "*/*")), "{path}");
        }
    }
}
