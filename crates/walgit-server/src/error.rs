//! HTTP error responses. Git-protocol user errors (non-fast-forward, bad ref)
//! are *not* mapped here: they are reported as `unpack`/`ng` pkt-lines inside a
//! 200 response per the smart HTTP contract. Only transport/auth/routing errors
//! become HTTP error statuses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    /// 401 on a browser-reachable lane (web api/v1/ui, settings, policy,
    /// admin, login): challenges `Bearer` only. A `Basic` challenge
    /// pops the browser's native password dialog on navigations and
    /// credentialed fetch/XHR (the SDK sign-in popup navigates to
    /// `/api-browser/v1/authenticate`) — `Basic` must never reach a surface
    /// a browser can navigate to (issue #91).
    Unauthorized,
    /// 401 on the git lane (smart HTTP, LFS, bundle downloads): challenges
    /// `Basic` first, `Bearer` second. git/libcurl holds credentials (URL
    /// userinfo, helpers) but only sends them after a 401 that offers Basic —
    /// a Bearer-only challenge leaves it without a scheme it implements, and
    /// the Basic password is interpreted as the token itself (issue #79).
    UnauthorizedGit,
    Forbidden,
    Conflict(String),
    PayloadTooLarge,
    UnsupportedMediaType(String),
    ServiceUnavailable(String),
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized | ApiError::UnauthorizedGit => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl ApiError {
    /// The plain-text body / SSE `error` packet message.
    pub fn message(&self) -> String {
        match self {
            ApiError::NotFound(m) => format!("not found: {m}"),
            ApiError::BadRequest(m) => format!("bad request: {m}"),
            ApiError::Unauthorized | ApiError::UnauthorizedGit => "unauthorized".to_string(),
            ApiError::Forbidden => "forbidden".to_string(),
            ApiError::Conflict(m) => format!("conflict: {m}"),
            ApiError::PayloadTooLarge => "payload too large".to_string(),
            ApiError::UnsupportedMediaType(m) => format!("unsupported media type: {m}"),
            ApiError::ServiceUnavailable(m) => format!("service unavailable: {m}"),
            ApiError::Internal(m) => format!("internal error: {m}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.message();
        let status = self.status();
        // Every 5xx is an operator-facing event: log it with its text (the
        // access log carries status only; a 500 whose reason lives solely in
        // the client's terminal is undebuggable).
        if status.is_server_error() {
            tracing::warn!(status = status.as_u16(), error = %msg, "request failed");
        }
        let mut resp = (status, msg).into_response();
        // RFC 6750: a 401 from a Bearer-protected resource MUST include
        // WWW-Authenticate. The challenge is channel-scoped (issue #91):
        // the git lane challenges `Basic` first — git holds credentials (URL
        // userinfo, helpers) but only sends them after a 401 that offers Basic;
        // a Bearer-only challenge leaves libcurl without a scheme it implements
        // (issue #79), and the Basic password is interpreted as the token
        // itself. Browser-reachable lanes challenge `Bearer` only: a Basic
        // challenge pops the browser's native password dialog on navigations
        // and credentialed fetch (the SDK sign-in popup navigates to a
        // browser-reachable 401) — the dialog sends the user's SSO password to
        // this host as a token and preempts the designed sign-in flow.
        if status == StatusCode::UNAUTHORIZED {
            let challenge = match self {
                ApiError::UnauthorizedGit => "Basic realm=\"walgit\", Bearer realm=\"walgit\"",
                _ => "Bearer realm=\"walgit\"",
            };
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static(challenge),
            );
        }
        // 503s are transient by contract (placement refusal during a fallback,
        // a store deadline, a warming copy): say when to come back.
        if status == StatusCode::SERVICE_UNAVAILABLE {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("15"),
            );
        }
        resp
    }
}

impl From<walgit_store::StoreError> for ApiError {
    fn from(e: walgit_store::StoreError) -> Self {
        match e {
            walgit_store::StoreError::NotFound { key } => ApiError::NotFound(key),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

/// The one auth-error mapping (web lane). `Unauthorized` here is the
/// Bearer-only challenge; the git lane (smart HTTP, LFS, bundle downloads)
/// wraps it with [`ApiError::git_lane`] for the Basic-first challenge.
impl From<crate::auth::AuthError> for ApiError {
    fn from(e: crate::auth::AuthError) -> Self {
        match e {
            crate::auth::AuthError::Invalid | crate::auth::AuthError::Unauthorized => {
                ApiError::Unauthorized
            }
            crate::auth::AuthError::Forbidden => ApiError::Forbidden,
            crate::auth::AuthError::Unavailable => {
                ApiError::ServiceUnavailable("auth provider unavailable".into())
            }
        }
    }
}

impl ApiError {
    /// The git lane's variant of a web-lane error: `Unauthorized` becomes
    /// `UnauthorizedGit` — the Basic-first challenge git needs (§1.3, #79/#91).
    #[must_use]
    pub fn git_lane(self) -> Self {
        match self {
            ApiError::Unauthorized => ApiError::UnauthorizedGit,
            other => other,
        }
    }
}
