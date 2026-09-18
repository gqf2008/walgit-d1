//! Read-only JSON API for the web UI (`web/API.md`, v2 contract).
//!
//! Two URL classes: ref-dependent (`refs`, `refs/{branches,tags,all,collab}`,
//! `refs/name/{rest}`, `resolve`, name-addressed tree/blob/commits/commit)
//! answered from a
//! per-manifest-version [`RefIndex`] with `stale-while-revalidate` + `ETag`,
//! and sha-addressed immutable ones (`tree/<sha>`, `blob/<sha>`,
//! `commits?ref=<sha>`, `commit/<sha>`) rendered once and cached in memory
//! (and, for remotely served repos, in the object store so every instance
//! shares one render cache).
//!
//! Object access (`Need::Objects`) goes through [`RepoHandle::sync_objects`]:
//! packs on disk when they fit this instance, otherwise the remote reader
//! (pack indexes local, data by range read; `web/objects.rs` faults what a
//! git command will touch into the loose store). Anything that cannot answer
//! immediately streams the SSE envelope (`crate::sse`) when the client
//! accepts it: notices + progress from the repo's task channel, then
//! `result`/`error`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt;
use serde::Serialize;
use walgit_store::{GetOptions, ObjectStore, Prefixed, PutBody, PutMode};
use walgit_wal::{
    ObjectAccess, RepoHandle, Reporter,
    collab::{
        BOARD_PATH, Board, BoardDef, Entry, EntryRef, MergeRules, build_board, build_report,
        default_board, merge_rule_eval, parse_board_def, pr_view, thread,
    },
};

use crate::sse::Rendered;
use crate::web::objects::{CommitMeta, Remote};
use crate::{AppState, cache::RefIndex, error::ApiError};

const MAX_BLOB: i64 = 2 * 1024 * 1024;

/// Cap on `?raw`. A blob is faulted whole (the remote reader has no range
/// reader), so this bounds what one request can materialize on a tmpfs host.
/// Real streaming for multi-GB media is a follow-up, not a silently-broken
/// viewer: over the cap the endpoint answers 413 instead of pretending.
const RAW_BLOB_MAX: i64 = 32 * 1024 * 1024;

/// Repository HTML/SVG is untrusted and must never execute on the app's origin
/// (a stored-XSS hole straight into every signed-in session). `sandbox` with no
/// allowances neuters scripts *and* same-origin access even when the URL is
/// opened directly; the viewer additionally frames HTML with `sandbox=""`.
const ACTIVE_CONTENT_CSP: &str =
    "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'";
const IMMUTABLE: &str = "private, max-age=31536000, immutable";
const SWR: &str = "private, max-age=0, stale-while-revalidate=60";
const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 1000;
/// Store key prefix (inside the repo prefix) of the shared render cache.
const SHARED_CACHE_PREFIX: &str = "cache/api/v1/";

#[derive(Serialize, Clone)]
pub(crate) struct RefInfo {
    pub(crate) name: String,
    pub(crate) sha: String,
}
#[derive(Serialize)]
struct Refs {
    head: Option<RefInfo>,
}
#[derive(Serialize)]
struct RefPage {
    refs: Vec<RefInfo>,
    more: bool,
}
#[derive(Serialize, Clone)]
struct Resolved {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    kind: &'static str,
}
#[derive(Serialize, Clone)]
struct Commit {
    sha: String,
    parents: Vec<String>,
    author: String,
    author_email: String,
    author_date: String,
    committer: String,
    /// Wire name is `commit_date` (the API contract); the field is renamed
    /// only so it does not repeat the struct's name.
    #[serde(rename = "commit_date")]
    date: String,
    subject: String,
    /// The message body WITHOUT the trailer block (see `trailers`).
    body: String,
    /// Git trailers of the message (`Key: value` lines of the last paragraph,
    /// `git interpret-trailers --parse` rules), in order.
    trailers: Vec<super::trailers::Trailer>,
}
impl From<CommitMeta> for Commit {
    fn from(m: CommitMeta) -> Self {
        let (body, trailers) = super::trailers::split_trailers(&m.body);
        Commit {
            sha: m.id.to_string(),
            parents: m
                .parents
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            author: m.author,
            author_email: m.author_email,
            author_date: m.author_date,
            committer: m.committer,
            date: m.commit_date,
            subject: m.subject,
            body,
            trailers,
        }
    }
}
impl Commit {
    fn with_body(mut self, raw: &str) -> Self {
        let (body, trailers) = super::trailers::split_trailers(raw.trim());
        self.body = body;
        self.trailers = trailers;
        self
    }
}
#[derive(Serialize)]
struct Tree {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    entries: Vec<TreeEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<Commit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readme: Option<Readme>,
}
#[derive(Serialize)]
struct TreeEntry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    mode: String,
    size: i64,
    sha: String,
}
#[derive(Serialize)]
struct Blob {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    name: String,
    size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    contents: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    too_large: Option<bool>,
}
#[derive(Serialize)]
struct Readme {
    name: String,
    contents: String,
}
#[derive(Serialize)]
struct Commits {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    /// Wire name is `commits` (the API contract); the field is renamed only
    /// so it does not repeat the struct's name.
    #[serde(rename = "commits")]
    items: Vec<Commit>,
    more: bool,
}
#[derive(Serialize)]
struct Stat {
    path: String,
    additions: i64,
    deletions: i64,
}
#[derive(Serialize)]
struct CommitDetail {
    commit: Commit,
    stats: Vec<Stat>,
    patch: String,
}
#[derive(serde::Deserialize, Default)]
struct CommitQuery {
    #[serde(rename = "ref")]
    ref_: Option<String>,
    path: Option<String>,
    skip: Option<usize>,
    n: Option<usize>,
}
#[derive(serde::Deserialize, Default)]
struct BlobQuery {
    raw: Option<String>,
}
#[derive(serde::Deserialize, Default)]
struct RefListQuery {
    prefix: Option<String>,
    q: Option<String>,
    after: Option<String>,
    n: Option<usize>,
}

pub fn router(state: Arc<AppState>) -> Router {
    // D26/D27: repo-scoped endpoints live under the repository's own prefix,
    // `/{owner}/{repo}/api/…` (bearer/session lane) and `/{owner}/{repo}/api-browser/…`
    // (browser lane: another origin with `credentials: "include"`);
    // same handlers, the lane differs by credential handling only. Non-repo
    // endpoints keep `/services/api/owners*` and `/api/v1/*`.
    let mut r = Router::new()
        .route("/services/api/instance", get(instance_info))
        .route("/services/api/owners", get(owners))
        .route("/services/api/owners/{owner}", get(owner_repos));
    for base in REPO_API_BASES {
        r = r
            .route(&format!("{base}/refs"), get(refs))
            .route(&format!("{base}/refs/{{kind}}"), get(ref_list))
            .route(&format!("{base}/refs/name/{{*rest}}"), get(ref_by_name))
            .route(&format!("{base}/merge-base"), get(merge_base))
            .route(&format!("{base}/diff"), get(diff))
            .route(&format!("{base}/collab/entries"), post(collab_entries))
            .route(&format!("{base}/collab/principal"), post(collab_principal))
            .route(&format!("{base}/collab/report"), get(collab_report))
            .route(&format!("{base}/collab/board"), get(collab_board))
            .route(&format!("{base}/collab/threads/{{id}}"), get(collab_thread))
            .route(
                &format!("{base}/collab/ci-artifacts/{{sha256}}"),
                get(collab_ci_artifact).head(collab_ci_artifact_size),
            )
            .route(&format!("{base}/blame/{{*rest}}"), get(blame))
            .route(&format!("{base}/archive/{{*rest}}"), get(archive))
            .route(&format!("{base}/resolve"), get(resolve_root))
            .route(&format!("{base}/resolve/"), get(resolve_root))
            .route(&format!("{base}/resolve/{{*rest}}"), get(resolve))
            .route(&format!("{base}/tree/{{*rest}}"), get(tree))
            .route(&format!("{base}/blob/{{*rest}}"), get(blob))
            .route(&format!("{base}/commits"), get(commits))
            .route(&format!("{base}/commit/{{sha}}"), get(commit_detail));
    }
    r.with_state(state)
}

/// Route prefixes of the repo-scoped JSON API (D27): one per lane, both
/// *after* the repository prefix. No lane-first forms, no aliases (banner).
pub const REPO_API_BASES: [&str; 2] = ["/{owner}/{repo}/api", "/{owner}/{repo}/api-browser"];
fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError::NotFound(msg.into())
}
fn internal<E: std::fmt::Display>(e: E) -> ApiError {
    ApiError::Internal(e.to_string())
}

/// One synced view of a repository for the duration of a request.
pub struct Repo {
    id: String,
    local: walgit_git::LocalRepo,
    #[allow(dead_code)]
    version: String,
    pub(crate) index: Arc<RefIndex>,
    handle: Arc<RepoHandle>,
    access: ObjectAccess,
    /// Whether objects are readable (`Need::Objects` satisfied).
    objects: bool,
    reporter: Reporter,
    /// Shared render cache (object store) — set for remotely served repos.
    shared: Option<Prefixed>,
}
impl Repo {
    fn remote(&self) -> Option<Remote> {
        match &self.access {
            ObjectAccess::Remote(r) if self.objects => Some(Remote::new(
                r.clone(),
                self.local.clone(),
                self.reporter.clone(),
            )),
            _ => None,
        }
    }
    /// Upgrade a refs-level view to objects (used by `resolve` for raw revisions).
    async fn need_objects(&mut self, st: &AppState) -> Result<(), ApiError> {
        if self.objects {
            return Ok(());
        }
        let (guard, access) = self
            .handle
            .sync_objects()
            .await
            .map_err(|e| crate::smart::wal_err(&e))?;
        drop(guard);
        self.objects = true;
        self.access = access;
        self.shared = shared_cache(st, &self.handle, &self.access);
        Ok(())
    }
}

/// What a request needs from the local copy: refs only (cheap, always
/// possible) or objects too (packs on disk or the remote reader).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    Refs,
    Objects,
}

fn shared_cache(st: &AppState, handle: &RepoHandle, access: &ObjectAccess) -> Option<Prefixed> {
    (st.cfg.cache.shared_render_cache && access.is_remote()).then(|| handle.store().clone())
}

async fn open(
    st: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<Arc<RepoHandle>, ApiError> {
    st.auth
        .require_read(headers)
        .await
        .map_err(ApiError::from)?;
    let id = walgit_git::RepoId::new(owner, name).map_err(|_| not_found("repository"))?;
    st.registry.open(&id).await.map_err(|e| match e {
        walgit_wal::WalError::NotFound => not_found("repository"),
        _ => internal(e),
    })
}

async fn view(
    st: &AppState,
    handle: Arc<RepoHandle>,
    need: Need,
    reporter: Reporter,
) -> Result<Repo, ApiError> {
    let (guard, access, objects) = match need {
        Need::Refs => (
            handle
                .sync_refs()
                .await
                .map_err(|e| crate::smart::wal_err(&e))?,
            ObjectAccess::Local,
            false,
        ),
        Need::Objects => {
            let (g, a) = handle
                .sync_objects()
                .await
                .map_err(|e| crate::smart::wal_err(&e))?;
            (g, a, true)
        }
    };
    // The guard is held until the local handle has been cloned and the ref
    // index for this manifest version exists. The local repository itself is
    // thread-safe and subsequent git commands read its synced state.
    let local = handle.local().clone();
    let version = handle
        .manifest_version()
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();
    let id = handle.id().to_string();
    let index = st
        .caches
        .ref_index
        .get_or_build(&id, &version, || local.refs())
        .map_err(internal)?;
    drop(guard);
    let shared = shared_cache(st, &handle, &access);
    Ok(Repo {
        id,
        local,
        version,
        index,
        handle,
        access,
        objects,
        reporter,
        shared,
    })
}

fn shared_key(cache_key: &str) -> String {
    use sha1::Digest;
    let h = sha1::Sha1::digest(cache_key.as_bytes());
    format!("{SHARED_CACHE_PREFIX}{}.json", hex::encode(h))
}

/// Run one endpoint: auth + open, immutable caches, then either a plain
/// response or (when the answer needs long work and the client accepts it)
/// the SSE envelope streaming the repo's progress until the result.
pub(crate) async fn run<F, Fut>(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    need: Need,
    immutable_key: Option<String>,
    work: F,
) -> Result<Response, ApiError>
where
    F: FnOnce(Repo) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Rendered, ApiError>> + Send + 'static,
{
    let handle = open(st, headers, owner, name).await?;
    let slow = need == Need::Objects && !handle.packs_ready();
    if let Some(key) = &immutable_key {
        if let Some(hit) = st.caches.api_immutable.get(key) {
            metrics::counter!("walgit_api_immutable_hit", "tier" => "memory").increment(1);
            return Ok(Rendered::json(hit, IMMUTABLE, None).into_response(headers));
        }
        if slow
            && st.cfg.cache.shared_render_cache
            && let Ok(walgit_store::GetResult::Object { body, meta }) = handle
                .store()
                .get(&shared_key(key), GetOptions::default())
                .await
            && let Ok(b) =
                walgit_store::util::collect(body, usize::try_from(meta.size).unwrap_or(usize::MAX))
                    .await
        {
            metrics::counter!("walgit_api_immutable_hit", "tier" => "store").increment(1);
            st.caches.api_immutable.insert(key.clone(), b.clone());
            return Ok(Rendered::json(b, IMMUTABLE, None).into_response(headers));
        }
    }
    if slow && crate::sse::wants_sse(headers) {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        let sources = vec![handle.subscribe_progress(), rx];
        let st2 = st.clone();
        let fut = async move {
            let repo = view(&st2, handle, need, Reporter::for_repo(tx)).await?;
            work(repo).await
        };
        return Ok(crate::sse::envelope(sources, fut));
    }
    let repo = view(st, handle, need, Reporter::none()).await?;
    Ok(work(repo).await?.into_response(headers))
}

// ---- response helpers --------------------------------------------------------

pub(crate) fn etag_for(sha: &str) -> String {
    format!("\"{sha}\"")
}
pub(crate) fn json_swr<T: Serialize>(value: &T, etag: Option<&str>) -> Rendered {
    Rendered::json(json_bytes(value), SWR, etag.map(str::to_string))
}
fn json_bytes<T: Serialize>(value: &T) -> bytes::Bytes {
    bytes::Bytes::from(serde_json::to_vec(value).unwrap_or_default())
}
fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Finish a ref-or-sha addressed request: immutable (+LRU +shared cache) or
/// SWR+ETag (304 handled by `Rendered::into_response`).
fn finish(
    st: &AppState,
    r: &Repo,
    immutable: bool,
    cache_key: &str,
    sha: &str,
    body: bytes::Bytes,
) -> Rendered {
    if immutable {
        st.caches
            .api_immutable
            .insert(cache_key.to_string(), body.clone());
        if let Some(store) = &r.shared {
            let store = store.clone();
            let key = shared_key(cache_key);
            let b = body.clone();
            tokio::spawn(async move {
                if let Err(e) = store
                    .put(&key, PutBody::Bytes(b), PutMode::Overwrite.into())
                    .await
                {
                    tracing::debug!(error = %e, key, "shared render cache put failed");
                }
            });
        }
        return Rendered::json(body, IMMUTABLE, None);
    }
    Rendered::json(body, SWR, Some(etag_for(sha)))
}

// ---- instance ----------------------------------------------------------------

/// Which instance answered (kind/name/revision/build) — footer of the UI. Not
/// cached: every response should reflect the machine that produced it.
async fn instance_info(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    st.auth
        .require_read(&headers)
        .await
        .map_err(ApiError::from)?;
    let mut r = axum::Json(crate::instance::info(&st.cfg)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

// ---- owners ------------------------------------------------------------------

pub(crate) async fn owners(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    st.auth
        .require_read(&headers)
        .await
        .map_err(ApiError::from)?;
    let repos = st.registry.list().await.map_err(internal)?;
    let mut out: Vec<String> = repos.into_iter().map(|r| r.owner().to_string()).collect();
    out.sort();
    out.dedup();
    Ok(json_swr(&out, None).into_response(&headers))
}
pub(crate) async fn owner_repos(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(owner): Path<String>,
) -> Result<Response, ApiError> {
    st.auth
        .require_read(&headers)
        .await
        .map_err(ApiError::from)?;
    let repos = st.registry.list().await.map_err(internal)?;
    let mut out: Vec<String> = repos
        .into_iter()
        .filter(|r| r.owner() == owner)
        .map(|r| r.name().to_string())
        .collect();
    out.sort();
    out.dedup();
    Ok(json_swr(&out, None).into_response(&headers))
}

// ---- refs --------------------------------------------------------------------

async fn refs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Refs,
        None,
        |r| async move {
            let head = r.index.head().map(|(name, sha)| RefInfo { name, sha });
            let etag = etag_for(head.as_ref().map_or("unborn", |h| h.sha.as_str()));
            Ok(json_swr(&Refs { head }, Some(&etag)))
        },
    )
    .await
}

/// A byte-sorted ref namespace: short-name pairs (`branches`/`tags`) or
/// full-name triples (`all`, `refs/collab/*`). Index-based access keeps a
/// 466 k-ref repository paged in O(page) without copying the namespace.
enum RefSlice<'a> {
    Pairs(&'a [(String, String)]),
    All(&'a [(String, String)]),
}

impl RefSlice<'_> {
    fn at(&self, i: usize) -> Option<(&str, &str)> {
        match self {
            RefSlice::Pairs(v) => v.get(i).map(|(n, s)| (n.as_str(), s.as_str())),
            RefSlice::All(v) => v.get(i).map(|(n, s)| (n.as_str(), s.as_str())),
        }
    }
    /// First index whose name sorts strictly after `x` (names byte-sorted).
    fn after(&self, x: &str) -> usize {
        match self {
            RefSlice::Pairs(v) => v.partition_point(|(n, _)| n.as_str() <= x),
            RefSlice::All(v) => v.partition_point(|(n, _)| n.as_str() <= x),
        }
    }
    /// First index whose name sorts at-or-after `x`.
    fn at_or_after(&self, x: &str) -> usize {
        match self {
            RefSlice::Pairs(v) => v.partition_point(|(n, _)| n.as_str() < x),
            RefSlice::All(v) => v.partition_point(|(n, _)| n.as_str() < x),
        }
    }
}

async fn ref_list(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, kind)): Path<(String, String, String)>,
    Query(q): Query<RefListQuery>,
) -> Result<Response, ApiError> {
    let wants_sse = crate::sse::wants_sse(&headers);
    let handle = open(&st, &headers, &owner, &repo_name).await?;
    let r = view(&st, handle, Need::Refs, Reporter::none()).await?;
    // `branches`/`tags` are short-name namespaces; `all` is every ref as its
    // full name; `collab` is the D1 collaboration namespace (`refs/collab/*`),
    // also full names. All are byte-sorted, so pagination is index arithmetic
    // over the cached index, never a copy.
    let (slice, ns) = match kind.as_str() {
        "branches" => (RefSlice::Pairs(&r.index.branches), None),
        "tags" => (RefSlice::Pairs(&r.index.tags), None),
        "all" => (RefSlice::All(&r.index.all), None),
        "collab" => (RefSlice::All(&r.index.all), Some("refs/collab/")),
        _ => return Err(not_found("ref namespace")),
    };
    let n = q.n.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let prefix = q
        .prefix
        .as_deref()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}/"));
    let needle =
        q.q.as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);
    let after = q.after.as_deref().unwrap_or("");
    // Byte-sorted: skip straight to the first candidate (> after, >= prefix,
    // >= namespace start). The namespace folds into `lower` so the collab view
    // is O(log n + page), not a linear skip past refs/heads and refs/tags.
    let ns_start = ns.unwrap_or("");
    let lower = match &prefix {
        Some(p) if p.as_str() > after => p.as_str(),
        _ => after,
    };
    let lower = if lower < ns_start { ns_start } else { lower };
    let start = slice.at_or_after(lower).max(slice.after(after));
    let mut refs = Vec::with_capacity(n.min(256));
    let mut more = false;
    let mut i = start;
    while let Some((name, sha)) = slice.at(i) {
        if let Some(ns) = ns
            && !name.starts_with(ns)
        {
            break; // sorted: past the namespace (start already >= ns)
        }
        if let Some(p) = &prefix
            && !name.starts_with(p.as_str())
        {
            break; // sorted: no further names share the prefix
        }
        if let Some(nd) = &needle
            && !name.to_ascii_lowercase().contains(nd.as_str())
        {
            i += 1;
            continue;
        }
        if refs.len() == n {
            more = true;
            break;
        }
        refs.push(RefInfo {
            name: name.to_string(),
            sha: sha.to_string(),
        });
        i += 1;
    }
    if wants_sse {
        // Streamed form: one `ref` packet per ref, then `done` (web/API.md).
        let mut items: Vec<Result<bytes::Bytes, std::convert::Infallible>> =
            Vec::with_capacity(refs.len() + 1);
        for r in &refs {
            items.push(Ok(crate::sse::packet("ref", r)));
        }
        items.push(Ok(crate::sse::packet(
            "done",
            &serde_json::json!({ "more": more }),
        )));
        let mut resp = crate::sse::sse_response(futures::stream::iter(items));
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(SWR));
        return Ok(resp);
    }
    Ok(json_swr(&RefPage { refs, more }, None).into_response(&headers))
}

/// Exact full-ref lookup (`refs/name/{rest}`), e.g. the tip of a D1 collab
/// inbox. The sha is the raw oid (no peeling); 404 when the ref does not exist.
async fn ref_by_name(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Refs,
        None,
        |r| async move {
            match r.index.by_name.get(&name) {
                Some((sha, _)) => {
                    let etag = etag_for(sha.as_str());
                    Ok(json_swr(
                        &RefInfo {
                            name,
                            sha: sha.clone(),
                        },
                        Some(&etag),
                    ))
                }
                None => Err(not_found("ref")),
            }
        },
    )
    .await
}

#[derive(serde::Deserialize, Default)]
struct MergeBaseQuery {
    from: String,
    to: String,
}

/// The merge base of two revisions (`?from=&to=`) — the diff base for 3-dot
/// PR comparisons. Local packs use `git merge-base`; remote-served bases use a
/// bounded bidirectional walk over the pack set (`Remote::merge_base`).
/// `merge_base` is `null` when the histories are unrelated.
async fn merge_base(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    Query(q): Query<MergeBaseQuery>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        |r| async move {
            let a = resolve_name(&r, &q.from).await?.sha;
            let b = resolve_name(&r, &q.to).await?.sha;
            let base = if let Some(remote) = r.remote() {
                remote.merge_base(&a, &b).await?.map(|oid| oid.to_string())
            } else {
                let out = r
                    .local
                    .git(&["merge-base", a.as_str(), b.as_str()])
                    .await
                    .map_err(internal)?;
                if out.status.success() {
                    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    (!sha.is_empty()).then_some(sha)
                } else if out.status.code() == Some(1) {
                    None // git: exit 1 = no common ancestor
                } else {
                    // Any other failure (corrupt objects, IO) is a server-side
                    // condition, not a missing revision.
                    return Err(internal(
                        String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    ));
                }
            };
            let etag = etag_for(&format!("{a}:{b}:{}", base.as_deref().unwrap_or("")));
            Ok(json_swr(
                &serde_json::json!({ "from": a, "to": b, "merge_base": base }),
                Some(&etag),
            ))
        },
    )
    .await
}

#[derive(serde::Deserialize, Default)]
struct DiffQuery {
    from: String,
    to: String,
    #[serde(default = "default_diff_format")]
    format: String,
}

fn default_diff_format() -> String {
    "patch".to_string()
}

#[derive(Serialize)]
struct NameStatus {
    status: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
}

#[derive(Serialize)]
struct DiffResult {
    from: String,
    to: String,
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<Vec<Stat>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<Vec<NameStatus>>,
}

/// The tree diff between two revisions (`?from=&to=&format=patch|stat|
/// name-status`), default `patch`. Local packs run `git diff` directly;
/// remote-served bases fault exactly the trees/blobs the diff touches into
/// the loose store first (`Remote::fault_diff`) and then run the same git.
async fn diff(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    Query(q): Query<DiffQuery>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let a = resolve_name(&r, &q.from).await?.sha;
            let b = resolve_name(&r, &q.to).await?.sha;
            match q.format.as_str() {
                "patch" | "stat" | "name-status" => {}
                other => return Err(not_found(format!("unknown diff format {other}"))),
            }
            if let Some(remote) = r.remote() {
                remote.fault_diff(&a, &b).await?;
            }
            Ok(json_swr(
                &git_diff(&r.local, &a, &b, &q.format).await?,
                None,
            ))
        },
    )
    .await
}

/// Run `git diff <a> <b>` and shape the output for the requested format.
async fn git_diff(
    local: &walgit_git::LocalRepo,
    a: &str,
    b: &str,
    format: &str,
) -> Result<DiffResult, ApiError> {
    let mut out = DiffResult {
        from: a.to_string(),
        to: b.to_string(),
        format: format.to_string(),
        patch: None,
        stats: None,
        changes: None,
    };
    match format {
        "patch" => {
            let patch = String::from_utf8_lossy(
                &git(
                    local,
                    vec![
                        "diff".into(),
                        "--no-color".into(),
                        "--no-ext-diff".into(),
                        "-M".into(),
                        a.into(),
                        b.into(),
                    ],
                )
                .await?,
            )
            .into_owned();
            out.patch = Some(patch);
        }
        "stat" => {
            let stats = parse_stats(
                &git(
                    local,
                    vec![
                        "diff".into(),
                        "--numstat".into(),
                        "-M".into(),
                        a.into(),
                        b.into(),
                    ],
                )
                .await?,
            );
            out.stats = Some(stats);
        }
        "name-status" => {
            let raw = git(
                local,
                vec![
                    "diff".into(),
                    "--name-status".into(),
                    "-M".into(),
                    a.into(),
                    b.into(),
                ],
            )
            .await?;
            out.changes = Some(parse_name_status(&raw));
        }
        _ => return Err(not_found("unknown diff format")),
    }
    Ok(out)
}

/// `git diff --name-status -M`: `STATUS\tpath` or `R<score>\told\tnew`.
fn parse_name_status(bytes: &[u8]) -> Vec<NameStatus> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let (status, path, old_path) = match f.as_slice() {
                [s, p] => (s, p, None),
                [s, old, p] => (s, p, Some(*old)),
                _ => return None,
            };
            Some(NameStatus {
                status: status.get(..1).unwrap_or("?").to_string(),
                path: (*path).to_string(),
                old_path: old_path.map(str::to_string),
            })
        })
        .collect()
}

#[derive(Serialize)]
struct BlameLine {
    line: u32,
    commit: String,
    author: String,
    author_email: String,
    time: i64,
    summary: String,
    text: String,
}

#[derive(Serialize)]
struct BlameResult {
    sha: String,
    path: String,
    blame: Vec<BlameLine>,
}

/// Line attribution for one file (`blame/{rev}/{path}`): `git blame
/// --porcelain` parsed to JSON. Local packs run git directly; remote-served
/// bases fault the path history first (`Remote::fault_blame`, bounded) and
/// then run the same git — same objects, same answer.
async fn blame(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        |r| async move {
            let res = resolve_rest(&r, &rest).await?;
            if res.path.is_empty() {
                return Err(not_found("blame needs a file path"));
            }
            if let Some(remote) = r.remote() {
                remote.fault_blame(&res.sha, &res.path).await?;
            }
            let raw = git(
                &r.local,
                vec![
                    "blame".into(),
                    "--porcelain".into(),
                    res.sha.clone(),
                    "--".into(),
                    res.path.clone(),
                ],
            )
            .await?;
            let blame = parse_porcelain_blame(&raw);
            Ok(json_swr(
                &BlameResult {
                    sha: res.sha,
                    path: res.path,
                    blame,
                },
                None,
            ))
        },
    )
    .await
}

/// `git blame --porcelain`: per line a `<sha> <orig> <final> <count>` header,
/// key-value fields (author / author-mail / author-time / summary …), then
/// `\t<text>`; groups are separated by blank lines.
fn parse_porcelain_blame(bytes: &[u8]) -> Vec<BlameLine> {
    let mut out = Vec::new();
    let mut cur: Option<BlameLine> = None;
    for raw in String::from_utf8_lossy(bytes).lines() {
        if raw.is_empty() {
            continue;
        }
        if let Some(text) = raw.strip_prefix('\t') {
            if let Some(mut c) = cur.take() {
                c.text = text.to_string();
                out.push(c);
            }
            continue;
        }
        let parts: Vec<&str> = raw.splitn(4, ' ').collect();
        let sha = parts.first().copied().filter(|s| is_full_sha(s));
        let line = parts.get(2).and_then(|s| s.parse::<u32>().ok());
        if let (Some(sha), Some(line)) = (sha, line) {
            cur = Some(BlameLine {
                line,
                commit: sha.to_string(),
                author: String::new(),
                author_email: String::new(),
                time: 0,
                summary: String::new(),
                text: String::new(),
            });
            continue;
        }
        if let Some(c) = &mut cur {
            if let Some(v) = raw.strip_prefix("author ") {
                c.author = v.to_string();
            } else if let Some(v) = raw.strip_prefix("author-mail ") {
                c.author_email = v.trim_matches(['<', '>']).to_string();
            } else if let Some(v) = raw.strip_prefix("author-time ") {
                c.time = v.parse().unwrap_or(0);
            } else if let Some(v) = raw.strip_prefix("summary ") {
                c.summary = v.to_string();
            }
        }
    }
    out
}

#[derive(serde::Deserialize, Default)]
struct ArchiveQuery {
    #[serde(default = "default_archive_format")]
    format: String,
}

fn default_archive_format() -> String {
    "tar.gz".to_string()
}

/// A tree archive (`archive/{rev}?format=tar.gz|zip`, default `tar.gz`) as a
/// binary download — the whole tree at one revision. Local packs run `git
/// archive` directly; remote-served bases fault the whole tree first
/// (`Remote::fault_tree_all`, bounded — larger trees get a 503 pointing at
/// bundle-uri, where big-repo downloads belong anyway).
async fn archive(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
    Query(q): Query<ArchiveQuery>,
) -> Result<Response, ApiError> {
    // Binary download: never the SSE envelope.
    let mut plain_headers = headers.clone();
    plain_headers.remove(header::ACCEPT);
    run(
        &st,
        &plain_headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let rev = rest.trim_matches('/');
            let res = resolve_name(&r, rev).await?;
            let format = match q.format.as_str() {
                "tar.gz" | "zip" => q.format,
                other => return Err(not_found(format!("unknown archive format {other}"))),
            };
            if let Some(remote) = r.remote() {
                remote.fault_tree_all(&res.sha).await?;
            }
            let bytes = git(
                &r.local,
                vec![
                    "archive".into(),
                    format!("--format={format}"),
                    res.sha.clone(),
                ],
            )
            .await?;
            let content_type: &'static str = if format == "zip" {
                "application/zip"
            } else {
                "application/gzip"
            };
            Ok(Rendered {
                body: bytes::Bytes::from(bytes),
                content_type,
                cache_control: SWR,
                etag: Some(etag_for(&format!("{}:{format}", res.sha))),
                extra: Vec::new(),
                range: None,
            })
        },
    )
    .await
}

// ---- D1 collab thin-API write path (browser writes; D1 §11) -------------------

#[derive(serde::Deserialize)]
struct CollabPost {
    entry: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct CollabPrincipalPost {
    principal: String,
    public_key: String,
}

pub(crate) fn ref_segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.contains("..") // git forbids `..` inside a component; the WAL
        // publish path does not re-check refnames, so an invalid one would only
        // explode when a replica materializes packed-refs.
        && s != "."
        && s != ".."
        && !s.to_ascii_lowercase().ends_with(".lock")
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
}

async fn git_hash_object(
    local: &walgit_git::LocalRepo,
    content: &[u8],
) -> Result<String, ApiError> {
    use tokio::io::AsyncWriteExt;
    let mut child = walgit_git::git_tokio_command()
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(internal)?;
    child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("hash-object stdin".into()))?
        .write_all(content)
        .await
        .map_err(internal)?;
    let out = child.wait_with_output().await.map_err(internal)?;
    if !out.status.success() {
        return Err(internal(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn git_pack_object(local: &walgit_git::LocalRepo, oid: &str) -> Result<Vec<u8>, ApiError> {
    use tokio::io::AsyncWriteExt;
    let mut child = walgit_git::git_tokio_command()
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(internal)?;
    child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("pack-objects stdin".into()))?
        .write_all(format!("{oid}\n").as_bytes())
        .await
        .map_err(internal)?;
    let out = child.wait_with_output().await.map_err(internal)?;
    if !out.status.success() {
        return Err(internal(format!(
            "git pack-objects failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Accept a signed collab entry, materialize it as a one-object bucket pack and
/// publish `refs/collab/inbox/<actor>/<uuid>` through the WAL — the
/// receive-pack-equivalent that lets a browser write (D1 §11 thin API).
/// Verification is client-side (signatures over the canonical form); the
/// server enforces identity (actor == the authenticated principal) and that
/// the ref lands in the actor's own inbox.
async fn collab_entries(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let principal = st
        .auth
        .require_write(&headers)
        .await
        .map_err(ApiError::from)?;
    // Parse after auth: the raw-bytes extractor keeps a body-less challenge
    // probe (an edge's sign-in fallback strips the body; the SDK's 401 dance)
    // answerable with the 401 it needs — `Json<>` would reject it first with
    // a 400 no client retries credentials on (#93 review).
    let body: CollabPost = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("invalid collab post: {e}")))?;
    let handle = open(&st, &headers, &owner, &repo_name).await?;
    let r = view(&st, handle.clone(), Need::Refs, Reporter::none()).await?;
    let entry = body.entry;
    let actor = entry
        .get("actor")
        .and_then(|a| a.as_str())
        .ok_or_else(|| ApiError::BadRequest("entry.actor is required".into()))?;
    // Auth `none` (loopback dev) has no distinct identity; otherwise the entry
    // must be posted by its own principal (no posting as someone else).
    if !principal.anonymous && actor != principal.name {
        return Err(ApiError::Forbidden);
    }
    if !ref_segment_ok(actor) {
        return Err(ApiError::BadRequest(format!(
            "actor {actor:?} is not a refname-safe segment"
        )));
    }
    // Transition 门禁 (issue #75 ①, 方案 A):status=done 需前置 needs-review
    // + verified approve review;其余流转自由。非法流转 400 机器可读错误。
    if entry.get("kind").and_then(|v| v.as_str()) == Some("status")
        && entry
            .get("body")
            .and_then(|b| b.get("status"))
            .and_then(|v| v.as_str())
            == Some("done")
    {
        let state = collab_load(&st, &r).await?;
        let thread_refs: Vec<&walgit_wal::collab::EntryRef> = state
            .entries
            .iter()
            .filter(|e| e.entry.id == entry.get("id").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        // 线程序先行(issue #104 观察 A):card_status 按给定顺序重放,
        // 未 thread() 的多 status 条目线程会误判当前状态。
        let thread_refs = walgit_wal::collab::thread(&thread_refs);
        if let Err(e) =
            walgit_wal::collab::validate_status_transition(&thread_refs, &state.principals)
        {
            return Err(ApiError::BadRequest(e));
        }
    }
    let content = serde_json::to_vec(&entry).map_err(internal)?;
    let ref_name = format!("refs/collab/inbox/{actor}/{}", uuid::Uuid::new_v4());
    let (oid, seq) =
        publish_collab_ref(&st, handle, &r, &principal.name, &ref_name, content).await?;
    Ok(json_swr(
        &serde_json::json!({ "ref": ref_name, "oid": oid, "seq": seq }),
        None,
    )
    .into_response(&headers))
}

/// Upper bound on the collab namespace one aggregation request may read: the
/// inbox is folded by `walgit collab gc` (D45 / D1 §11.4), so this counts only
/// the **unfolded** tail plus the principals registry — past this size the
/// answer is a 503 pointing at gc / the CLI, not an unbounded fan-out of
/// faults and objects. The snapshot itself is one ref + one bounded blob
/// (`COLLAB_SNAPSHOT_MAX_BYTES`).
const COLLAB_MAX_ENTRIES: usize = 20_000;

/// Bound on the folded-history blob at `refs/collab/meta/snapshot` (D45): the
/// fold turns N historical refs into one ref + one blob, and this cap keeps
/// the blob bounded work for the remote reader and the parser.
const COLLAB_SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// One `git cat-file --batch` for many oids: a process per entry made the
/// aggregation O(refs) subprocesses per request. Requests go out in small
/// chunks so neither pipe can fill while the other side waits; a missing or
/// unreadable oid is simply absent from the map (the aggregation skips
/// unparsable entries anyway).
async fn git_cat_file_batch(
    local: &walgit_git::LocalRepo,
    oids: &[String],
) -> Result<HashMap<String, Vec<u8>>, ApiError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    const CHUNK: usize = 512;
    let mut child = walgit_git::git_tokio_command()
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(internal)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("cat-file stdin".into()))?;
    let mut stdout = tokio::io::BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| ApiError::Internal("cat-file stdout".into()))?,
    );
    let mut out: HashMap<String, Vec<u8>> = HashMap::new();
    for chunk in oids.chunks(CHUNK) {
        let mut req = String::with_capacity(chunk.len() * 41);
        for oid in chunk {
            req.push_str(oid);
            req.push('\n');
        }
        stdin.write_all(req.as_bytes()).await.map_err(internal)?;
        stdin.flush().await.map_err(internal)?;
        for oid in chunk {
            let mut header = String::new();
            let n = stdout.read_line(&mut header).await.map_err(internal)?;
            if n == 0 {
                return Err(ApiError::Internal("cat-file --batch closed early".into()));
            }
            let header = header.trim_end();
            let mut it = header.split(' ');
            let (got, kind, size) = (
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
                it.next().unwrap_or(""),
            );
            if kind == "missing" || got != *oid {
                continue;
            }
            let size: usize = size
                .parse()
                .map_err(|_| ApiError::Internal(format!("cat-file header {header:?}")))?;
            let mut body = vec![0u8; size];
            stdout.read_exact(&mut body).await.map_err(internal)?;
            let mut nl = [0u8; 1];
            stdout.read_exact(&mut nl).await.map_err(internal)?;
            out.insert(oid.clone(), body);
        }
    }
    drop(stdin);
    let _ = child.wait().await;
    Ok(out)
}

/// Git object size without reading its body. Remote objects are probed through
/// the store; local objects use `git cat-file -s`. Both paths let callers
/// enforce a byte cap before any materialization.
async fn object_size(r: &Repo, oid: &str) -> Result<Option<u64>, ApiError> {
    if let Some(remote) = r.remote() {
        let oid = gix_hash::ObjectId::from_hex(oid.as_bytes())
            .map_err(|_| internal(format!("invalid object id {oid:?}")))?;
        return Ok(remote.kind_and_size(&oid).await?.map(|(_, size)| size));
    }
    let out = r.local.git(&["cat-file", "-s", oid]).await.map_err(internal)?;
    if !out.status.success() {
        return Ok(None);
    }
    let size = std::str::from_utf8(&out.stdout)
        .map_err(|e| internal(format!("cat-file -s {oid}: {e}")))?
        .trim()
        .parse::<u64>()
        .map_err(|e| internal(format!("cat-file -s {oid}: {e}")))?;
    Ok(Some(size))
}

/// Materialize a JSON blob as a one-object bucket pack and publish one ref
/// through the WAL (D1 §11 thin API). Shared by inbox entries and principal
/// registration; returns `(oid, seq)`.
async fn publish_collab_ref(
    st: &Arc<AppState>,
    handle: Arc<RepoHandle>,
    r: &Repo,
    principal_name: &str,
    ref_name: &str,
    content: Vec<u8>,
) -> Result<(String, u64), ApiError> {
    let oid = git_hash_object(&r.local, &content).await?;
    let pack_bytes = git_pack_object(&r.local, &oid).await?;
    let tmp = std::env::temp_dir().join(format!("walgit-collab-{}.pack", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, &pack_bytes)
        .await
        .map_err(internal)?;
    let f = tokio::fs::File::open(&tmp).await.map_err(internal)?;
    // Thin collab-ref publish also installs a pack before its manifest CAS.
    let _prune_guard = handle.prune_guard().await;
    let pack = r
        .local
        .ingest_pack(
            f,
            walgit_git::IngestOptions {
                fsck: st.cfg.wal.fsck_objects,
                max_bytes: None,
                thin: false,
            },
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError::Internal("ingest produced no pack".into()))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    // Honor the repo's own value as `old`: registration of an existing
    // principal is an update (CAS), a fresh inbox ref a create — exactly what
    // receive-pack would have parsed from the client's command line.
    let old_oid = r
        .index
        .by_name
        .get(ref_name)
        .map(|(sha, _)| sha.clone())
        .unwrap_or_default();
    let txn = walgit_proto::v1::RefTransaction {
        updates: vec![walgit_proto::v1::RefUpdate {
            name: ref_name.to_string(),
            old_oid,
            new_oid: oid.clone(),
            new_symbolic_target: String::new(),
            new_peeled: String::new(),
        }],
        push_options: Vec::new(),
        atomic: true,
    };
    // The same authorization gate as receive-pack (D16): load policy, classify
    // non-fast-forward moves when anything is protected, evaluate the
    // transaction. `refs/collab/*` protection configured by an admin binds the
    // browser path too — one ref, one guard level, no second write semantics.
    let policy = crate::policy::load(&st.store, handle.id())
        .await
        .map_err(|e| internal(format!("load policy: {e}")))?;
    let mut forces = std::collections::HashSet::<String>::new();
    if policy.has_protect() {
        for u in &txn.updates {
            if crate::policy::classify(&u.old_oid, &u.new_oid) == crate::policy::RefOp::Update
                && !matches!(r.local.is_ancestor(&u.old_oid, &u.new_oid).await, Ok(true))
            {
                forces.insert(u.name.clone());
            }
        }
    }
    let ev = crate::policy::evaluate(&policy, principal_name, &txn, |u| forces.contains(&u.name));
    if let Some((_, Err(reason))) = ev.per_ref.iter().find(|(name, _)| name == ref_name) {
        tracing::warn!(repo = %handle.id(), ref = ref_name, %reason, "collab thin API: policy denied");
        return Err(ApiError::Forbidden);
    }
    let mut meta = std::collections::HashMap::new();
    meta.insert("principal".to_string(), principal_name.to_string());
    meta.insert("agent".to_string(), "walgit-web".into());
    let result = handle
        .publish_push_synced(Some(pack), ev.publish, meta)
        .await
        .map_err(|e| crate::smart::wal_err(&e))?;
    for (_, res) in &result.per_ref {
        if let Err(e) = res {
            return Err(internal(format!("publish ref: {e}")));
        }
    }
    Ok((oid, result.seq))
}

/// First-use self-registration of the authenticated principal's Ed25519 public
/// key at `refs/collab/meta/principals/<principal>` (D1 §5): the token binds
/// the principal, this ref binds the key. Registration is one-directional
/// (the tombstone is `revokePrincipal` via git); re-registration overwrites
/// with the new key.
async fn collab_principal(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let principal = st
        .auth
        .require_write(&headers)
        .await
        .map_err(ApiError::from)?;
    // Parse after auth — same reason as `collab_entries` (#93 review).
    let body: CollabPrincipalPost = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("invalid principal post: {e}")))?;
    if !principal.anonymous && body.principal != principal.name {
        return Err(ApiError::Forbidden);
    }
    if !ref_segment_ok(&body.principal) {
        return Err(ApiError::BadRequest(format!(
            "principal {:?} is not a refname-safe segment",
            body.principal
        )));
    }
    let handle = open(&st, &headers, &owner, &repo_name).await?;
    let r = view(&st, handle.clone(), Need::Refs, Reporter::none()).await?;
    let content = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "principal": body.principal,
        "public_key": body.public_key,
        "registered_at": chrono::Utc::now().timestamp(),
    }))
    .map_err(internal)?;
    let ref_name = format!("refs/collab/meta/principals/{}", body.principal);
    let (oid, seq) =
        publish_collab_ref(&st, handle, &r, &principal.name, &ref_name, content).await?;
    Ok(json_swr(
        &serde_json::json!({ "ref": ref_name, "oid": oid, "seq": seq }),
        None,
    )
    .into_response(&headers))
}

// ---- D1 collab aggregation API (read path; D1 §8 dashboard + thread views) -----

/// The collab state a read request needs: every inbox entry, the principals
/// registry and the merge-rule document (`refs/collab/meta/rules`, D1 §6).
/// Entries that do not parse are skipped — one corrupt inbox entry must not
/// take the dashboard down; the deterministic aggregation over the rest is
/// identical to what the `walgit collab` CLI computes locally.
struct CollabState {
    entries: Vec<EntryRef>,
    principals: HashMap<String, String>,
    rules: MergeRules,
}

async fn collab_load(st: &AppState, r: &Repo) -> Result<CollabState, ApiError> {
    // Refs-level work: the byte-sorted index, no LIST on the bucket.
    let mut principals: HashMap<String, String> = HashMap::new();
    let mut plan: Vec<(&str, String, String)> = Vec::new(); // (kind, rest, oid)
    let mut rules = MergeRules::default();
    let mut rules_oid: Option<String> = None;
    let mut snapshot_oid: Option<String> = None;
    for (name, oid) in &r.index.all {
        if let Some(rest) = name.strip_prefix("refs/collab/meta/principals/") {
            plan.push(("principal", rest.to_string(), oid.clone()));
        } else if let Some(rest) = name.strip_prefix("refs/collab/inbox/") {
            plan.push(("entry", rest.to_string(), oid.clone()));
        } else if name == "refs/collab/meta/rules" {
            rules_oid = Some(oid.clone());
        } else if name == walgit_wal::collab::SNAPSHOT_REF {
            snapshot_oid = Some(oid.clone());
        }
    }
    if plan.len() > COLLAB_MAX_ENTRIES {
        return Err(ApiError::ServiceUnavailable(format!(
            "collab namespace has more than {COLLAB_MAX_ENTRIES} unfolded refs; fold the inbox with `walgit collab gc` (D1 §11.4) or aggregate offline with the `walgit collab` CLI (this budget guards the remote reader and the per-request object fan-out)"
        )));
    }
    // D45 size precheck: refuse an oversized snapshot before materializing its
    // body, on both remote and local/mounted object paths.
    if let Some(oid) = snapshot_oid.as_ref()
        && object_size(r, oid).await?.is_some_and(|size| {
            size > u64::try_from(COLLAB_SNAPSHOT_MAX_BYTES).unwrap_or(u64::MAX)
        })
    {
        return Err(ApiError::ServiceUnavailable(format!(
            "collab snapshot exceeds {} MiB; aggregate offline with the `walgit collab` CLI",
            COLLAB_SNAPSHOT_MAX_BYTES / (1024 * 1024)
        )));
    }
    if let Some(remote) = r.remote() {
        let mut oids: Vec<gix_hash::ObjectId> = plan
            .iter()
            .filter_map(|(_, _, oid)| gix_hash::ObjectId::from_hex(oid.as_bytes()).ok())
            .collect();
        for oid in rules_oid.iter().chain(snapshot_oid.iter()) {
            if let Ok(oid) = gix_hash::ObjectId::from_hex(oid.as_bytes()) {
                oids.push(oid);
            }
        }
        remote.fault_many(&oids).await?;
        // The batched cat-file below must see the faulted loose objects.
        r.local
            .refresh_async()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    let mut want: Vec<String> = plan.iter().map(|(_, _, oid)| oid.clone()).collect();
    if let Some(oid) = &rules_oid {
        want.push(oid.clone());
    }
    if let Some(oid) = &snapshot_oid {
        want.push(oid.clone());
    }
    let blobs = git_cat_file_batch(&r.local, &want).await?;
    // D45 read semantics: the folded history (snapshot) ∪ the unfolded tail,
    // deduped by oid — a mid-fold state (snapshot moved, deletes pending)
    // aggregates identically to either side of it.
    let mut set = walgit_wal::collab::EntrySet::new();
    if let Some(oid) = &snapshot_oid {
        // Fail closed when the advertised snapshot cannot be read: skipping it
        // would silently degrade the aggregation to the inbox tail and drop
        // every folded entry from history (D45).
        let Some(bytes) = blobs.get(oid) else {
            return Err(internal(format!(
                "{} ({oid}) is advertised by the ref index but its blob is missing locally; \
                 repair the checkout before aggregating",
                walgit_wal::collab::SNAPSHOT_REF
            )));
        };
        if bytes.len() > COLLAB_SNAPSHOT_MAX_BYTES {
            return Err(ApiError::ServiceUnavailable(format!(
                "collab snapshot exceeds {} MiB; aggregate offline with the `walgit collab` CLI",
                COLLAB_SNAPSHOT_MAX_BYTES / (1024 * 1024)
            )));
        }
        // Fail closed on a corrupt document: silently skipping it would drop
        // every folded entry from history. (Per-record corruption is skipped
        // inside, like a corrupt inbox blob.)
        let snap = walgit_wal::collab::parse_snapshot(bytes)
            .map_err(|e| internal(format!("{}: {e}", walgit_wal::collab::SNAPSHOT_REF)))?;
        for rec in &snap.entries {
            if let Some(er) = rec.entry_ref(r.local.object_format()) {
                set.insert(er);
            }
        }
    }
    let mut entries_deferred: Vec<EntryRef> = Vec::new();
    for (kind, rest, oid) in plan {
        let Some(bytes) = blobs.get(&oid) else {
            continue; // pruned between index and read: skip like an unparsable entry
        };
        match kind {
            "principal" => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes)
                    && let Some(k) = v.get("public_key").and_then(|k| k.as_str())
                {
                    principals.insert(rest, k.to_string());
                }
            }
            _ => {
                if let Ok(entry) = serde_json::from_slice::<Entry>(bytes) {
                    let principal = rest
                        .rsplit_once('/')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_default();
                    entries_deferred.push(EntryRef {
                        oid,
                        principal,
                        entry,
                    });
                }
            }
        }
    }
    for er in entries_deferred {
        set.insert(er);
    }
    let entries = set.into_entries();
    // Cross-repo identity (issue #76): a principal absent from this repo's local
    // registry may still be registered at host level. Best-effort: a host store
    // failure degrades to repo-local verification (old behavior).
    if let Ok(host) = crate::web::v1::host_principals_map(st).await {
        for (principal, public_key) in host {
            principals.entry(principal).or_insert(public_key);
        }
    }

    if let Some(oid) = rules_oid
        && let Some(bytes) = blobs.get(&oid)
    {
        rules = serde_json::from_slice(bytes)
            .map_err(|e| internal(format!("refs/collab/meta/rules: {e}")))?;
    }
    Ok(CollabState {
        entries,
        principals,
        rules,
    })
}

/// The full observability report (D1 §8): thread summaries, PR status + merge
/// rule evaluation, verification health and per-actor/per-kind activity.
async fn collab_report(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let state = collab_load(&st2, &r).await?;
            let refs: Vec<&EntryRef> = state.entries.iter().collect();
            let report = build_report(
                &refs,
                &state.principals,
                &state.rules,
                chrono::Utc::now().timestamp(),
            );
            Ok(Rendered::json(json_bytes(&report), SWR, None))
        },
    )
    .await
}

/// One thread's ordered entries with per-entry verification, plus the
/// aggregated PR view + merge rule evaluation when the thread has a `patch`.
async fn collab_thread(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, id)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let state = collab_load(&st2, &r).await?;
            let filtered: Vec<&EntryRef> =
                state.entries.iter().filter(|e| e.entry.id == id).collect();
            if filtered.is_empty() {
                return Err(not_found(format!("no collab thread {id}")));
            }
            let ordered = thread(&filtered);
            // Issue #75 ③: related/depends_on oids are validated against the
            // whole collab state — a reference the aggregation cannot resolve
            // is reported per entry as `broken_refs` (machine-readable).
            let known: std::collections::HashSet<&str> =
                state.entries.iter().map(|e| e.oid.as_str()).collect();
            let entries: Vec<serde_json::Value> = ordered
                .iter()
                .map(|e| {
                    let referenced = walgit_wal::collab::referenced_oids(&e.entry.body);
                    let broken: Vec<&String> = referenced
                        .iter()
                        .filter(|o| !known.contains(o.as_str()))
                        .collect();
                    serde_json::json!({
                        "oid": e.oid,
                        "principal": e.principal,
                        "verified": e.is_verified(&state.principals),
                        "entry": e.entry,
                        "broken_refs": broken,
                    })
                })
                .collect();
            let pr = filtered.iter().any(|e| e.entry.kind == "patch").then(|| {
                let pr = pr_view(&filtered, &state.principals);
                let merge = merge_rule_eval(&state.rules, &pr);
                serde_json::json!({ "pr": pr, "merge": merge })
            });
            Ok(Rendered::json(
                json_bytes(&serde_json::json!({ "id": id, "entries": entries, "pr": pr })),
                SWR,
                None,
            ))
        },
    )
    .await
}

/// §8.2 storage convention (issue #161): a CI log/artifact's bytes are a
/// plain git blob at `refs/collab/ci-artifacts/<actor>/<sha256>`, pushed
/// through receive-pack like any collab object. This is the HTTP read side
/// for SDK and browser: refs-level scan of the namespace, the blob faulted
/// like any collab object, and — the whole point of the content address —
/// sha256-verified before one byte goes out. Immutable + strong `ETag`: the
/// bytes behind an address never change (D10).
async fn collab_ci_artifact(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, sha256)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    validate_ci_artifact_sha(&sha256)?;
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let candidates = ci_artifact_refs(&r, &sha256, None);
            let mut saw_oversize = false;
            for (_, oid) in candidates {
                let Some(size) = object_size(&r, oid).await? else {
                    continue;
                };
                if size > walgit_wal::ci::CI_ARTIFACT_MAX_BYTES {
                    saw_oversize = true;
                    continue;
                }
                if let Some(remote) = r.remote() {
                    let gix = gix_hash::ObjectId::from_hex(oid.as_bytes())
                        .map_err(|_| not_found("ci artifact"))?;
                    remote.fault_many(std::slice::from_ref(&gix)).await?;
                    // The batched cat-file below must see the faulted object.
                    r.local
                        .refresh_async()
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                }
                let oid_owned = oid.to_string();
                let blobs =
                    git_cat_file_batch(&r.local, std::slice::from_ref(&oid_owned)).await?;
                let Some(bytes) = blobs.get(oid) else {
                    continue;
                };
                let digest = <sha2::Sha256 as sha2::Digest>::digest(bytes);
                if hex::encode(digest) == sha256 {
                    return Ok(Rendered {
                        body: bytes::Bytes::from(bytes.clone()),
                        content_type: "application/octet-stream",
                        cache_control: IMMUTABLE,
                        etag: Some(etag_for(&sha256)),
                        extra: Vec::new(),
                        range: None,
                    });
                }
                // A ref whose payload does not hash to its address is hostile
                // or corrupt; try the next candidate for the same address.
            }
            if saw_oversize {
                Err(ApiError::PayloadTooLarge)
            } else {
                Err(not_found("ci artifact"))
            }
        },
    )
    .await
}

fn validate_ci_artifact_sha(sha256: &str) -> Result<(), ApiError> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ApiError::BadRequest(
            "ci-artifacts key must be a 64-char lowercase hex sha256".into(),
        ));
    }
    Ok(())
}

/// The refs an artifact address may resolve to. The browser/SDK lookup has no
/// actor and scans the content-addressed namespace for the first valid bytes
/// (B3); the CLI preflight names the exact inbox owner it is about to fetch.
fn ci_artifact_refs<'a>(
    r: &'a Repo,
    sha256: &str,
    actor: Option<&str>,
) -> Vec<(&'a str, &'a str)> {
    let prefix = walgit_wal::ci::CI_ARTIFACT_REF_PREFIX;
    if let Some(actor) = actor {
        let exact = format!("{prefix}{actor}/{sha256}");
        return r
            .index
            .all
            .binary_search_by(|(name, _)| name.as_str().cmp(exact.as_str()))
            .ok()
            .and_then(|i| r.index.all.get(i))
            .map(|(name, oid)| vec![(name.as_str(), oid.as_str())])
            .unwrap_or_default();
    }
    let suffix = format!("/{sha256}");
    let start = r
        .index
        .all
        .partition_point(|(name, _)| name.as_str() < prefix);
    r.index
        .all
        .get(start..)
        .unwrap_or_default()
        .iter()
        .take_while(|(name, _)| name.starts_with(prefix))
        .filter(|(name, _)| name.ends_with(&suffix))
        .map(|(name, oid)| (name.as_str(), oid.as_str()))
        .collect()
}

#[derive(serde::Deserialize)]
struct CiArtifactQuery {
    actor: Option<String>,
}

/// HEAD preflight for the CLI: it answers only whether the exact actor's
/// artifact exists and fits the 16 MiB object cap, before `git fetch` can
/// materialize an oversized blob. The body route remains the byte endpoint.
async fn collab_ci_artifact_size(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, sha256)): Path<(String, String, String)>,
    Query(q): Query<CiArtifactQuery>,
) -> Result<Response, ApiError> {
    validate_ci_artifact_sha(&sha256)?;
    if let Some(actor) = &q.actor
        && !ref_segment_ok(actor)
    {
        return Err(ApiError::BadRequest("invalid ci artifact actor".into()));
    }
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let mut saw_oversize = false;
            for (_, oid) in ci_artifact_refs(&r, &sha256, q.actor.as_deref()) {
                let Some(size) = object_size(&r, oid).await? else {
                    continue;
                };
                if size > walgit_wal::ci::CI_ARTIFACT_MAX_BYTES {
                    saw_oversize = true;
                    continue;
                }
                return Ok(Rendered {
                    body: bytes::Bytes::new(),
                    content_type: "application/octet-stream",
                    cache_control: IMMUTABLE,
                    etag: Some(etag_for(&sha256)),
                    extra: Vec::new(),
                    range: None,
                });
            }
            if saw_oversize {
                Err(ApiError::PayloadTooLarge)
            } else {
                Err(not_found("ci artifact"))
            }
        },
    )
    .await
}

/// The work-unit board (D1 §8): `build_board` — the same projection the
/// `walgit collab` CLI computes offline — over the collab state, under the
/// board definition versioned at `.walgit/board.toml` (HEAD).
///
/// The render cache (`cache/api/v1/*.json`, D1 §11 open question 3) is
/// deliberately **not** used: it exists for sha-addressed immutable answers,
/// while the projection's input is the live collab refs, which move with every
/// entry push — keying it would need a second cache with its own invalidation
/// story, i.e. a staleness surface, for a computation that is refs-level plus
/// one bounded blob fan-out (principle IV: every read revalidates). SWR like
/// `collab/report`, never immutable.
async fn collab_board(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        None,
        move |r| async move {
            let state = collab_load(&st2, &r).await?;
            let board_def = load_board_def(&r).await?;
            let refs: Vec<&EntryRef> = state.entries.iter().collect();
            let board: Board = build_board(&refs, &state.principals, &state.rules, &board_def);
            Ok(Rendered::json(json_bytes(&board), SWR, None))
        },
    )
    .await
}

/// The board definition: `.walgit/board.toml` at HEAD — versioned with the
/// repository, so every client (this endpoint, the CLI, a fresh clone) reads
/// the same committed definition. Absent file ⇒ `default_board()` (the
/// definition is optional); **present but invalid fails the request** with the
/// parse error — a broken definition must not silently fold cards into the
/// wrong lane.
async fn load_board_def(r: &Repo) -> Result<BoardDef, ApiError> {
    let head = r.index.head().map(|(_, sha)| sha);
    let bytes = async {
        let sha = head.ok_or_else(|| ApiError::NotFound("unborn HEAD".into()))?;
        if let Some(remote) = r.remote() {
            let oid = gix_hash::ObjectId::from_hex(sha.as_bytes())
                .map_err(|_| not_found("HEAD revision"))?;
            let (_, blob_oid, mode) =
                remote
                    .fault_path(&oid, BOARD_PATH)
                    .await
                    .map_err(|e| match e {
                        ApiError::NotFound(_) => {
                            ApiError::NotFound(format!("{BOARD_PATH} at HEAD"))
                        }
                        other => other,
                    })?;
            if !mode.is_blob() {
                return Err(ApiError::BadRequest(format!("{BOARD_PATH} is not a blob")));
            }
            Ok(remote.get(&blob_oid).await?.data.to_vec())
        } else {
            git(
                &r.local,
                vec![
                    "cat-file".into(),
                    "blob".into(),
                    format!("{sha}:{BOARD_PATH}"),
                ],
            )
            .await
            .map_err(|_| ApiError::NotFound(format!("{BOARD_PATH} at HEAD")))
        }
    }
    .await;
    match bytes {
        Ok(b) => parse_board_def(&String::from_utf8_lossy(&b))
            .map_err(|e| ApiError::BadRequest(format!("{BOARD_PATH}: {e}"))),
        // No definition (or no HEAD yet): the board is optional. A present but
        // broken definition fails closed above; store failures keep their own
        // status (only NotFound means "absent").
        Err(ApiError::NotFound(_)) => Ok(default_board()),
        Err(e) => Err(e),
    }
}

// ---- resolve -----------------------------------------------------------------

/// §3: longest branch/tag prefix of `rest` wins (branch beats tag on ties);
/// else the first segment must be a commit-ish; empty -> default branch.
async fn resolve_rest(r: &Repo, rest: &str) -> Result<Resolved, ApiError> {
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        let (name, sha) = r.index.head().ok_or_else(|| not_found("unborn HEAD"))?;
        return Ok(Resolved {
            ref_name: name,
            sha,
            path: String::new(),
            kind: "branch",
        });
    }
    // Candidate prefixes, longest first.
    let mut cut_points: Vec<usize> = rest.match_indices('/').map(|(i, _)| i).collect();
    cut_points.push(rest.len());
    for &cut in cut_points.iter().rev() {
        // cut comes from match_indices('/') or rest.len(): '/' is ASCII, so
        // every cut sits on a char boundary and split_at cannot panic.
        let (name, path_rest) = rest.split_at(cut);
        let path = path_rest.trim_start_matches('/').to_string();
        if let Some(sha) = r.index.branch(name) {
            return Ok(Resolved {
                ref_name: name.to_string(),
                sha: sha.to_string(),
                path,
                kind: "branch",
            });
        }
        if let Some(sha) = r.index.tag(name) {
            return Ok(Resolved {
                ref_name: name.to_string(),
                sha: sha.to_string(),
                path,
                kind: "tag",
            });
        }
    }
    let (first, path) = match rest.split_once('/') {
        Some((f, p)) => (f, p.to_string()),
        None => (rest, String::new()),
    };
    let sha = rev_parse_commit(r, first).await?;
    Ok(Resolved {
        ref_name: first.to_string(),
        sha,
        path,
        kind: "commit",
    })
}

/// Resolve a single revision name (no path): branch, tag, then git rev-parse.
async fn resolve_name(r: &Repo, name: &str) -> Result<Resolved, ApiError> {
    if (name.is_empty() || name == "HEAD")
        && let Some((n, sha)) = r.index.head()
    {
        return Ok(Resolved {
            ref_name: n,
            sha,
            path: String::new(),
            kind: "branch",
        });
    }
    if let Some(sha) = r.index.branch(name) {
        return Ok(Resolved {
            ref_name: name.into(),
            sha: sha.into(),
            path: String::new(),
            kind: "branch",
        });
    }
    if let Some(sha) = r.index.tag(name) {
        return Ok(Resolved {
            ref_name: name.into(),
            sha: sha.into(),
            path: String::new(),
            kind: "tag",
        });
    }
    let sha = rev_parse_commit(r, name).await?;
    Ok(Resolved {
        ref_name: name.into(),
        sha,
        path: String::new(),
        kind: "commit",
    })
}

/// `rev-list -1 <rev>`, which peels to a commit and prints nothing for a
/// non-commit: local git when objects are on disk, the pack indexes (unique
/// prefix, tag peel) when served remotely.
async fn rev_parse_commit(r: &Repo, rev: &str) -> Result<String, ApiError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(not_found("revision"));
    }
    if !r.objects {
        return Err(not_found(format!("unknown revision {rev}")));
    }
    if let Some(remote) = r.remote() {
        return Ok(remote.resolve_commitish(rev).await?.to_string());
    }
    let out = git(
        &r.local,
        vec![
            "rev-list".into(),
            "-1".into(),
            "--end-of-options".into(),
            rev.into(),
        ],
    )
    .await
    .map_err(|_| not_found(format!("unknown revision {rev}")))?;
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if sha.is_empty() {
        return Err(not_found(format!("unknown revision {rev}")));
    }
    Ok(sha)
}

async fn resolve_root(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    resolve_impl(&st, &headers, &owner, &repo_name, "").await
}
async fn resolve(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    resolve_impl(&st, &headers, &owner, &repo_name, &rest).await
}
async fn resolve_impl(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    repo_name: &str,
    rest: &str,
) -> Result<Response, ApiError> {
    // Branch/tag names resolve from the index; a raw revision falls back to
    // object access. Refs-only first so huge repos still answer for named refs.
    let handle = open(st, headers, owner, repo_name).await?;
    let mut r = view(st, handle, Need::Refs, Reporter::none()).await?;
    let res = match resolve_rest(&r, rest).await {
        Ok(res) => res,
        Err(ApiError::NotFound(_)) if !r.objects => {
            r.need_objects(st).await?;
            resolve_rest(&r, rest).await?
        }
        Err(e) => return Err(e),
    };
    let etag = etag_for(&res.sha);
    Ok(json_swr(&res, Some(&etag)).into_response(headers))
}

/// Split `{ref}/{path}` for tree/blob: a leading full sha is taken verbatim
/// (immutable response); otherwise §3 resolution (SWR + `ETag`).
fn split_addr(rest: &str) -> Option<(Resolved, bool)> {
    let rest = rest.trim_matches('/');
    let (first, path) = match rest.split_once('/') {
        Some((f, p)) => (f, p.trim_matches('/').to_string()),
        None => (rest, String::new()),
    };
    is_full_sha(first).then(|| {
        (
            Resolved {
                ref_name: first.to_string(),
                sha: first.to_string(),
                path,
                kind: "commit",
            },
            true,
        )
    })
}
async fn resolve_addr(r: &Repo, rest: &str) -> Result<(Resolved, bool), ApiError> {
    if let Some(x) = split_addr(rest) {
        return Ok(x);
    }
    Ok((resolve_rest(r, rest).await?, false))
}

// ---- git plumbing ------------------------------------------------------------

fn output_bytes(out: std::process::Output) -> Result<Vec<u8>, ApiError> {
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(not_found(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}
async fn git(local: &walgit_git::LocalRepo, args: Vec<String>) -> Result<Vec<u8>, ApiError> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = local.git(&refs).await.map_err(internal)?;
    output_bytes(out)
}

fn parse_commit_record(record: &str) -> Option<Commit> {
    let mut p = record.split('\0');
    let sha = p.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let parents = p.next()?.split_whitespace().map(str::to_string).collect();
    Some(
        Commit {
            sha,
            parents,
            author: p.next()?.to_string(),
            author_email: p.next()?.to_string(),
            author_date: p.next()?.to_string(),
            committer: p.next()?.to_string(),
            date: p.next()?.to_string(),
            subject: p.next()?.to_string(),
            body: String::new(),
            trailers: Vec::new(),
        }
        .with_body(p.next().unwrap_or("")),
    )
}
fn parse_commits(bytes: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(bytes)
        .split('\x1e')
        .filter_map(parse_commit_record)
        .collect()
}
fn log_format() -> String {
    "%x1e%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%cI%x00%s%x00%b%x00".to_string()
}

// ---- tree ----------------------------------------------------------------------

fn tree_key(repo: &str, sha: &str, path: &str) -> String {
    format!("{repo}\0tree\0{sha}\0{path}")
}

async fn tree(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let key = split_addr(&rest)
        .map(|(res, _)| tree_key(&format!("{owner}/{repo_name}"), &res.sha, &res.path));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = resolve_addr(&r, &rest).await?;
            let key = tree_key(&r.id, &res.sha, &res.path);
            if immutable && let Some(hit) = st2.caches.api_immutable.get(&key) {
                return Ok(Rendered::json(hit, IMMUTABLE, None));
            }
            let body = match r.remote() {
                Some(remote) => render_tree_remote(&remote, &res).await?,
                None => render_tree(&r.local, &res).await?,
            };
            Ok(finish(&st2, &r, immutable, &key, &res.sha, body))
        },
    )
    .await
}

async fn render_tree(
    local: &walgit_git::LocalRepo,
    res: &Resolved,
) -> Result<bytes::Bytes, ApiError> {
    // A bare commit-ish already denotes its root tree to `ls-tree`; `^{tree}`
    // is not usable here — a bash-style git (msys64's on Windows) expands the
    // braces out of the argument before git ever sees it.
    let spec = if res.path.is_empty() {
        res.sha.clone()
    } else {
        format!("{}:{}", res.sha, res.path)
    };
    let bytes = git(
        local,
        vec!["ls-tree".into(), "-l".into(), "-z".into(), spec],
    )
    .await?;
    let mut entries = Vec::new();
    for item in bytes.split(|b| *b == 0).filter(|x| !x.is_empty()) {
        let Some(tab) = item.iter().position(|b| *b == b'\t') else {
            continue;
        };
        // tab < item.len() (position() found the tab inside it), and the
        // second split skips exactly that one byte.
        let (meta, after_tab) = item.split_at(tab);
        let (_, name) = after_tab.split_at(1);
        // `ls-tree -l` right-aligns the size with padding spaces.
        let fields: Vec<&[u8]> = meta
            .split(|b| *b == b' ')
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() < 4 {
            continue;
        }
        // Guarded above: `mode kind sha size` are the first four fields.
        let [mode, kind, sha, size_field, ..] = fields.as_slice() else {
            continue; // len >= 4 checked above; unreachable
        };
        let kind = String::from_utf8_lossy(kind).to_string();
        let size = if kind == "blob" {
            String::from_utf8_lossy(size_field).parse().unwrap_or(-1)
        } else {
            -1
        };
        entries.push(TreeEntry {
            name: String::from_utf8_lossy(name).to_string(),
            kind,
            mode: String::from_utf8_lossy(mode).to_string(),
            size,
            sha: String::from_utf8_lossy(sha).to_string(),
        });
    }
    sort_entries(&mut entries);
    let commit = newest_commit(local, &res.sha, &res.path)
        .await
        .ok()
        .and_then(|b| parse_commits(&b).into_iter().next());
    let mut readme = None;
    if let Some(e) = readme_entry(&entries)
        && let Ok(content) = git(local, vec!["cat-file".into(), "blob".into(), e.sha.clone()]).await
        && let Ok(s) = String::from_utf8(content)
    {
        readme = Some(Readme {
            name: e.name.clone(),
            contents: s,
        });
    }
    Ok(json_bytes(&Tree {
        ref_name: res.ref_name.clone(),
        sha: res.sha.clone(),
        path: res.path.clone(),
        entries,
        commit,
        readme,
    }))
}

fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        let ad = a.kind == "tree";
        let bd = b.kind == "tree";
        bd.cmp(&ad)
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
}
fn readme_entry(entries: &[TreeEntry]) -> Option<&TreeEntry> {
    entries.iter().find(|e| {
        e.kind == "blob"
            && [
                "readme",
                "readme.md",
                "readme.markdown",
                "readme.txt",
                "readme.rst",
            ]
            .contains(&e.name.to_ascii_lowercase().as_str())
    })
}

/// Tree listing straight from the remote pack set: entries from the parsed
/// tree, blob sizes from pack entry headers (concurrent), the newest commit
/// via a bounded walk, README read directly.
async fn render_tree_remote(remote: &Remote, res: &Resolved) -> Result<bytes::Bytes, ApiError> {
    let sha =
        gix_hash::ObjectId::from_hex(res.sha.as_bytes()).map_err(|_| not_found("revision"))?;
    remote.reporter.notice(format!(
        "Reading {} from the WAL pack set",
        if res.path.is_empty() {
            "the root tree".to_string()
        } else {
            format!("tree {}", res.path)
        }
    ));
    let (_c, target, mode) = remote.fault_path(&sha, &res.path).await?;
    if !mode.is_tree() {
        return Err(not_found(format!("'{}' is not a tree", res.path)));
    }
    let raw = remote.tree_entries(&target).await?;
    let total = raw.len();
    let entries: Vec<TreeEntry> = futures::stream::iter(raw)
        .map(|e| async move {
            let kind = match e.mode.kind() {
                gix_object::tree::EntryKind::Tree => "tree",
                gix_object::tree::EntryKind::Commit => "commit",
                _ => "blob",
            };
            let size = if kind == "blob" {
                remote
                    .kind_and_size(&e.oid)
                    .await
                    .ok()
                    .flatten()
                    .map_or(-1, |(_, s)| i64::try_from(s).unwrap_or(i64::MAX))
            } else {
                -1
            };
            TreeEntry {
                name: String::from_utf8_lossy(&e.name).to_string(),
                kind: kind.to_string(),
                mode: format!("{:06o}", e.mode.kind() as u16),
                size,
                sha: e.oid.to_string(),
            }
        })
        .buffer_unordered(32)
        .collect()
        .await;
    if total > 64 {
        remote.reporter.notice(format!("Sized {total} entries"));
    }
    let mut entries = entries;
    sort_entries(&mut entries);
    let commit = remote
        .newest_touching(sha, &res.path)
        .await?
        .map(Commit::from);
    let mut readme = None;
    if let Some(e) = readme_entry(&entries)
        && let Ok(oid) = gix_hash::ObjectId::from_hex(e.sha.as_bytes())
        && let Ok(o) = remote.get(&oid).await
        && let Ok(s) = String::from_utf8(o.data.to_vec())
    {
        readme = Some(Readme {
            name: e.name.clone(),
            contents: s,
        });
    }
    Ok(json_bytes(&Tree {
        ref_name: res.ref_name.clone(),
        sha: res.sha.clone(),
        path: res.path.clone(),
        entries,
        commit,
        readme,
    }))
}

async fn newest_commit(
    local: &walgit_git::LocalRepo,
    sha: &str,
    path: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut a = vec![
        "log".into(),
        "-1".into(),
        format!("--format={}", log_format()),
        sha.into(),
    ];
    if !path.is_empty() {
        a.push("--".into());
        a.push(path.into());
    }
    git(local, a).await
}

// ---- blob ----------------------------------------------------------------------

fn blob_key(repo: &str, sha: &str, path: &str) -> String {
    format!("{repo}\0blob\0{sha}\0{path}")
}

async fn blob(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
    Query(q): Query<BlobQuery>,
) -> Result<Response, ApiError> {
    let raw = q.raw.is_some();
    // `?raw` is a page navigation (the "Raw" link): never the SSE envelope.
    let mut plain_headers = headers.clone();
    if raw {
        plain_headers.remove(header::ACCEPT);
    }
    let key = if raw {
        None
    } else {
        split_addr(&rest)
            .map(|(res, _)| blob_key(&format!("{owner}/{repo_name}"), &res.sha, &res.path))
    };
    let st2 = st.clone();
    let req_headers = headers.clone();
    run(
        &st,
        &plain_headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = resolve_addr(&r, &rest).await?;
            if res.path.is_empty() {
                return Err(not_found("blob path"));
            }
            let name = res.path.rsplit('/').next().unwrap_or(&res.path).to_string();
            let (size, bytes): (i64, Option<Vec<u8>>) = if let Some(remote) = r.remote() {
                let sha = gix_hash::ObjectId::from_hex(res.sha.as_bytes())
                    .map_err(|_| not_found("revision"))?;
                remote
                    .reporter
                    .notice(format!("Reading {} from the WAL pack set", res.path));
                let (_c, target, mode) = remote.fault_path(&sha, &res.path).await?;
                if !mode.is_blob_or_symlink() {
                    return Err(not_found(format!("'{}' is not a file", res.path)));
                }
                let (_, size) = remote
                    .kind_and_size(&target)
                    .await?
                    .ok_or_else(|| not_found("blob"))?;
                // Real blob sizes are far below 2^63; the old `as i64` wrapped
                // only for sizes no repository can reach, so saturate instead.
                let size = i64::try_from(size).unwrap_or(i64::MAX);
                if size > MAX_BLOB {
                    (size, None)
                } else {
                    let o = remote.get(&target).await?;
                    (size, Some(o.data.to_vec()))
                }
            } else {
                let bytes = git(
                    &r.local,
                    vec![
                        "cat-file".into(),
                        "blob".into(),
                        format!("{}:{}", res.sha, res.path),
                    ],
                )
                .await?;
                (i64::try_from(bytes.len()).unwrap_or(i64::MAX), Some(bytes))
            };
            // `?raw` is the **byte** channel (images, media, PDF …): any type, its
            // real content type, and a single range so media can seek. It used to
            // answer only for text and hand every other file back as JSON
            // `{binary:true}` — i.e. the bytes were unreachable, so nothing but
            // plain text could ever be rendered.
            if raw {
                if size > RAW_BLOB_MAX {
                    return Err(ApiError::PayloadTooLarge);
                }
                let bytes = bytes.ok_or_else(|| not_found("blob"))?;
                return Ok(raw_blob(bytes::Bytes::from(bytes), &res.path, &res.sha, immutable, &req_headers));
            }
            let is_text = size <= MAX_BLOB
                && !bytes.as_ref().is_some_and(|b| b.contains(&0))
                && bytes
                    .as_ref()
                    .is_some_and(|b| std::str::from_utf8(b).is_ok());
            let b = if size > MAX_BLOB {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: None,
                    binary: None,
                    too_large: Some(true),
                }
            } else if !is_text {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: None,
                    binary: Some(true),
                    too_large: None,
                }
            } else {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: Some(
                        String::from_utf8(bytes.unwrap_or_default()).unwrap_or_default(),
                    ),
                    binary: None,
                    too_large: None,
                }
            };
            let key = blob_key(&r.id, &res.sha, &res.path);
            Ok(finish(&st2, &r, immutable, &key, &res.sha, json_bytes(&b)))
        },
    )
    .await
}

/// `?raw` for any file: the real content type, a single RFC 9110 range (media
/// seeking and PDF partial loads), and the two headers that keep untrusted
/// repository content inert. Extracted so the policy is one readable place.
fn raw_blob(
    bytes: bytes::Bytes,
    path: &str,
    rev: &str,
    immutable: bool,
    req: &HeaderMap,
) -> Rendered {
    let content_type = content_type_for(path);
    let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let range = req
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|spec| parse_range(spec, total));
    let body = match range {
        // Bounds come from parse_range(), which clamps to `total - 1`.
        Some((start, end)) => bytes.slice(
            usize::try_from(start).unwrap_or(0)..=usize::try_from(end).unwrap_or(0),
        ),
        None => bytes,
    };
    let mut extra = vec![(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    )];
    if is_active_content(content_type) {
        extra.push((
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(ACTIVE_CONTENT_CSP),
        ));
    }
    Rendered {
        body,
        content_type,
        cache_control: if immutable { IMMUTABLE } else { SWR },
        // Keyed by *file*, not just revision: every path at one commit used to
        // share a validator.
        etag: (!immutable).then(|| etag_for(&format!("{rev}:{path}"))),
        extra,
        range: range.map(|(start, end)| (start, end, total)),
    }
}

/// Content type for the raw byte channel, by extension. Deliberately
/// conservative: anything unknown is `application/octet-stream`, and the two
/// formats that are actually *programs* (HTML, SVG, XML) are marked so the
/// caller can sandbox them.
fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("svg") => "image/svg+xml",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("ogv") => "video/ogg",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("oga" | "ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("flac") => "audio/flac",
        Some("pdf") => "application/pdf",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("xhtml") => "application/xhtml+xml; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        // Named text formats stay text (the "Raw" link should display them).
        Some(
            "md" | "markdown" | "txt" | "log" | "csv" | "tsv" | "json" | "yaml" | "yml" | "toml"
            | "ini" | "cfg" | "rs" | "py" | "js" | "ts" | "sh" | "go" | "c" | "h" | "cpp" | "sql",
        ) => "text/plain; charset=utf-8",
        // Everything else is bytes: unknown is *not* a licence to hand a binary
        // to the browser as text (the blob page reads text through the JSON
        // lane, so `?raw` only has to be right for downloads and media).
        _ => "application/octet-stream",
    }
}

/// Formats that can run code when a browser treats them as a document.
fn is_active_content(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or_default().trim();
    matches!(
        base,
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "application/xml"
    )
}

/// `bytes=start-end`, `bytes=start-`, `bytes=-suffix` → `(start, end)` with
/// `end` inclusive. `None` for anything else (multi-range, malformed, or
/// unsatisfiable) — the caller then serves the whole body, which RFC 9110
/// allows and is what a plain `<img>`/`<video>` expects.
fn parse_range(spec: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let rest = spec.trim().strip_prefix("bytes=")?;
    if rest.contains(',') {
        return None;
    }
    let (first, last) = rest.split_once('-')?;
    let last = last.trim();
    if first.trim().is_empty() {
        let suffix: u64 = last.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        return Some((len.saturating_sub(suffix), len - 1));
    }
    let start: u64 = first.trim().parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if last.is_empty() {
        len - 1
    } else {
        last.parse::<u64>().ok()?.min(len - 1)
    };
    (end >= start).then_some((start, end))
}

#[cfg(test)]
mod blob_view_tests {
    use super::{content_type_for, is_active_content, parse_range};

    #[test]
    fn content_types_cover_the_viewer_formats() {
        assert_eq!(content_type_for("a/b/pic.PNG"), "image/png");
        assert_eq!(content_type_for("clip.mp4"), "video/mp4");
        assert_eq!(content_type_for("song.flac"), "audio/flac");
        assert_eq!(content_type_for("doc.pdf"), "application/pdf");
        assert_eq!(content_type_for("page.html"), "text/html; charset=utf-8");
        assert_eq!(content_type_for("notes.md"), "text/plain; charset=utf-8");
        // Unknown → bytes, never text: a `.dat` is not a document.
        assert_eq!(content_type_for("blob.bin"), "application/octet-stream");
        assert_eq!(content_type_for("bin.dat"), "application/octet-stream");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }

    #[test]
    fn only_code_bearing_types_are_active() {
        assert!(is_active_content("text/html; charset=utf-8"));
        assert!(is_active_content("image/svg+xml"));
        assert!(!is_active_content("image/png"));
        assert!(!is_active_content("application/pdf"));
        assert!(!is_active_content("text/plain; charset=utf-8"));
    }

    #[test]
    fn ranges_are_inclusive_and_clamped() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=0-999", 100), Some((0, 99)));
        // Multi-range and unsatisfiable requests fall back to the whole body.
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("items=0-9", 100), None);
        assert_eq!(parse_range("bytes=0-9", 0), None);
    }
}

// ---- commits -------------------------------------------------------------------

fn commits_key(repo: &str, sha: &str, path: &str, skip: usize, n: usize) -> String {
    format!("{repo}\0commits\0{sha}\0{path}\0{skip}\0{n}")
}

async fn commits(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    Query(q): Query<CommitQuery>,
) -> Result<Response, ApiError> {
    let reference = q.ref_.clone().unwrap_or_else(|| "HEAD".into());
    let skip = q.skip.unwrap_or(0);
    let n = q.n.unwrap_or(35).clamp(1, 200);
    let path = q.path.clone().unwrap_or_default();
    let key = is_full_sha(&reference)
        .then(|| commits_key(&format!("{owner}/{repo_name}"), &reference, &path, skip, n));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = if is_full_sha(&reference) {
                (
                    Resolved {
                        ref_name: reference.clone(),
                        sha: reference.clone(),
                        path: String::new(),
                        kind: "commit",
                    },
                    true,
                )
            } else {
                (resolve_name(&r, &reference).await?, false)
            };
            let key = commits_key(&r.id, &res.sha, &path, skip, n);
            if immutable && let Some(hit) = st2.caches.api_immutable.get(&key) {
                return Ok(Rendered::json(hit, IMMUTABLE, None));
            }
            let mut cs: Vec<Commit> = if let Some(remote) = r.remote() {
                let start = gix_hash::ObjectId::from_hex(res.sha.as_bytes())
                    .map_err(|_| not_found("revision"))?;
                let label = if path.is_empty() {
                    "Walking history".to_string()
                } else {
                    format!("Walking history of {path}")
                };
                remote.reporter.notice(format!(
                    "{label} from {} (reading commits from the WAL pack set)",
                    // sha passed from_hex above (40/64 hex chars): byte 12 is a
                    // char boundary.
                    res.sha.split_at(12).0
                ));
                let all = remote
                    .walk(
                        start,
                        (!path.is_empty()).then_some(path.as_str()),
                        skip + n + 1,
                        &label,
                    )
                    .await?;
                all.into_iter().skip(skip).map(Commit::from).collect()
            } else {
                let mut a = vec![
                    "log".into(),
                    format!("--format={}", log_format()),
                    "--no-color".into(),
                    format!("--skip={skip}"),
                    format!("-{count}", count = n.saturating_add(1)),
                    res.sha.clone(),
                ];
                if !path.is_empty() {
                    a.extend(["--".into(), path.clone()]);
                }
                parse_commits(&git(&r.local, a).await?)
            };
            let more = cs.len() > n;
            if more {
                cs.truncate(n);
            }
            let body = json_bytes(&Commits {
                ref_name: res.ref_name.clone(),
                sha: res.sha.clone(),
                items: cs,
                more,
            });
            Ok(finish(&st2, &r, immutable, &key, &res.sha, body))
        },
    )
    .await
}

// ---- commit detail -------------------------------------------------------------

fn commit_key(repo: &str, sha: &str) -> String {
    format!("{repo}\0commit\0{sha}")
}

async fn commit_detail(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rev)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let key = is_full_sha(&rev).then(|| commit_key(&format!("{owner}/{repo_name}"), &rev));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let immutable = is_full_sha(&rev);
            let sha = if immutable {
                rev.clone()
            } else {
                resolve_name(&r, &rev).await?.sha
            };
            let key = commit_key(&r.id, &sha);
            if immutable && let Some(hit) = st2.caches.api_immutable.get(&key) {
                return Ok(Rendered::json(hit, IMMUTABLE, None));
            }
            if let Some(remote) = r.remote() {
                // Fault the commit, its first parent and every object the diff
                // touches into the loose store; `git show` below then runs as-is.
                let oid = gix_hash::ObjectId::from_hex(sha.as_bytes())
                    .map_err(|_| not_found("commit"))?;
                remote.reporter.notice(format!(
                    "Reading commit {} from the WAL pack set",
                    // sha passed from_hex above (40/64 hex chars): byte 12 is a
                    // char boundary.
                    sha.split_at(12).0
                ));
                remote.fault_commit_diff(&oid).await?;
            }
            let commit = parse_commits(
                // `--diff-merges=off`: plain `show -s` on a merge still sets up
                // the combined diff and reads the other parents' subtrees (on a
                // remotely served repo those are exactly the objects we did not
                // fault).
                &git(
                    &r.local,
                    vec![
                        "show".into(),
                        "-s".into(),
                        "--diff-merges=off".into(),
                        format!("--format={}", log_format()),
                        sha.clone(),
                    ],
                )
                .await?,
            )
            .into_iter()
            .next()
            .ok_or_else(|| not_found("commit"))?;
            let stat_out = git(
                &r.local,
                vec![
                    "show".into(),
                    "--format=".into(),
                    "--numstat".into(),
                    "-M".into(),
                    "--diff-merges=first-parent".into(),
                    "--root".into(),
                    sha.clone(),
                ],
            )
            .await?;
            let stats = parse_stats(&stat_out);
            let patch = String::from_utf8_lossy(
                &git(
                    &r.local,
                    vec![
                        "show".into(),
                        "--format=".into(),
                        "-p".into(),
                        "-M".into(),
                        "--no-color".into(),
                        "--no-ext-diff".into(),
                        "--diff-merges=first-parent".into(),
                        "--root".into(),
                        sha.clone(),
                    ],
                )
                .await?,
            )
            .into_owned();
            let body = json_bytes(&CommitDetail {
                commit,
                stats,
                patch,
            });
            Ok(finish(&st2, &r, immutable, &key, &sha, body))
        },
    )
    .await
}
fn parse_stats(bytes: &[u8]) -> Vec<Stat> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            // `git --numstat -M` rows are `<add>\t<del>\t<path>`.
            let mut f = line.split('\t');
            let (Some(add), Some(del), Some(path)) = (f.next(), f.next(), f.next()) else {
                return None;
            };
            if !add.chars().all(|c| c.is_ascii_digit()) && add != "-" {
                return None;
            }
            Some(Stat {
                path: normalize_rename(path),
                additions: if add == "-" {
                    -1
                } else {
                    add.parse().unwrap_or(-1)
                },
                deletions: if del == "-" {
                    -1
                } else {
                    del.parse().unwrap_or(-1)
                },
            })
        })
        .collect()
}
/// `git --numstat -M` prints renames as `old => new` or `prefix/{old => new}/suffix`;
/// return the new path.
fn normalize_rename(s: &str) -> String {
    if let (Some(open), Some(close)) = (s.find('{'), s.rfind('}'))
        && open < close
    {
        // '{' and '}' are ASCII, so open/close sit on char boundaries and
        // open < close ≤ len - 1 bounds every split below.
        let (before_brace, from_brace) = s.split_at(open);
        let (_, after_open) = from_brace.split_at(1); // skip '{'
        let (inner, after_close) = after_open.split_at(close - open - 1);
        let (_, suffix) = after_close.split_at(1); // skip '}'
        if let Some((_, new)) = inner.split_once(" => ") {
            let mut out = String::with_capacity(s.len());
            out.push_str(before_brace);
            out.push_str(new);
            out.push_str(suffix);
            return out.replace("//", "/");
        }
    }
    if let Some((_, new)) = s.split_once(" => ") {
        return new.to_string();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rename_paths() {
        assert_eq!(
            super::normalize_rename("src/{main.rs => app.rs}"),
            "src/app.rs"
        );
        assert_eq!(super::normalize_rename("{a => b}/x.rs"), "b/x.rs");
        assert_eq!(super::normalize_rename("a/{ => sub}/x.rs"), "a/sub/x.rs");
        assert_eq!(super::normalize_rename("old.rs => new.rs"), "new.rs");
        assert_eq!(super::normalize_rename("plain.rs"), "plain.rs");
    }
}
