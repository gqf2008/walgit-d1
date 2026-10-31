//! The `/mcp` tools' one dependency: walgit's own JSON API, called in-process.
//!
//! A tool call is not a second implementation of anything. It builds the HTTP
//! request the bundled UI (or `walgit repo …`) would build, hands it to this
//! process's own router through `tower::ServiceExt::oneshot`, and returns the
//! answer. That is deliberate (D64): placement (D30), the 503 for a repo this
//! host does not serve, freshness (every read revalidates), authz (`actor ==
//! caller`) and the response shapes are then *the same code*, not a second
//! contract that can drift. The caller's own credential travels on the inner
//! request, so an MCP client can never reach further than its bearer allows.
//!
//! `oneshot` enters the router directly — no socket, no TLS, no edge. The one
//! cost is that the API is reached through its own URL grammar, so paths and
//! query values must be encoded here.
//!
//! Every tool goes through [`Call`], the one write included (`collab_publish`
//! is a POST and nothing else): the API decides what a write means, so there is
//! no second path to keep in step with it.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::AppState;

/// A failed inner call: the API's status and its own message (never a
/// re-worded one — the message is what the model has to act on).
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status.as_u16())
    }
}

impl std::error::Error for ApiError {}

/// The API's failure *is* the JSON-RPC failure: its sentence is the one that
/// names the fix, so it travels unchanged.
impl From<ApiError> for rmcp::ErrorData {
    fn from(e: ApiError) -> Self {
        rmcp::ErrorData::internal_error(e.to_string(), None)
    }
}

/// One tool call's identity: the caller's headers, replayed on every inner
/// request.
pub struct Call<'a> {
    st: &'a Arc<AppState>,
    headers: HeaderMap,
}

impl<'a> Call<'a> {
    /// `principal` is the identity the outer `/mcp` request authenticated as.
    /// It is replayed as `X-Walgit-Principal`, which is the header the inner
    /// request's own `require_read`/`require_write` resolve the caller from:
    /// behind an edge or a push broker the credential in `Authorization` is the
    /// *hop's*, so without this the inner request runs as the forwarder (which
    /// is usually an admin) and `collab_publish`'s `actor == caller` compares
    /// against the wrong name.
    pub fn new(st: &'a Arc<AppState>, headers: &HeaderMap, principal: &str) -> Self {
        // Only what an inner request needs to be the *same* caller: the
        // credential, the session cookie (browser lane), and the host (absolute
        // URLs in recipes/overview). Everything else — Accept, encodings,
        // content-length, MCP's own headers — is set by this client.
        //
        // `user-agent` is deliberately NOT replayed: the browser host
        // canonicalisation redirects a browser UA on a loopback host, and a 302
        // is not an answer (D64).
        let mut kept = HeaderMap::new();
        for name in [header::AUTHORIZATION, header::COOKIE, header::HOST] {
            if let Some(v) = headers.get(&name) {
                kept.insert(name, v.clone());
            }
        }
        // Behind an edge the client's credential is not `Authorization` at all:
        // the hop owns that header and forwards the client's in
        // `X-Walgit-Authorization`, announcing it in `X-Walgit-Capabilities`
        // (D39). Both must travel together to the inner request — dropping them
        // made the inner `require_read` resolve the *hop's* principal (often
        // admin) and silently granted the caller its reads.
        for name in [
            crate::auth::FORWARDED_AUTHORIZATION_HEADER,
            crate::static_object::CAPABILITIES_HEADER,
        ] {
            let (Ok(header_name), Some(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                headers.get(name),
            ) else {
                continue;
            };
            kept.insert(header_name, value.clone());
        }
        // Set from the authenticated principal, never copied from the request:
        // the outer `require_read` already decided whether the caller may
        // forward an identity (only a `trusted_forwarders` principal may), so
        // this carries that verdict instead of re-openning the question.
        if let Ok(value) = axum::http::HeaderValue::from_str(principal) {
            kept.insert(
                axum::http::HeaderName::from_static(crate::auth::PRINCIPAL_HEADER),
                value,
            );
        }
        Self { st, headers: kept }
    }

    /// The router this instance serves: the exact one `lib.rs::router` builds,
    /// built once on the first tool call and kept on the state (a clone is
    /// cheap — the state is an `Arc`). Building it here rather than inside
    /// `router` keeps the state's handle out of the router's own construction.
    fn inner(&self) -> Router {
        self.st
            .inner_router
            .get_or_init(|| crate::router(self.st.clone()))
            .clone()
    }

    async fn send(&self, method: Method, path: &str) -> Result<(StatusCode, Vec<u8>), ApiError> {
        let mut builder = Request::builder().method(method).uri(path);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let req = builder
            .body(Body::empty())
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("building the inner request failed: {e}"),
            })?;
        let resp = self.inner().oneshot(req).await.map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("the inner request failed: {e}"),
        })?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes().to_vec())
            .unwrap_or_default();
        Ok((status, bytes))
    }

    /// `GET path` → JSON. The API's error body is `{"error": "…"}`; a non-JSON
    /// error page (an edge, a proxy) is passed through as text.
    pub async fn get(&self, path: &str) -> Result<Value, ApiError> {
        let (status, bytes) = self.send(Method::GET, path).await?;
        Self::decode(status, &bytes)
    }

    /// `POST path` with a JSON body. The body is small by construction (a
    /// collab entry, bounded by `COLLAB_ENTRY_MAX_BYTES`), so it never streams.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        let payload = serde_json::to_vec(body).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("encoding the inner request body failed: {e}"),
        })?;
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let req = builder.body(Body::from(payload)).map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("building the inner request failed: {e}"),
        })?;
        let resp = self.inner().oneshot(req).await.map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("the inner request failed: {e}"),
        })?;
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes().to_vec())
            .unwrap_or_default();
        Self::decode(status, &bytes)
    }

    fn decode(status: StatusCode, bytes: &[u8]) -> Result<Value, ApiError> {
        if status.is_success() {
            return serde_json::from_slice(bytes).map_err(|e| ApiError {
                status,
                message: format!("the API answered non-JSON ({e})"),
            });
        }
        let message = serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(std::string::ToString::to_string)
            })
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(bytes).trim().to_string();
                if text.is_empty() {
                    status.canonical_reason().unwrap_or("error").to_string()
                } else {
                    text
                }
            });
        Err(ApiError { status, message })
    }
}

/// Percent-encode a path segment or a query value (RFC 3986 unreserved set
/// kept, everything else as `%XX`). `/` is encoded here — callers join
/// segments themselves, so a repo name or a file path can never smuggle a
/// separator through.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Percent-encode a *path*: like [`enc`], but `/` stays a separator (the API's
/// wildcard routes take `tree/{sha}/{path}`).
pub fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

/// `?k=v&k2=v2`, skipping empty values so the API's defaults apply.
pub fn query(pairs: &[(&str, Option<String>)]) -> String {
    let mut parts = Vec::new();
    for (k, v) in pairs {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            parts.push(format!("{k}={}", enc(v)));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_keeps_structure_and_escapes_separators() {
        assert_eq!(enc("walgit v1"), "walgit%20v1");
        assert_eq!(enc("a/b"), "a%2Fb");
        assert_eq!(enc_path("main/src/lib.rs"), "main/src/lib.rs");
        assert_eq!(enc_path("weird path/x.rs"), "weird%20path/x.rs");
        assert_eq!(enc("bü"), "b%C3%BC");
    }

    #[test]
    fn query_skips_empty_values() {
        assert_eq!(query(&[("n", None), ("prefix", Some(String::new()))]), "");
        assert_eq!(
            query(&[("prefix", Some("feature/".into())), ("n", Some("10".into()))]),
            "?prefix=feature%2F&n=10"
        );
    }
}
