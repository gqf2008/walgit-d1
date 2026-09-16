//! S3-compatible backend (rustfs for local dev / CI).
//!
//! Uses `aws-sdk-s3` for all operations. GET responses are streamed via
//! presigned URLs + `reqwest` because the SDK's `GetObjectOutput::body()`
//! returns `&ByteStream` with no owned-body extractor. All other operations
//! (PUT, HEAD, DELETE, LIST) use the SDK directly.
//!
//! ## Version tokens
//!
//! Every PUT stores a fresh `walgit-incarnation` user-metadata token and
//! returns `<etag>@<incarnation>`. `Version::http_etag()` exposes the bare
//! `ETag` for HTTP and S3 conditional headers; full equality includes the
//! incarnation, so identical bytes re-created under the same key compare
//! unequal. Objects written before this metadata existed fall back to
//! `<etag>@lm:<Last-Modified>`. Quotes are stripped consistently on read.
//!
//! ## Conditional PUT
//!
//! `PutMode::Create`    → `If-None-Match: *`  (object must not exist).
//! `PutMode::Update(v)` → `If-Match: <etag>`  (CAS on the current bare `ETag`).
//! On failure the SDK returns a `PreconditionFailed` service error; we fill
//! `current` via a follow-up HEAD when the SDK doesn't include it. `Update`
//! intentionally speaks the bare `ETag`: user metadata cannot participate in an
//! S3 `If-Match`, so the full-incarnation guard lives on the delete path that
//! GC actually needs.
//!
//! ## Conditional DELETE
//!
//! We HEAD (read the full incarnation), compare, then DELETE with
//! `If-Match: <etag>`. The HEAD comparison rejects a stale version even when a
//! re-created object has identical bytes; the DELETE guard rejects a different
//! `ETag` racing in after that HEAD. What remains is the narrow
//! same-ETag-after-HEAD window, bounded by walgit's lease/claim fences.
//!
//! ## Multipart upload
//!
//! Objects above `cfg.multipart_threshold` use `CreateMultipartUpload` +
//! `UploadPart` + `CompleteMultipartUpload`. `CreateMultipartUpload` does NOT
//! support `If-None-Match`/`If-Match` in the S3 API:
//!
//! * `PutMode::Overwrite` (bundle lists, caches) gets plain multipart.
//! * `PutMode::Create` gets a `HEAD` pre-check plus multipart. The pre-check
//!   keeps the common "already exists" answer, but the check and the upload
//!   are not atomic, so a concurrent create can win the key. That is safe for
//!   walgit's large immutable objects — packs, `.idx`/`.rev`/`.bitmap`/
//!   `.commit-graph` side files are content-addressed, so racing writers put
//!   identical bytes. Single-shot PUT cannot be the answer above the
//!   threshold: S3 caps one `PutObject` at 5 GiB while a tier-2 base may be
//!   tens of GiB, and a slow uplink exceeds the request timeout long before
//!   that.
//! * `PutMode::Update` (CAS) stays single-shot and conditional; every
//!   CAS-rewritten object (manifests, leases) is small.
//!
//! A multipart upload whose future is dropped (task abort, drain) is aborted
//! best-effort by a drop guard. A hard kill cannot run it, so buckets should
//! also carry an `AbortIncompleteMultipartUpload` lifecycle rule (a few days)
//! to reclaim the parts of an interrupted large pack upload.
//!
//! ## rustfs compatibility (tested with rustfs/rustfs:latest)
//!
//! See the compatibility notes at the bottom of this file.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream as S3ByteStream;
use aws_smithy_types::DateTime;
use bytes::Bytes;
use futures::StreamExt;

/// S3 user metadata carrying a fresh incarnation token on every PUT. The
/// token is not derivable from the content: identical bytes re-created under
/// the same key must compare unequal for conditional delete.
const INCARNATION_META: &str = "walgit-incarnation";

fn new_incarnation() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn metadata_incarnation(metadata: Option<&HashMap<String, String>>) -> Option<&str> {
    metadata
        .and_then(|m| m.get(INCARNATION_META))
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

fn version_from_parts(
    etag: Option<&str>,
    incarnation: Option<&str>,
    last_modified: Option<&DateTime>,
) -> Version {
    let etag = etag.unwrap_or("").trim_matches('"');
    if let Some(incarnation) = incarnation.filter(|v| !v.is_empty()) {
        return Version::new(format!("{etag}@{incarnation}"));
    }
    // Objects written before incarnation metadata existed still need a stable
    // version. Last-Modified is only a fallback; new writes always carry the
    // unforgeable metadata token.
    let last_modified = last_modified
        .map(|dt| {
            dt.fmt(aws_smithy_types::date_time::Format::HttpDate)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if last_modified.is_empty() {
        Version::new(etag)
    } else {
        Version::new(format!("{etag}@lm:{last_modified}"))
    }
}

use crate::{
    BoxStream, GetOptions, GetResult, ObjectMeta, ObjectStore, PutBody, PutMode, PutOptions,
    Result, StoreError, Version, util,
};

/// S3-compatible object store.
#[derive(Clone)]
pub struct S3Store {
    client: S3Client,
    bucket: String,
    /// reqwest client for streaming GETs via presigned URLs.
    http: reqwest::Client,
    /// #130: bound on *silence*, not total time — no response head within this
    /// on a presigned GET, and no bytes for this long mid-body (a 30 GiB range
    /// read streams fine; a dropped tunnel errors as Retryable).
    idle_timeout: Duration,
    multipart_threshold: u64,
    multipart_part_size: u64,
}

impl S3Store {
    /// Build a store from `walgit-config::StoreConfig`.
    ///
    /// Literal `access_key`/`secret_key` (D43, written by the setup wizard)
    /// take precedence; otherwise credentials are read from the env vars named
    /// in `cfg.s3.access_key_env` / `cfg.s3.secret_key_env`
    /// (defaults `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), plus
    /// `AWS_SESSION_TOKEN` when present.
    pub fn new(cfg: &walgit_config::StoreConfig) -> anyhow::Result<Self> {
        let access_key = match cfg.s3.access_key.as_deref().filter(|v| !v.is_empty()) {
            Some(k) => k.to_string(),
            None => std::env::var(&cfg.s3.access_key_env).map_err(|_| {
                anyhow::anyhow!("s3: env var {} not set (access key)", cfg.s3.access_key_env)
            })?,
        };
        let secret_key = match cfg.s3.secret_key.as_deref().filter(|v| !v.is_empty()) {
            Some(k) => k.to_string(),
            None => std::env::var(&cfg.s3.secret_key_env).map_err(|_| {
                anyhow::anyhow!("s3: env var {} not set (secret key)", cfg.s3.secret_key_env)
            })?,
        };

        let creds = static_credentials(
            &access_key,
            &secret_key,
            std::env::var("AWS_SESSION_TOKEN").ok(),
        );
        let region = aws_sdk_s3::config::Region::new(cfg.s3.region.clone());

        // #130: socket bounds on the SDK lane (HEAD/PUT/multipart/LIST — every
        // one of them rides `client`, and every one has wedged a process at
        // 15.5 h of sleep on a dead connection). `connect_timeout` caps TCP+TLS
        // connect; `read_timeout` caps request→response-head *per attempt*
        // (verified in aws-smithy-http-client 1.2.0: it wraps the connector's
        // call future, not the body) — so it bounds control calls and bounded
        // part uploads (≤ `multipart_part_size`) without touching streamed
        // reads, which leave the SDK entirely: GETs go presigned through
        // `http` below, with their own connect + idle bounds. The SDK's own
        // retry policy (attempt cap 3) still applies; the store-level
        // `max_retries` backoff layers on top.
        let sdk_timeout = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
            .connect_timeout(cfg.connect_timeout)
            .read_timeout(cfg.idle_timeout)
            .build();

        let mut s3_config = aws_sdk_s3::Config::builder()
            .region(region)
            .credentials_provider(creds)
            .force_path_style(cfg.s3.force_path_style)
            .behavior_version_latest()
            .timeout_config(sdk_timeout);

        if !cfg.s3.endpoint.is_empty() {
            s3_config = s3_config.endpoint_url(&cfg.s3.endpoint);
        }

        let client = S3Client::from_conf(s3_config.build());
        // The presigned-GET lane: connect capped by `connect_timeout`, body
        // liveness enforced per chunk below (reqwest has no read-idle
        // timeout, and `Client::timeout` is a *total* deadline — the one
        // thing a 24-minute clone must not inherit, §2.3).
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .build()?;

        Ok(S3Store {
            client,
            bucket: cfg.bucket.clone(),
            http,
            idle_timeout: cfg.idle_timeout,
            multipart_threshold: cfg.multipart_threshold.as_u64(),
            multipart_part_size: cfg.multipart_part_size.as_u64(),
        })
    }

    /// `bytes=start-(end-1)` for a half-open range (S3 Range is inclusive).
    fn range_header(range: &Range<u64>) -> String {
        format!("bytes={}-{}", range.start, range.end.saturating_sub(1))
    }

    // ---- GET via presigned URL + reqwest (true streaming) ---------------

    async fn presigned_get(&self, key: &str, opts: &GetOptions) -> Result<reqwest::Response> {
        let presigning = PresigningConfig::expires_in(Duration::from_secs(60))
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning config: {e}")))?;

        let mut builder = self.client.get_object().bucket(&self.bucket).key(key);

        if let Some(v) = &opts.if_none_match {
            builder = builder.if_none_match(v.http_etag());
        }
        if let Some(v) = &opts.if_match {
            builder = builder.if_match(v.http_etag());
        }
        if let Some(r) = &opts.range {
            builder = builder.range(Self::range_header(r));
        }

        let presigned = builder
            .presigned(presigning)
            .await
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning get: {e}")))?;

        let mut req = self.http.get(presigned.uri());
        for (name, value) in presigned.headers() {
            req = req.header(name, value);
        }

        // Open = connect + wait for response headers: cap the silence by the
        // idle bound (#130 — a dead pooled connection would hang `send()`
        // forever; bytes that flow are a different conversation, see the
        // per-chunk bound in `get_result_from_response`).
        match tokio::time::timeout(self.idle_timeout, req.send()).await {
            Ok(r) => r.map_err(|e| StoreError::retryable(anyhow::anyhow!("s3 get http: {e}"))),
            Err(_) => Err(StoreError::retryable(anyhow::anyhow!(
                "s3 get {key}: no response after {:?} (idle timeout)",
                self.idle_timeout
            ))),
        }
    }

    fn get_result_from_response(key: &str, resp: reqwest::Response, idle: Duration) -> Result<GetResult> {
        let status = resp.status();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned());
        let content_length = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let incarnation = resp
            .headers()
            .get("x-amz-meta-walgit-incarnation")
            .and_then(|v| v.to_str().ok());
        let last_modified = resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                DateTime::from_str(v, aws_smithy_types::date_time::Format::HttpDate).ok()
            });
        let version = version_from_parts(etag.as_deref(), incarnation, last_modified.as_ref());

        // `ObjectMeta::size` is the size of the whole object (as on GCS/memory),
        // also for range reads: `Content-Range: bytes a-b/total` carries it.
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, t)| t.trim().parse::<u64>().ok());

        match status.as_u16() {
            200 | 206 => {
                let meta = ObjectMeta {
                    key: key.into(),
                    size: total.or(content_length).unwrap_or(0),
                    version,
                };
                // Per-chunk idle bound, not a total deadline: a 30 GiB range
                // read streams for as long as bytes arrive; a tunnel that went
                // quiet mid-body errors as Retryable after `idle` of silence
                // and the remote reader retries the segment (#130 — the 24-min
                // clone must survive this bound, the dead connection must not).
                let key_owned = key.to_owned();
                let body = tokio_stream::StreamExt::timeout(resp.bytes_stream(), idle)
                    .map(move |r| match r {
                        Ok(Ok(b)) => Ok(b),
                        Ok(Err(e)) => {
                            Err(StoreError::retryable(anyhow::anyhow!("s3 body: {e}")))
                        }
                        Err(_) => Err(StoreError::retryable(anyhow::anyhow!(
                            "s3 body {key_owned}: no bytes for {idle:?} (idle timeout)"
                        ))),
                    })
                    .boxed();
                Ok(GetResult::Object { meta, body })
            }
            304 => Ok(GetResult::NotModified { version }),
            404 => Err(StoreError::NotFound { key: key.into() }),
            412 => Err(StoreError::PreconditionFailed {
                key: key.into(),
                current: Some(version),
            }),
            s if s >= 500 || s == 429 => {
                Err(StoreError::Retryable(anyhow::anyhow!("s3 get status {s}")))
            }
            s => Err(StoreError::Other(anyhow::anyhow!("s3 get status {s}"))),
        }
    }
}

// ---- PutBody → SDK ByteStream ------------------------------------------

async fn body_to_s3(body: PutBody) -> Result<(S3ByteStream, u64)> {
    Ok(match body {
        PutBody::Bytes(b) => {
            let len = b.len() as u64;
            (S3ByteStream::from(b), len)
        }
        PutBody::Stream { len, stream } => {
            // Collect into Bytes: walgit's Stream bodies are small objects
            // (manifests, leases). Large packs use PutBody::File which
            // streams via ByteStream::read_from().
            let hint = usize::try_from(len).map_err(|_| {
                StoreError::InvalidArgument(format!("stream length {len} exceeds usize"))
            })?;
            let collected = util::collect(stream, hint).await?;
            (S3ByteStream::from(collected), len)
        }
        PutBody::File(path) => {
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| StoreError::other(anyhow::anyhow!("stat {}: {e}", path.display())))?;
            let len = meta.len();
            let stream = S3ByteStream::read_from()
                .path(&path)
                .buffer_size(64 * 1024)
                .build()
                .await
                .map_err(|e| StoreError::other(anyhow::anyhow!("file stream: {e}")))?;
            (stream, len)
        }
    })
}

// ---- error classification ----------------------------------------------

/// Extract the error code string from an `SdkError`'s service error metadata.
fn err_code<E>(err: &aws_sdk_s3::error::SdkError<E>) -> Option<&str>
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    err.as_service_error().and_then(|e| e.meta().code())
}

/// #130: the transport shapes a wedge produces — a connect timeout, a read
/// timeout (no response head within `store.idle_timeout`), a dead connector,
/// a dispatch error — say nothing about the object and are exactly what the
/// callers' retry budgets exist for: surface them as `Retryable`, never as
/// the old catch-all `Other` that turned a dead tunnel into an
/// un-retryable-looking hang. Returns `None` for non-transport errors so the
/// site's own classification (CAS codes, 404 leniency) still applies.
fn transport_retryable<E>(what: &str, err: &aws_sdk_s3::error::SdkError<E>) -> Option<StoreError> {
    use aws_sdk_s3::error::SdkError;
    let kind = match err {
        SdkError::TimeoutError(_) => "timeout",
        SdkError::DispatchFailure(d) if d.is_timeout() => "timeout",
        SdkError::DispatchFailure(d) if d.is_io() => "io",
        SdkError::DispatchFailure(_) => "dispatch",
        SdkError::ResponseError(_) => "incomplete response",
        _ => return None,
    };
    Some(StoreError::retryable(anyhow::anyhow!(
        "s3 {what} {kind} failure: {err}"
    )))
}

fn classify_put_error(
    key: &str,
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
) -> StoreError {
    if let Some(e) = transport_retryable("put", err) {
        return e;
    }
    let code = err_code(err).unwrap_or("");
    match code {
        "PreconditionFailed" | "ConditionalRequestConflict" => StoreError::PreconditionFailed {
            key: key.into(),
            current: None,
        },
        _ => StoreError::Other(anyhow::anyhow!("s3 put error: {err}")),
    }
}

fn classify_delete_error(
    key: &str,
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::delete_object::DeleteObjectError>,
) -> StoreError {
    if let Some(e) = transport_retryable("delete", err) {
        return e;
    }
    match err_code(err).unwrap_or("") {
        "PreconditionFailed" | "ConditionalRequestConflict" => StoreError::PreconditionFailed {
            key: key.into(),
            current: None,
        },
        _ => StoreError::Other(anyhow::anyhow!("s3 delete error: {err}")),
    }
}

fn classify_list_error(
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error>,
) -> StoreError {
    if let Some(e) = transport_retryable("list", err) {
        return e;
    }
    StoreError::Other(anyhow::anyhow!("s3 list error: {err}"))
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    fn backend(&self) -> &'static str {
        "s3"
    }

    async fn get(&self, key: &str, opts: GetOptions) -> Result<GetResult> {
        let resp = self.presigned_get(key, &opts).await?;
        Self::get_result_from_response(key, resp, self.idle_timeout)
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match resp {
            Ok(out) => {
                let etag = out.e_tag().map(|s| s.trim_matches('"').to_owned());
                // A HEAD content-length is never negative; u64::try_from just
                // spells that out for the type system.
                let size = u64::try_from(out.content_length().unwrap_or(0)).map_err(|_| {
                    StoreError::InvalidArgument(format!("negative content-length on {key}"))
                })?;
                let version = version_from_parts(
                    etag.as_deref(),
                    metadata_incarnation(out.metadata()),
                    out.last_modified(),
                );
                Ok(Some(ObjectMeta {
                    key: key.into(),
                    size,
                    version,
                }))
            }
            Err(err) => {
                if let Some(aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)) =
                    err.as_service_error()
                {
                    return Ok(None);
                }
                Err(transport_retryable("head", &err).unwrap_or_else(|| {
                    StoreError::Other(anyhow::anyhow!("s3 head error: {err}"))
                }))
            }
        }
    }

    async fn put(&self, key: &str, body: PutBody, opts: PutOptions) -> Result<ObjectMeta> {
        let (s3_body, len) = body_to_s3(body).await?;

        // Above the threshold, Overwrite and Create both go multipart;
        // `CreateMultipartUpload` has no conditional header support in the S3
        // API, so Create keeps its no-overwrite intent with a HEAD pre-check
        // (best effort — see the module docs). Update stays single-shot, and
        // every CAS-rewritten object is small.
        let use_multipart = len > self.multipart_threshold
            && matches!(opts.mode, PutMode::Overwrite | PutMode::Create);

        if use_multipart {
            if matches!(opts.mode, PutMode::Create)
                && let Some(current) = self.head(key).await?
            {
                return Err(StoreError::PreconditionFailed {
                    key: key.into(),
                    current: Some(current.version),
                });
            }
            return self.multipart_put(key, s3_body, len, &opts).await;
        }

        let incarnation = new_incarnation();
        let mut builder = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(s3_body)
            .metadata(INCARNATION_META, incarnation.clone())
            .content_length(len.try_into().map_err(|_| {
                StoreError::InvalidArgument(format!(
                    "object {key} is larger than i64::MAX (S3's own cap is 5 TiB)"
                ))
            })?);

        match &opts.mode {
            PutMode::Overwrite => {}
            PutMode::Create => {
                builder = builder.if_none_match("*");
            }
            PutMode::Update(v) => {
                builder = builder.if_match(v.http_etag());
            }
        }

        if let Some(ct) = opts.content_type {
            builder = builder.content_type(ct);
        }

        let result = builder.send().await;
        match result {
            Ok(resp) => {
                let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
                Ok(ObjectMeta {
                    key: key.into(),
                    size: len,
                    version: version_from_parts(etag.as_deref(), Some(&incarnation), None),
                })
            }
            Err(e) => {
                let mut err = classify_put_error(key, &e);
                // Fill `current` via HEAD if we got a PreconditionFailed.
                if let StoreError::PreconditionFailed { current: c, .. } = &mut err
                    && c.is_none() {
                        *c = self.head(key).await.ok().flatten().map(|m| m.version);
                    }
                Err(err)
            }
        }
    }

    async fn delete(&self, key: &str, if_version: Option<Version>) -> Result<()> {
        if let Some(want) = &if_version {
            // S3 has no conditional delete: emulate via HEAD + compare + DELETE.
            // RACE: a concurrent writer could replace the object between HEAD
            // and DELETE. Acceptable for walgit's lease-guarded semantics.
            let head = self.head(key).await?;
            match head {
                None => return Err(StoreError::NotFound { key: key.into() }),
                Some(meta) if &meta.version != want => {
                    return Err(StoreError::PreconditionFailed {
                        key: key.into(),
                        current: Some(meta.version),
                    });
                }
                _ => {}
            }
        }

        let mut delete = self.client.delete_object().bucket(&self.bucket).key(key);
        if let Some(want) = &if_version {
            // The HEAD above compares the full incarnation. Rustfs/R2 support
            // `If-Match` on DELETE, so this second guard still prevents a
            // different ETag from being deleted if the object changes between
            // the HEAD and this request.
            delete = delete.if_match(want.http_etag());
        }
        let resp = delete.send().await;

        match resp {
            Ok(_) => Ok(()),
            Err(err) => {
                // S3 DeleteObject is idempotent: deleting a non-existent key
                // returns Ok, not an error. If we get here, it's a real error.
                // For unconditional deletes we treat any error as transient.
                if if_version.is_none() {
                    // Unconditional delete — be lenient (idempotent on S3/rustfs).
                    let err_str = err.to_string();
                    if err_str.contains("404")
                        || err_str.contains("NoSuchKey")
                        || err_str.contains("not found")
                    {
                        return Ok(());
                    }
                }
                let mut err = classify_delete_error(key, &err);
                if let StoreError::PreconditionFailed { current, .. } = &mut err {
                    match self.head(key).await {
                        Ok(Some(meta)) => *current = Some(meta.version),
                        Ok(None) => return Err(StoreError::NotFound { key: key.into() }),
                        Err(_) => {}
                    }
                }
                Err(err)
            }
        }
    }

    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        // LIST does not expose S3 user metadata, so a full incarnation needs
        // one HEAD per object. Callers that only need keys must use
        // `list_keys` to keep bulk walks one LIST per page.
        let store = self.clone();
        let stream = self.list_keys(prefix, start_after).then(move |r| {
            let store = store.clone();
            async move {
                match r {
                    Ok(key) => store.head(&key).await,
                    Err(e) => Err(e),
                }
            }
        });
        Box::pin(stream.filter_map(|r| async move {
            match r {
                Ok(Some(meta)) => Some(Ok(meta)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        }))
    }

    fn list_keys(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> BoxStream<'static, Result<String>> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let prefix = prefix.to_owned();
        let start_after = start_after.map(std::borrow::ToOwned::to_owned);

        Box::pin(futures::stream::unfold(
            ListState {
                client,
                bucket,
                prefix,
                start_after,
                continuation_token: None,
                started: false,
                buffer: Vec::new().into_iter(),
            },
            |mut state| async move {
                // Drain buffered items first.
                if let Some(item) = state.buffer.next() {
                    return Some((item, state));
                }

                if state.started && state.continuation_token.is_none() {
                    return None;
                }
                state.started = true;

                let mut builder = state
                    .client
                    .list_objects_v2()
                    .bucket(&state.bucket)
                    .prefix(&state.prefix)
                    .max_keys(1000);

                if let Some(sa) = &state.start_after {
                    builder = builder.start_after(sa);
                }
                if let Some(ct) = &state.continuation_token {
                    builder = builder.continuation_token(ct);
                }

                match builder.send().await {
                    Ok(resp) => {
                        let items: Vec<Result<String>> = resp
                            .contents()
                            .iter()
                            .map(|obj| Ok(obj.key().unwrap_or("").to_owned()))
                            .collect();

                        state.continuation_token = resp
                            .is_truncated()
                            .unwrap_or(false)
                            .then(|| resp.next_continuation_token().map(std::borrow::ToOwned::to_owned))
                            .flatten();
                        state.buffer = items.into_iter();

                        let item = state.buffer.next();
                        item.map(|i| (i, state))
                    }
                    Err(err) => Some((Err(classify_list_error(&err)), state)),
                }
            },
        ))
    }

    async fn list_prefixes(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut continuation_token: Option<String> = None;
        loop {
            let mut builder = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .delimiter("/")
                .max_keys(1000);
            if let Some(ct) = &continuation_token {
                builder = builder.continuation_token(ct);
            }
            let resp = builder.send().await.map_err(|e| classify_list_error(&e))?;
            out.extend(
                resp.common_prefixes()
                    .iter()
                    .filter_map(|p| p.prefix().map(str::to_owned)),
            );
            continuation_token = resp
                .is_truncated()
                .unwrap_or(false)
                .then(|| resp.next_continuation_token().map(std::borrow::ToOwned::to_owned))
                .flatten();
            if continuation_token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// A presigned GET (1 h): the edge needs no credentials and `Range` stays free (unsigned).
    async fn accel_target(&self, key: &str) -> Option<crate::AccelTarget> {
        let url = self
            .signed_get_url(key, Duration::from_secs(3600))
            .await
            .ok()
            .flatten()?;
        Some(crate::AccelTarget {
            url,
            authorization: None,
        })
    }

    fn supports_compose(&self) -> bool {
        true
    }

    /// Concatenate `sources` into `dest` with one multipart upload whose parts are
    /// `UploadPartCopy` byte ranges of the sources — nothing streams through this process
    /// except the parts S3 will not copy: every part but the last must be >= 5 MiB, so a
    /// small source (a bundle header in front of a 30 GB pack) is fetched and uploaded
    /// together with the beginning of the next source as one ordinary part.
    #[allow(
        clippy::indexing_slicing,
        reason = "every indexed read uses an index produced by the segment scan in the same loop iteration: while pos < total, the first source whose cumulative end exceeds pos exists and is < sources.len() (sizes are non-negative; zero-length sources are skipped by the same scan). offset_of(i) sums sizes[..i] only with that in-range i."
    )]
    async fn compose(
        &self,
        dest: &str,
        sources: &[String],
        opts: PutOptions,
    ) -> Result<ObjectMeta> {
        const MIN_PART: u64 = 5 * 1024 * 1024;
        const COPY_PART: u64 = 1024 * 1024 * 1024; // <= 5 GiB per UploadPartCopy
        if sources.is_empty() {
            return Err(StoreError::InvalidArgument(
                "compose needs at least one source".into(),
            ));
        }
        if let PutMode::Create = opts.mode
            && self.head(dest).await?.is_some()
        {
            return Err(StoreError::PreconditionFailed {
                key: dest.to_owned(),
                current: None,
            });
        }
        // Sizes first: the layout of parts depends on them.
        let mut sizes = Vec::with_capacity(sources.len());
        for src in sources {
            let m = self
                .head(src)
                .await?
                .ok_or_else(|| StoreError::NotFound { key: src.clone() })?;
            sizes.push(m.size);
        }
        let total: u64 = sizes.iter().sum();
        // The virtual concatenation, cut into parts: a part is [start, end) of the whole.
        // Runs that lie inside one source and are >= MIN_PART become copies; everything else
        // (a small source, the tail that pads it to MIN_PART) is read and uploaded.
        let incarnation = new_incarnation();
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(dest)
            .metadata(INCARNATION_META, incarnation.clone());
        if let Some(ct) = opts.content_type {
            create = create.content_type(ct);
        }
        if opts.immutable {
            create = create.cache_control("public, max-age=31536000, immutable");
        }
        let upload = create
            .send()
            .await
            .map_err(|e| {
                transport_retryable("create multipart", &e)
                    .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 create multipart: {e}")))
            })?;
        let upload_id = upload
            .upload_id()
            .ok_or_else(|| {
                StoreError::other(anyhow::anyhow!("no upload_id from CreateMultipartUpload"))
            })?
            .to_owned();
        let mut parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
        let mut part_number = 1i32;
        let mut pos: u64 = 0; // absolute offset into the concatenation
        // `i` below is always the index of the source that covers `pos`
        // (see the scan comment); all `sizes[i]` / `sources[i]` reads in this
        // method use exactly that index, so every indexed access is in range.
        let offset_of = |i: usize| -> u64 { sizes.iter().take(i).sum() };
        let result: Result<()> = async {
            while pos < total {
                // Which source does `pos` fall in, and how far does it run?
                // pos < total guarantees the scan succeeds (some source's
                // cumulative end exceeds pos); Err is only a safety net.
                let i = (0..sources.len())
                    .find(|&i| pos < offset_of(i) + sizes[i])
                    .ok_or_else(|| {
                        StoreError::InvalidArgument(
                            "compose: internal scan lost the segment for the current offset".into(),
                        )
                    })?;
                let src_end = offset_of(i) + sizes[i];
                let run = src_end - pos;
                let last_part = src_end == total;
                if run >= MIN_PART || last_part {
                    // Copy a range of this one source.
                    let len = run.min(COPY_PART);
                    let from = pos - offset_of(i);
                    let part = self
                        .client
                        .upload_part_copy()
                        .bucket(&self.bucket)
                        .key(dest)
                        .upload_id(&upload_id)
                        .part_number(part_number)
                        .copy_source(format!(
                            "{}/{}",
                            self.bucket,
                            crate::util::encode_path(&sources[i])
                        ))
                        .copy_source_range(format!("bytes={from}-{}", from + len - 1))
                        .send()
                        .await
                        .map_err(|e| {
                            transport_retryable("upload part copy", &e).unwrap_or_else(|| {
                                StoreError::Other(anyhow::anyhow!("s3 upload part copy: {e}"))
                            })
                        })?;
                    let etag = part
                        .copy_part_result()
                        .and_then(|r| r.e_tag())
                        .unwrap_or("")
                        .to_owned();
                    parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .e_tag(etag)
                            .part_number(part_number)
                            .build(),
                    );
                    pos += len;
                } else {
                    // Too small to copy on its own: read MIN_PART bytes across source boundaries.
                    let want = MIN_PART.min(total - pos);
                    let want_usize = usize::try_from(want).map_err(|_| {
                        StoreError::InvalidArgument(
                            "compose gather wants more bytes than fit this host's usize".into(),
                        )
                    })?;
                    let mut buf = Vec::with_capacity(want_usize);
                    let mut p = pos;
                    while (buf.len() as u64) < want {
                        let j = (0..sources.len())
                            .find(|&j| p < offset_of(j) + sizes[j])
                            .ok_or_else(|| {
                                StoreError::InvalidArgument(
                                    "compose: internal scan lost the segment for the gather offset"
                                        .into(),
                                )
                            })?;
                        let from = p - offset_of(j);
                        let take = (sizes[j] - from).min(want - buf.len() as u64);
                        let (_, bytes) = self
                            .get(
                                &sources[j],
                                GetOptions {
                                    range: Some(from..from + take),
                                    ..GetOptions::default()
                                },
                            )
                            .await?
                            .bytes()
                            .await?
                            .ok_or_else(|| StoreError::NotFound {
                                key: sources[j].clone(),
                            })?;
                        buf.extend_from_slice(&bytes);
                        p += take;
                    }
                    let len = buf.len() as u64;
                    let len_i64 = i64::try_from(len).map_err(|_| {
                        StoreError::InvalidArgument(
                            "uploaded part is larger than i64::MAX (S3's own cap is 5 TiB)".into(),
                        )
                    })?;
                    let part = self
                        .client
                        .upload_part()
                        .bucket(&self.bucket)
                        .key(dest)
                        .upload_id(&upload_id)
                        .part_number(part_number)
                        .body(S3ByteStream::from(Bytes::from(buf)))
                        .content_length(len_i64)
                        .send()
                        .await
                        .map_err(|e| {
                            transport_retryable("upload part", &e)
                                .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 upload part: {e}")))
                        })?;
                    parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .e_tag(part.e_tag().unwrap_or("").to_owned())
                            .part_number(part_number)
                            .build(),
                    );
                    pos += len;
                }
                part_number += 1;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = self.abort_multipart(dest, &upload_id).await;
            return Err(e);
        }
        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        let resp = match self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(dest)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self.abort_multipart(dest, &upload_id).await;
                return Err(transport_retryable("complete multipart", &e)
                    .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 complete multipart: {e}"))));
            }
        };
        let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
        Ok(ObjectMeta {
            key: dest.into(),
            size: total,
            version: version_from_parts(etag.as_deref(), Some(&incarnation), None),
        })
    }

    async fn signed_get_url(&self, key: &str, ttl: Duration) -> Result<Option<String>> {
        let presigning = PresigningConfig::expires_in(ttl)
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning config: {e}")))?;
        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning)
            .await
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning: {e}")))?;
        Ok(Some(presigned.uri().to_owned()))
    }
}

/// State for the lazy list stream.
struct ListState {
    client: S3Client,
    bucket: String,
    prefix: String,
    start_after: Option<String>,
    continuation_token: Option<String>,
    started: bool,
    buffer: std::vec::IntoIter<Result<String>>,
}

// ---- multipart upload (Overwrite only) ---------------------------------

impl S3Store {
    async fn multipart_put(
        &self,
        key: &str,
        body: S3ByteStream,
        len: u64,
        opts: &PutOptions,
    ) -> Result<ObjectMeta> {
        use tokio::io::AsyncReadExt;

        let incarnation = new_incarnation();
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .metadata(INCARNATION_META, incarnation.clone());

        if let Some(ct) = opts.content_type {
            create = create.content_type(ct);
        }

        let upload = create
            .send()
            .await
            .map_err(|e| {
                transport_retryable("create multipart", &e)
                    .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 create multipart: {e}")))
            })?;

        let upload_id = upload
            .upload_id()
            .ok_or_else(|| {
                StoreError::other(anyhow::anyhow!("no upload_id from CreateMultipartUpload"))
            })?
            .to_owned();

        // Cancellation (task abort/drain) drops this future without running any
        // `abort_multipart` below; the guard fires best-effort on drop. A hard
        // process kill cannot be covered in-process — see the module docs.
        let mut abort_guard = MultipartAbortGuard::new(
            self.client.clone(),
            self.bucket.clone(),
            key.to_owned(),
            upload_id.clone(),
        );

        let part_size = self.multipart_part_size;
        let mut part_number = 1i32;
        let mut uploaded_parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
        let mut remaining = len;

        let mut reader = body.into_async_read();

        while remaining > 0 {
            let this_part = part_size.min(remaining);
            let to_read = usize::try_from(this_part).map_err(|_| {
                StoreError::InvalidArgument(format!(
                    "configured multipart part size exceeds this host's usize ({key})"
                ))
            })?;
            let mut buf = vec![0u8; to_read];
            let mut read_total = 0;

            while read_total < to_read {
                // The loop guard keeps the tail slice in bounds: the only
                // slice start used is read_total < to_read == buf.len().
                #[allow(
                    clippy::indexing_slicing,
                    reason = "the `read_total < to_read` loop guard keeps the tail slice start below buf.len()"
                )]
                let n = match reader.read(&mut buf[read_total..]).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = self.abort_multipart(key, &upload_id).await;
                        return Err(StoreError::other(anyhow::anyhow!("multipart read: {e}")));
                    }
                };
                if n == 0 {
                    break;
                }
                read_total += n;
            }

            if read_total < to_read {
                // A body shorter than the declared length must never Complete:
                // the object would keep the content-addressed key while holding
                // fewer bytes than the key names.
                let _ = self.abort_multipart(key, &upload_id).await;
                abort_guard.disarm();
                return Err(StoreError::InvalidArgument(format!(
                    "put body for {key} ended after {read_total} bytes of part {part_number} \
                     (declared length {len}); refusing to complete a truncated object"
                )));
            }
            buf.truncate(read_total);
            let actual = read_total as u64;
            let actual_i64 = i64::try_from(actual).map_err(|_| {
                StoreError::InvalidArgument(format!(
                    "uploaded part is larger than i64::MAX (S3's own cap is 5 TiB) ({key})"
                ))
            })?;

            let part = match self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .body(S3ByteStream::from(Bytes::from(buf)))
                .content_length(actual_i64)
                .send()
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    let _ = self.abort_multipart(key, &upload_id).await;
                    return Err(transport_retryable("upload part", &e)
                        .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 upload part: {e}"))));
                }
            };

            let etag = part.e_tag().unwrap_or("").to_owned();
            uploaded_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .e_tag(etag)
                    .part_number(part_number)
                    .build(),
            );

            remaining -= actual;
            part_number += 1;
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(uploaded_parts))
            .build();

        let resp = match self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self.abort_multipart(key, &upload_id).await;
                return Err(transport_retryable("complete multipart", &e)
                    .unwrap_or_else(|| StoreError::Other(anyhow::anyhow!("s3 complete multipart: {e}"))));
            }
        };

        abort_guard.disarm();
        let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
        Ok(ObjectMeta {
            key: key.into(),
            size: len,
            version: version_from_parts(etag.as_deref(), Some(&incarnation), None),
        })
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|e| StoreError::other(anyhow::anyhow!("abort multipart: {e}")))?;
        Ok(())
    }
}

/// Best-effort `AbortMultipartUpload` for a multipart upload whose future is
/// dropped before completion (task abort, drain, early return). Spawning needs
/// a runtime: outside one there is nothing to abort with, and the bucket's
/// lifecycle rule is the only backstop left.
struct MultipartAbortGuard {
    client: S3Client,
    bucket: String,
    key: String,
    upload_id: String,
    armed: bool,
}

impl MultipartAbortGuard {
    fn new(client: S3Client, bucket: String, key: String, upload_id: String) -> Self {
        Self {
            client,
            bucket,
            key,
            upload_id,
            armed: true,
        }
    }

    /// The upload completed (or was aborted explicitly): never abort on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MultipartAbortGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let bucket = std::mem::take(&mut self.bucket);
        let key = std::mem::take(&mut self.key);
        let upload_id = std::mem::take(&mut self.upload_id);
        runtime.spawn(async move {
            let _ = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .send()
                .await;
        });
    }
}

fn static_credentials(
    access_key: &str,
    secret_key: &str,
    session_token: Option<String>,
) -> Credentials {
    Credentials::new(access_key, secret_key, session_token, None, "walgit-static")
}

// ---- rustfs compatibility notes (integration testing) -------------------
//
// 1. Presigned URLs: rustfs honors SigV4 presigned GET URLs with conditional
//    headers (If-None-Match, If-Match, Range) in SignedHeaders.
// 2. If-None-Match: * on PUT: 412 "PreconditionFailed" when object exists.
// 3. If-Match: <etag> on PUT: 412 when ETag mismatch.
// 4. 304 Not Modified: HTTP 304 with ETag header, empty body.
// 5. ListObjectsV2: StartAfter, ContinuationToken, IsTruncated/NextToken OK.
// 6. DeleteObject: idempotent for absent keys (204).
// 7. Multipart: CreateMultipartUpload + UploadPart + CompleteMultipartUpload
//    supported. No conditional headers on Create/Complete (same as real S3).
// 8. ETags: quoted, MD5 for single-PUT, compound for multipart. Quotes
//    stripped consistently in our Version.
// 9. force_path_style: required for rustfs local dev.
// 10. If-Match on DeleteObject: honored (wrong ETag -> 412); we use it as a
//     second guard after the full-incarnation HEAD comparison.
// 11. User metadata survives PutObject/CreateMultipartUpload and is returned
//     by HeadObject/GetObject; ListObjectsV2 does not expose it, hence the
//     separate full `list` vs cheap `list_keys` contract.

#[cfg(test)]
mod tests {
    use super::*;

    /// #130 regression — the wedge shape: a peer that accepts the TCP
    /// connection and then **never replies** (no bytes, no RST; a dropped
    /// VPN tunnel to R2 looks exactly like this). Before the socket bounds,
    /// `compact --base` slept 15.5 h on two such connections. With them,
    /// every lane — the SDK (head/put/list) and the presigned-GET reqwest —
    /// must return a **retryable** error, not hang, not `Other`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_peer_yields_retryable_errors_not_hangs() {
        use std::net::TcpListener;
        use std::time::Instant;

        // Accept and keep the sockets open, never read, never answer: a
        // request hangs exactly as it does on a silently dead tunnel.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let accepted = accepted.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    accepted.lock().unwrap().push(stream);
                }
            });
        }

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "wedge".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                ..Default::default()
            },
            connect_timeout: Duration::from_millis(500),
            idle_timeout: Duration::from_millis(500),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        // The SDK lane (this is where `compact --base` wedged: HEADs and
        // UploadPart on keep-alive connections that went dead).
        let started = Instant::now();
        let err = store.head("repos/x/y/manifest.pb").await.unwrap_err();
        assert!(err.is_retryable(), "head: {err:?}");
        // SDK internal retries (3 attempts) plus jittered backoff: bound it
        // generously — minutes-waiting is the bug, seconds is the fix.
        assert!(started.elapsed() < Duration::from_secs(60), "{:?}", started.elapsed());

        // The presigned-GET lane (ranged reads, the 24-minute clone traffic).
        let started = Instant::now();
        let Err(err) = ObjectStore::get(
            &store,
            "repos/x/y/wal/pack.pack",
            GetOptions {
                range: Some(0..1024),
                ..GetOptions::default()
            },
        )
        .await
        else {
            panic!("dead peer must not answer");
        };
        assert!(err.is_retryable(), "get: {err:?}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    /// A `PutMode::Create` above the threshold must use the multipart path
    /// (issue #218: 135 MiB immutable packs used to go out as one `PutObject`
    /// and time out past `store.idle_timeout`; single-shot is also capped at
    /// 5 GiB, below a tier-2 base). The fake S3 here *only* implements the
    /// multipart protocol and answers 501 to a plain `PUT`, so a regression to
    /// single-shot cannot pass. The pre-check half pins the documented
    /// best-effort create: an existing key answers `PreconditionFailed`
    /// without starting an upload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_create_uses_multipart_and_prechecks_existence() {
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let existing = Arc::new(Mutex::new(false));
        {
            let seen = seen.clone();
            let existing = existing.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let seen = seen.clone();
                    let existing = existing.clone();
                    std::thread::spawn(move || {
                        fake_s3_multipart_only(stream, &seen, &existing);
                    });
                }
            });
        }

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "multi-create".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                force_path_style: true,
                ..Default::default()
            },
            multipart_threshold: bytesize::ByteSize::b(1024),
            multipart_part_size: bytesize::ByteSize::b(2048),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        // 4 KiB with a 2 KiB part size: two UploadPart requests.
        let body = Bytes::from(vec![7u8; 4096]);
        let meta = store
            .put(
                "repos/o/r/wal/pack.pack",
                PutBody::Bytes(body.clone()),
                PutOptions {
                    mode: PutMode::Create,
                    immutable: true,
                    ..Default::default()
                },
            )
            .await
            .expect("large Create must take the multipart path");
        assert_eq!(meta.size, 4096);

        let requests = seen.lock().unwrap().clone();
        assert!(
            requests
                .first()
                .is_some_and(|r| r.starts_with("HEAD ") && r.ends_with("pack.pack HTTP/1.1")),
            "a large Create must HEAD the key first: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("uploads")),
            "CreateMultipartUpload missing: {requests:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("PUT ") && r.contains("partNumber="))
                .count(),
            2,
            "every part must be uploaded separately: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("uploadId=")),
            "CompleteMultipartUpload missing: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.starts_with("PUT ") && !r.contains("partNumber=")),
            "no single-shot PUT may be attempted: {requests:?}"
        );
        assert!(
            !requests.iter().any(|r| r.starts_with("DELETE ")),
            "a completed upload must not abort: {requests:?}"
        );
        // The documented budget: HEAD + CreateMultipartUpload + Complete + one
        // request per part (see docs/ROUNDTRIPS.md).
        assert_eq!(
            requests.len(),
            3 + 2,
            "HEAD + initiate + complete + 2 parts: {requests:?}"
        );

        // The key now exists in the fake bucket: the pre-check must answer
        // PreconditionFailed and issue no multipart requests at all.
        *existing.lock().unwrap() = true;
        let before = seen.lock().unwrap().len();
        let again = store
            .put(
                "repos/o/r/wal/pack.pack",
                PutBody::Bytes(body),
                PutOptions::from(PutMode::Create),
            )
            .await;
        assert!(
            matches!(again, Err(StoreError::PreconditionFailed { .. })),
            "existing large Create must be PreconditionFailed, got {again:?}"
        );
        let after = seen.lock().unwrap().clone();
        assert_eq!(
            after.len(),
            before + 1,
            "only the HEAD pre-check may run once the key exists: {:?}",
            &after[before..]
        );
        assert!(after.last().is_some_and(|r| r.starts_with("HEAD ")));
    }

    /// Minimal S3 stub for [`large_create_uses_multipart_and_prechecks_existence`]:
    /// HEAD (404 absent / 200 present), `CreateMultipartUpload`, `UploadPart` and
    /// `CompleteMultipartUpload`. Any other request — in particular a single-shot
    /// `PUT` without a part number — is 501.
    fn fake_s3_multipart_only(
        mut stream: std::net::TcpStream,
        seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        existing: &std::sync::Arc<std::sync::Mutex<bool>>,
    ) {
        use std::io::{BufRead, BufReader, Read as _, Write as _};

        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut content_length = 0usize;
        let mut expect_continue = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("expect:") && lower.contains("100-continue") {
                expect_continue = true;
            }
        }
        if expect_continue {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = stream.flush();
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }

        let line = request_line.trim_end().to_owned();
        seen.lock().unwrap().push(line.clone());
        let (method, target) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let present = *existing.lock().unwrap();

        let (status, headers, body): (&str, String, Vec<u8>) = if method == "HEAD" {
            if present {
                (
                    "200 OK",
                    "etag: \"stub\"\r\ncontent-length: 0\r\n".to_owned(),
                    Vec::new(),
                )
            } else {
                (
                    "404 Not Found",
                    "content-length: 0\r\n".to_owned(),
                    Vec::new(),
                )
            }
        } else if method == "POST" && target.contains("uploads") {
            let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>stub-upload</UploadId></InitiateMultipartUploadResult>";
            (
                "200 OK",
                format!(
                    "content-type: application/xml\r\ncontent-length: {}\r\n",
                    xml.len()
                ),
                xml.to_vec(),
            )
        } else if method == "PUT" && target.contains("partNumber=") {
            (
                "200 OK",
                "etag: \"stub-part\"\r\ncontent-length: 0\r\n".to_owned(),
                Vec::new(),
            )
        } else if method == "DELETE" && target.contains("uploadId=") {
            (
                "204 No Content",
                "content-length: 0\r\n".to_owned(),
                Vec::new(),
            )
        } else if method == "POST" && target.contains("uploadId=") {
            let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult><Location>http://stub/b/k</Location><Bucket>b</Bucket><Key>k</Key><ETag>\"stub-complete\"</ETag></CompleteMultipartUploadResult>";
            (
                "200 OK",
                format!(
                    "content-type: application/xml\r\ncontent-length: {}\r\n",
                    xml.len()
                ),
                xml.to_vec(),
            )
        } else {
            (
                "501 Not Implemented",
                "content-length: 0\r\n".to_owned(),
                Vec::new(),
            )
        };

        let head = format!("HTTP/1.1 {status}\r\n{headers}connection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    }

    /// A body shorter than its declared length must abort, never Complete: the
    /// object would otherwise keep a content-addressed key while holding fewer
    /// bytes than the key names (review finding on #218).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn short_multipart_body_aborts_instead_of_completing() {
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let existing = Arc::new(Mutex::new(false));
        {
            let seen = seen.clone();
            let existing = existing.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let seen = seen.clone();
                    let existing = existing.clone();
                    std::thread::spawn(move || {
                        fake_s3_multipart_only(stream, &seen, &existing);
                    });
                }
            });
        }

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "short-body".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                force_path_style: true,
                ..Default::default()
            },
            multipart_threshold: bytesize::ByteSize::b(1024),
            multipart_part_size: bytesize::ByteSize::b(2048),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        // Declared 4097 bytes, the stream holds 4096: the second part is short.
        let body = Bytes::from(vec![1u8; 4096]);
        let err = store
            .put(
                "repos/o/r/wal/short.pack",
                PutBody::Stream {
                    len: 4097,
                    stream: crate::util::once(body),
                },
                PutOptions {
                    mode: PutMode::Create,
                    immutable: true,
                    ..Default::default()
                },
            )
            .await
            .expect_err("a short body must not succeed");
        assert!(
            matches!(err, StoreError::InvalidArgument(_)),
            "short body must be InvalidArgument, got {err:?}"
        );

        let requests = seen.lock().unwrap().clone();
        assert!(
            !requests
                .iter()
                .any(|r| r.starts_with("POST ") && r.contains("uploadId=")),
            "a short body must never Complete: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|r| r.starts_with("DELETE ") && r.contains("uploadId=")),
            "a short body must abort its upload: {requests:?}"
        );
    }

    /// Dropping the upload future (task abort/drain) must fire the drop guard's
    /// best-effort abort; a hard kill needs the bucket lifecycle rule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_upload_guard_aborts_the_multipart_upload() {
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let existing = Arc::new(Mutex::new(false));
        {
            let seen = seen.clone();
            let existing = existing.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let seen = seen.clone();
                    let existing = existing.clone();
                    std::thread::spawn(move || {
                        fake_s3_multipart_only(stream, &seen, &existing);
                    });
                }
            });
        }

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "drop-abort".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                force_path_style: true,
                ..Default::default()
            },
            multipart_threshold: bytesize::ByteSize::b(1024),
            multipart_part_size: bytesize::ByteSize::b(2048),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        drop(MultipartAbortGuard::new(
            store.client.clone(),
            store.bucket.clone(),
            "repos/o/r/wal/dropped.pack".to_owned(),
            "stub-upload".to_owned(),
        ));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let requests = seen.lock().unwrap().clone();
            if requests
                .iter()
                .any(|r| r.starts_with("DELETE ") && r.contains("uploadId="))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drop guard never aborted: {requests:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The idle bound must measure **silence**, not total time: a response
    /// that trickles bytes forever is a working download (a 30 GiB base-pack
    /// range read at §1.1 speeds is legitimate), and must not be cut off by
    /// the deadline that kills the dead connection above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trickling_body_is_not_killed_by_the_idle_bound() {
        use futures::TryStreamExt;
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let chunk = b"x".repeat(1024);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let chunk = chunk.clone();
                std::thread::spawn(move || {
                    // Read the request (whatever it is), then drip the body:
                    // one byte-range window of latency per 500 ms — every
                    // gap is *below* the 2 s idle bound, the total is far
                    // above it.
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 8192\r\netag: \"trickle\"\r\n\r\n",
                    );
                    let _ = s.flush();
                    for _ in 0..8 {
                        std::thread::sleep(Duration::from_millis(500));
                        if s.write_all(&chunk).is_err() || s.flush().is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "trickle".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                ..Default::default()
            },
            connect_timeout: Duration::from_millis(500),
            idle_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        let started = Instant::now();
        let got = ObjectStore::get(&store, "wal/pack.pack", GetOptions::default())
            .await
            .expect("trickle must not trip the idle bound");
        let GetResult::Object { body, .. } = got else {
            panic!("object expected");
        };
        let bytes = body
            .try_fold(Vec::new(), |mut acc, b| async move {
                acc.extend_from_slice(&b);
                Ok(acc)
            })
            .await
            .expect("full body survives");
        assert_eq!(bytes.len(), 8192, "all 8 chunks");
        assert!(started.elapsed() > Duration::from_secs(2), "test itself must stream longer than the idle bound — otherwise it proves nothing");
    }

    /// Regression: a DELETE `If-Match` race is a CAS failure, not an opaque
    /// `Other`. GC relies on `PreconditionFailed` to keep the marker and retry
    /// on a later pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conditional_delete_if_match_412_is_precondition_failed() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.starts_with("HEAD ") {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\netag: \"etag1\"\r\nx-amz-meta-walgit-incarnation: inc1\r\nconnection: close\r\n\r\n",
                        );
                    } else if req.starts_with("DELETE ") {
                        let body = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>PreconditionFailed</Code><Message>etag changed</Message><RequestId>req</RequestId><HostId>host</HostId></Error>";
                        let head = format!(
                            "HTTP/1.1 412 Precondition Failed\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = s.write_all(head.as_bytes());
                        let _ = s.write_all(body);
                    }
                });
            }
        });

        let cfg = walgit_config::StoreConfig {
            backend: walgit_config::StoreBackend::S3,
            bucket: "delete-412".into(),
            s3: walgit_config::S3Config {
                endpoint: format!("http://{addr}"),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                ..Default::default()
            },
            connect_timeout: Duration::from_millis(500),
            idle_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let store = S3Store::new(&cfg).unwrap();

        let err = ObjectStore::delete(&store, "k", Some(Version::new("etag1@inc1")))
            .await
            .unwrap_err();
        assert!(err.is_precondition_failed(), "{err:?}");
    }

    #[test]
    fn static_credentials_include_session_token_when_present() {
        let creds = static_credentials("access", "secret", Some("session".into()));

        assert_eq!(creds.access_key_id(), "access");
        assert_eq!(creds.secret_access_key(), "secret");
        assert_eq!(creds.session_token(), Some("session"));
    }

    #[test]
    fn static_credentials_work_without_session_token() {
        let creds = static_credentials("access", "secret", None);

        assert_eq!(creds.access_key_id(), "access");
        assert_eq!(creds.secret_access_key(), "secret");
        assert_eq!(creds.session_token(), None);
    }
}
