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
    ///
    /// Every client-visible form of an `ApiError` goes through here (the HTTP
    /// body, the SSE `error` packet, and the `walgit: …` line a receive-pack
    /// refusal puts on the sideband for `git` to print). Task records and
    /// progress packets take the same [`redact_url_queries`] exit inside
    /// `walgit-wal`, so no client-visible text carries a signed URL's query
    /// string; scheme+host+path are kept (they name the object without
    /// granting it).
    pub fn message(&self) -> String {
        redact_url_queries(&self.raw_message())
    }

    /// The message before URL redaction. Logs deliberately go through
    /// [`Self::message`] too: a signed URL is not worth keeping in a log line,
    /// and the host/path/status is all an operator needs to find the failure.
    fn raw_message(&self) -> String {
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

/// One redaction for every client-visible form of an error (the HTTP body,
/// the SSE `error` packet, a task record and the `walgit: …` line a
/// receive-pack refusal puts on the sideband): scheme+host+path survive,
/// the query string (`X-Amz-Credential` / `X-Amz-Signature` / `x-id=…`) does
/// not. Lives in `walgit-wal` because task records and progress packets are
/// built there; the server shares the same implementation.
pub(crate) use walgit_wal::redact_url_queries;

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
            // Transport error, timeout, upstream 5xx, throttling: the store
            // layer already classified it as retryable (cc-ai-store-503). A
            // 500 tells a git client "do not retry" and a browser/SDK the
            // same, so a transient bucket failure must be 503 + Retry-After
            // (the responder adds the header). Everything else keeps the
            // existing 500 mapping and the 4xx semantics are untouched.
            other if other.is_retryable() => {
                ApiError::ServiceUnavailable(format!("object store: {other}"))
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

/// The one WAL → HTTP mapping. `WalError` wraps store errors in two shapes
/// (`Store` directly and `CoordError::Store` from the CAS/read helpers) and the
/// server used to classify only the first, so a failed manifest read — which is
/// a coordinated read — fell into the `wal: {e}` 500 catch-all. Transient
/// failures now answer 503 + Retry-After on every lane (git smart HTTP and the
/// JSON API).
/// `TooLarge` is *not* transient (re-pulling cannot shrink the pack set), so it
/// keeps the 500 mapping; the smart lanes still answer it with the bundle-uri
/// pkt ERR where the protocol allows a readable remedy.
impl From<&walgit_wal::WalError> for ApiError {
    fn from(e: &walgit_wal::WalError) -> Self {
        match e {
            walgit_wal::WalError::NotFound => ApiError::NotFound(e.to_string()),
            other if other.is_retryable() => {
                ApiError::ServiceUnavailable(format!("object store: {other}"))
            }
            _ => ApiError::Internal(format!("wal: {e}")),
        }
    }
}

/// Bundle reads are store reads underneath (`BundleError::Store`): a
/// transient bucket failure on a bundle list/URI must be the same 503 class as
/// every other lane. Every other variant keeps the existing 500 mapping.
impl From<&walgit_bundle::BundleError> for ApiError {
    fn from(e: &walgit_bundle::BundleError) -> Self {
        match e {
            walgit_bundle::BundleError::Store(se) if se.is_retryable() => {
                ApiError::ServiceUnavailable(format!("object store: {e}"))
            }
            _ => ApiError::Internal(format!("bundle: {e}")),
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

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::http::header::RETRY_AFTER;
    use axum::response::IntoResponse;

    use super::{ApiError, redact_url_queries};

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Incident cc-ai-store-503: a store transport error used to be a 500, and
    /// git clients treat 500 as permanent. The store already says "retryable";
    /// the HTTP lane must say 503 and tell the client when to come back.
    #[tokio::test]
    async fn retryable_store_error_is_503_with_retry_after() {
        let err: ApiError =
            walgit_store::StoreError::retryable(anyhow::anyhow!("connection reset")).into();
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        let resp = err.into_response();
        assert_eq!(resp.headers()[RETRY_AFTER], "15");
        let body = body_text(resp).await;
        assert!(body.contains("service unavailable"), "{body}");
        assert!(body.contains("connection reset"), "{body}");
    }

    /// The incident's actual shape: the manifest read is a coordinated read, so
    /// the store error arrives as `WalError::Coord(CoordError::Store(..))` —
    /// the branch the old `wal_err` missed.
    #[tokio::test]
    async fn coordinated_store_error_is_503() {
        let wal = walgit_wal::WalError::Coord(walgit_store::coord::CoordError::Store(
            walgit_store::StoreError::retryable(anyhow::anyhow!("upstream 503")),
        ));
        let err = ApiError::from(&wal);
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.into_response().headers()[RETRY_AFTER], "15");
    }

    /// Non-retryable store failures keep the 500 they had (and carry no
    /// Retry-After, which would invite a pointless retry).
    #[tokio::test]
    async fn non_retryable_store_error_keeps_500() {
        let err: ApiError = walgit_store::StoreError::other(anyhow::anyhow!("denied")).into();
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !err.into_response().headers().contains_key(RETRY_AFTER),
            "a permanent failure must not ask for a retry"
        );
    }

    /// 404 keeps its meaning: a missing object is not a transient failure.
    #[tokio::test]
    async fn store_not_found_stays_404() {
        let err: ApiError = walgit_store::StoreError::NotFound {
            key: "repos/o/r/manifest.pb".into(),
        }
        .into();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    /// The presigned URL a store failure quotes carries the signature; the
    /// client sees scheme+host+path only, and nothing in the body reintroduces
    /// the original text.
    #[tokio::test]
    async fn client_visible_body_redacts_signed_url_queries() {
        let signed = "https://acct.r2.cloudflarestorage.com/bucket/repos/o/r/manifest.pb\
                      ?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                      &X-Amz-Credential=AKIAEXAMPLE%2F20261106\
                      &X-Amz-Signature=deadbeef\
                      &x-id=GetObject";
        let err = ApiError::ServiceUnavailable(format!(
            "s3 get http: error sending request for url ({signed})"
        ));
        let body = body_text(err.into_response()).await;
        assert!(!body.contains("X-Amz-"), "leaked signing params: {body}");
        assert!(!body.contains("x-id="), "leaked request params: {body}");
        assert!(!body.contains("deadbeef"), "leaked signature: {body}");
        assert!(
            body.contains("https://acct.r2.cloudflarestorage.com/bucket/repos/o/r/manifest.pb"),
            "scheme+host+path must survive: {body}"
        );

        // The same guarantee holds for a permanent error (the 500 body is just
        // as client-visible).
        let err = ApiError::Internal(format!("s3 put error: {signed}"));
        let body = body_text(err.into_response()).await;
        assert!(
            !body.contains("X-Amz-") && !body.contains("x-id="),
            "{body}"
        );
    }

    /// Every branch of the two classifiers, including the coordination
    /// wrapper the incident came through and the deliberately non-retryable
    /// `TooLarge` (500, no Retry-After).
    #[test]
    fn wal_error_classification_branches() {
        use walgit_store::StoreError;
        use walgit_store::coord::CoordError;
        use walgit_wal::WalError;

        let status = |e: &WalError| ApiError::from(e).status();
        assert_eq!(
            status(&WalError::Store(StoreError::retryable(anyhow::anyhow!(
                "io"
            )))),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&WalError::Coord(CoordError::Store(StoreError::retryable(
                anyhow::anyhow!("io")
            )))),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&WalError::Coord(CoordError::RetriesExhausted {
                key: "manifest.pb".into(),
                attempts: 3
            })),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&WalError::Retry { attempts: 3 }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&WalError::Reclaiming("checksum".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&WalError::CommitUnknown("lost reply".into())),
            StatusCode::SERVICE_UNAVAILABLE,
            "an unknown commit outcome is retried, never read as a rejection"
        );
        assert_eq!(status(&WalError::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(
            status(&WalError::Store(StoreError::other(anyhow::anyhow!(
                "denied"
            )))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status(&WalError::Store(StoreError::InvalidArgument("bad".into()))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status(&WalError::TooLarge { bytes: 10, max: 1 }),
            StatusCode::INTERNAL_SERVER_ERROR,
            "TooLarge is not transient: a retry cannot shrink the pack set"
        );
    }

    /// A bundle read is a store read underneath: its retryable class is 503,
    /// every other variant keeps the 500 it had.
    #[test]
    fn bundle_error_classification() {
        use walgit_bundle::BundleError;
        use walgit_store::StoreError;

        assert_eq!(
            ApiError::from(&BundleError::Store(StoreError::retryable(anyhow::anyhow!(
                "timeout"
            ))))
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::from(&BundleError::Store(StoreError::other(anyhow::anyhow!(
                "denied"
            ))))
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::from(&BundleError::NoRefs).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn redaction_touches_only_url_queries() {
        // A question mark in prose is not a query string.
        assert_eq!(
            redact_url_queries("what? nothing to see"),
            "what? nothing to see"
        );
        // A URL without a query is unchanged.
        assert_eq!(
            redact_url_queries("see https://host/a/b.git for details"),
            "see https://host/a/b.git for details"
        );
        // Multiple URLs in one message.
        assert_eq!(
            redact_url_queries("a https://h/1?X-Amz-Signature=a b https://h/2?x=1 c"),
            "a https://h/1 b https://h/2 c"
        );
        // Fragments go too, and quotes terminate a URL token.
        assert_eq!(
            redact_url_queries("url \"https://h/p?a=1#frag\" tail"),
            "url \"https://h/p\" tail"
        );
    }
}
