//! MCP resources: the `walgit://` URIs a client can list, template, read and
//! subscribe to (D64).
//!
//! Four kinds, one vocabulary: these are the URIs `web/SKILL.md` documents and
//! the ones the bundled UI prints on each repository's collab page.
//!
//! | URI | What it answers |
//! |---|---|
//! | `walgit://refs/{owner}/{repo}` | HEAD + a page of branches and tags, at the seq it was read |
//! | `walgit://wal/{owner}/{repo}?from=N` | the WAL entries from `N` (the "what moved" lane) |
//! | `walgit://collab/board/{owner}/{repo}` | the D1 board projection |
//! | `walgit://collab/thread/{owner}/{repo}/{thread}` | one thread: entries, status, reviews |
//!
//! **Subscriptions are polls, not held requests** (D52/D55): a subscription runs
//! one bounded task per URI that reads the repository's refs-level seq every
//! `mcp.subscribe_interval` and emits `notifications/resources/updated` when it
//! changes. The seq is a conditional `manifest.pb` GET (or a 304) — the same
//! observation lane `git ls-remote` and `walgit collab watch` use, at
//! at-least-once semantics: the notification means "re-read", never "here is the
//! delta", so a dropped or coalesced event costs nothing.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{MetaObject, Resource, ResourceContents, ResourceTemplate};
use rmcp::ErrorData;
use serde_json::{json, Value};

use axum::http::HeaderMap;

use super::api::{enc, query, ApiError, Call};
use crate::AppState;

/// Where every resource URI lives.
pub const SCHEME: &str = "walgit://";

/// One repository's resource set as `resources/list` reports it. Bounded so a
/// host with thousands of repositories answers with a page rather than a
/// megabyte of URI metadata.
const LIST_MAX_REPOS: usize = 100;

/// One `resources/read` page for the WAL resource.
const WAL_PAGE: u64 = 200;

const JSON_MIME: &str = "application/json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uri {
    Refs {
        owner: String,
        repo: String,
    },
    Wal {
        owner: String,
        repo: String,
        from: u64,
    },
    Board {
        owner: String,
        repo: String,
    },
    Thread {
        owner: String,
        repo: String,
        thread: String,
    },
}

impl Uri {
    pub fn owner(&self) -> &str {
        match self {
            Uri::Refs { owner, .. }
            | Uri::Wal { owner, .. }
            | Uri::Board { owner, .. }
            | Uri::Thread { owner, .. } => owner,
        }
    }
    pub fn repo(&self) -> &str {
        match self {
            Uri::Refs { repo, .. }
            | Uri::Wal { repo, .. }
            | Uri::Board { repo, .. }
            | Uri::Thread { repo, .. } => repo,
        }
    }
}

/// Parse `walgit://kind/{owner}/{repo}[/{arg}][?from=N]`.
pub fn parse(uri: &str) -> Result<Uri, ErrorData> {
    let bad = || ErrorData::invalid_params(format!("not a walgit resource URI: {uri}"), None);
    let rest = uri.strip_prefix(SCHEME).ok_or_else(bad)?;
    let (path, query_str) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let seg = |i: usize| -> Result<String, ErrorData> {
        parts
            .get(i)
            .map(|s| percent_decode(s))
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)
    };
    let kind = parts.first().copied().ok_or_else(bad)?;
    match kind {
        "refs" => Ok(Uri::Refs {
            owner: seg(1)?,
            repo: seg(2)?,
        }),
        "collab" if parts.get(1) == Some(&"board") => Ok(Uri::Board {
            owner: seg(2)?,
            repo: seg(3)?,
        }),
        "collab" if parts.get(1) == Some(&"thread") => Ok(Uri::Thread {
            owner: seg(2)?,
            repo: seg(3)?,
            thread: seg(4)?,
        }),
        "wal" => {
            let mut from = 0u64;
            for pair in query_str.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                if k == "from" {
                    from = v.parse::<u64>().map_err(|_| {
                        ErrorData::invalid_params(
                            format!("wal from= must be a seq number, got {v:?}"),
                            None,
                        )
                    })?;
                }
            }
            Ok(Uri::Wal {
                owner: seg(1)?,
                repo: seg(2)?,
                from,
            })
        }
        _ => Err(bad()),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let (Some(b'%'), Some(hex)) = (bytes.get(i), bytes.get(i + 1..i + 3))
            && let Some(b) = std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        if let Some(b) = bytes.get(i) {
            out.push(*b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `resources/list`: one entry per (repository, kind) for this host's
/// repositories — a page, bounded by [`LIST_MAX_REPOS`]. The page count travels
/// in `_meta` so a client knows an index exists beyond what it was handed.
pub async fn list(st: &Arc<AppState>, principal: &str) -> Result<Vec<Resource>, ErrorData> {
    let mut repos = st
        .registry
        .list()
        .await
        .map_err(|e| ErrorData::internal_error(format!("listing repositories: {e}"), None))?;
    repos.sort_by_key(std::string::ToString::to_string);
    let total = repos.len();
    repos.truncate(LIST_MAX_REPOS);
    let mut out = Vec::with_capacity(repos.len() * 3);
    for id in &repos {
        let owner = id.owner();
        let repo = id.name();
        out.push(
            Resource::new(format!("{SCHEME}refs/{owner}/{repo}"), format!("{owner}/{repo} refs"))
                .with_title(format!("{owner}/{repo}: refs"))
                .with_description(
                    "HEAD plus a page of branches and tags, with the WAL seq it was read at",
                )
                .with_mime_type(JSON_MIME)
                .with_meta(page_meta(total, repos.len(), principal)),
        );
        out.push(
            Resource::new(format!("{SCHEME}wal/{owner}/{repo}"), format!("{owner}/{repo} WAL"))
                .with_title(format!("{owner}/{repo}: WAL tail"))
                .with_description(
                    "PUSH / REF_UPDATE / COMPACT / CHECKPOINT entries from the newest 200 seqs \
                     (add ?from=N for another window)",
                )
                .with_mime_type(JSON_MIME)
                .with_meta(page_meta(total, repos.len(), principal)),
        );
        out.push(
            Resource::new(
                format!("{SCHEME}collab/board/{owner}/{repo}"),
                format!("{owner}/{repo} board"),
            )
            .with_title(format!("{owner}/{repo}: collaboration board"))
            .with_description("The D1 board projection (cards, owners, claims) over refs/collab/*")
            .with_mime_type(JSON_MIME)
            .with_meta(page_meta(total, repos.len(), principal)),
        );
    }
    Ok(out)
}

fn page_meta(total: usize, listed: usize, principal: &str) -> MetaObject {
    let mut meta = serde_json::Map::new();
    meta.insert("walgit/repos_total".into(), json!(total));
    meta.insert("walgit/repos_listed".into(), json!(listed));
    meta.insert("walgit/caller".into(), json!(principal));
    if total > listed {
        meta.insert(
            "walgit/hint".into(),
            json!("list is paged by repository count; a specific repository's resources are in \
                   resources/templates/list"),
        );
    }
    MetaObject(meta)
}

/// `resources/templates/list`: the RFC 6570 forms of the four kinds. Clients
/// build concrete URIs from these instead of guessing the grammar.
pub fn templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(format!("{SCHEME}refs/{{owner}}/{{repo}}"), "refs")
            .with_title("Repository refs")
            .with_description(
                "HEAD plus a page of branches and tags, with the WAL seq it was read at",
            )
            .with_mime_type(JSON_MIME),
        ResourceTemplate::new(format!("{SCHEME}wal/{{owner}}/{{repo}}{{?from}}"), "wal")
            .with_title("WAL tail")
            .with_description("WAL entries from ?from=<seq> (default: the newest 200 seqs)")
            .with_mime_type(JSON_MIME),
        ResourceTemplate::new(
            format!("{SCHEME}collab/board/{{owner}}/{{repo}}"),
            "board",
        )
        .with_title("Collaboration board")
        .with_description("The D1 board projection over refs/collab/*")
        .with_mime_type(JSON_MIME),
        ResourceTemplate::new(
            format!("{SCHEME}collab/thread/{{owner}}/{{repo}}/{{thread}}"),
            "thread",
        )
        .with_title("Collaboration thread")
        .with_description("One thread: its signed entries, projected status and reviews")
        .with_mime_type(JSON_MIME),
    ]
}

/// The version of one resource — and the cache that makes it honest.
///
/// The version must move **iff** what `resources/read` would return changes. A
/// repository-wide seq is not that: every subscriber of every card woke on
/// every ref write (a `ci-artifacts` blob landing woke card watchers), and a
/// `card_claim` TTL lapsing moves no ref at all while it does move the board's
/// owner column (D52's 2026-10-20 amendment). The first attempt at the second
/// half carried `now / 60` and woke every board subscriber once a minute
/// forever — `walgit-mcp-resource-version-churn-20261101`.
///
/// So a board or thread version is computed from the projection the read would
/// serve, never from the clock and never from the repository's seq:
/// * board: `b:<witness>[:claim:<expires_at>]` — `<witness>` fingerprints the
///   cards themselves (id, tip, status, owner), so it moves when a card moves
///   and not when an unrelated ref does; `claim:` is the earliest claim that is
///   still live, so a lapse is exactly one more move;
/// * thread: `h:<tip oid>` — that thread's own tip (`h:-` while unknown);
/// * refs / wal: `seq:<n>` — they *are* the repository-wide view, so that is
///   their version, and subscribing to them watches the whole repository.
pub(super) async fn version(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    uri: &Uri,
) -> Result<String, PollError> {
    let (seq, handle) = refs_level_poll(st, uri.owner(), uri.repo()).await?;
    version_at(st, headers, principal, uri, seq, &handle).await
}

/// [`version`] for a caller that already synced refs (a read would otherwise
/// pay for the manifest twice).
async fn version_at(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    uri: &Uri,
    seq: u64,
    handle: &walgit_wal::RepoHandle,
) -> Result<String, PollError> {
    match uri {
        Uri::Board { .. } => {
            let projection = projection(st, headers, principal, uri, handle).await?;
            Ok(board_version(&projection, unix_now().cast_signed()))
        }
        Uri::Thread { thread, .. } => {
            let projection = projection(st, headers, principal, uri, handle).await?;
            Ok(thread_version(projection.heads.get(thread).map(String::as_str)))
        }
        Uri::Refs { .. } | Uri::Wal { .. } => Ok(format!("seq:{seq}")),
    }
}

/// The board's version: the cards' witness, plus the earliest claim that is
/// still live. A board with no claim has no clock in its version at all.
fn board_version(projection: &Projection, now: i64) -> String {
    let claim = projection
        .expiries
        .iter()
        .copied()
        .filter(|expires_at| *expires_at >= now)
        .min();
    match claim {
        Some(expires_at) => format!("b:{:016x}:claim:{expires_at}", projection.witness),
        None => format!("b:{:016x}", projection.witness),
    }
}

/// A thread's version: its own tip, so a write anywhere else moves nothing.
fn thread_version(head: Option<&str>) -> String {
    format!("h:{}", head.unwrap_or("-"))
}

/// What a board/thread version is computed from. Built from the board document
/// itself, so the version and the body `resources/read` returns cannot disagree.
pub(super) struct Projection {
    /// Fingerprint of `refs/collab/{inbox,meta}` — the cache key. It ignores
    /// the `ci-artifacts` namespace deliberately: those refs change constantly
    /// and no projection reads them, so they must not cost a rebuild.
    pub collab: u64,
    /// Fingerprint of the cards the board projects.
    pub witness: u64,
    /// The board document, i.e. exactly what reading the board returns.
    pub board: Value,
    /// `thread id → tip oid` (`BoardCard::last_oid`).
    pub heads: HashMap<String, String>,
    /// `expires_at` of every card that carries a claim.
    pub expiries: Vec<i64>,
}

/// The projection for this repository, cached across subscribers and rebuilt
/// only when the collab refs moved.
async fn projection(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    uri: &Uri,
    handle: &walgit_wal::RepoHandle,
) -> Result<Arc<Projection>, PollError> {
    let (owner, repo) = (uri.owner(), uri.repo());
    let key = format!("{owner}/{repo}");
    let collab = collab_digest(handle);
    if let Some(hit) = st.mcp.projection(&key, collab) {
        return Ok(hit);
    }
    let base = format!("/{}/{}", enc(owner), enc(repo));
    let board = Call::new(st, headers, principal)
        .get(&format!("{base}/api/collab/board"))
        .await
        .map_err(|e| PollError::Other(api_err(&e)))?;
    let (heads, expiries, witness) = index_board(&board);
    Ok(st.mcp.remember_projection(
        key,
        Arc::new(Projection {
            collab,
            witness,
            board,
            heads,
            expiries,
        }),
    ))
}

/// Fingerprint of the refs a collab projection is built from.
///
/// Read from the local ref index (already synced), not the bucket: this runs on
/// every tick of every board/thread subscription, and its only job is to answer
/// "could the projection have changed?" without paying for the aggregation.
fn collab_digest(handle: &walgit_wal::RepoHandle) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match handle.local().refs() {
        Ok(snapshot) => {
            for r in snapshot.refs.iter().filter(|r| {
                let name = r.name.as_str();
                name.starts_with("refs/collab/inbox/") || name.starts_with("refs/collab/meta/")
            }) {
                r.name.hash(&mut hasher);
                r.oid.hash(&mut hasher);
            }
        }
        // No local refs (a repository whose object work lives elsewhere): give
        // the caller a fingerprint that cannot be mistaken for a cached one.
        Err(_) => return 0,
    }
    hasher.finish()
}

/// `thread id → tip oid`, every claim's expiry and the cards' witness, out of
/// one board document.
fn index_board(board: &Value) -> (HashMap<String, String>, Vec<i64>, u64) {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut heads = HashMap::new();
    let mut expiries = Vec::new();
    for (column_index, column) in board
        .get("columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        column_index.hash(&mut hasher);
        for card in column
            .get("cards")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for field in ["id", "last_oid", "status", "owner"] {
                card.get(field)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .hash(&mut hasher);
            }
            if let (Some(id), Some(oid)) = (
                card.get("id").and_then(Value::as_str),
                card.get("last_oid").and_then(Value::as_str),
            ) {
                heads.insert(id.to_string(), oid.to_string());
            }
            if let Some(expires_at) = card
                .get("claim")
                .and_then(|claim| claim.get("expires_at"))
                .and_then(Value::as_i64)
            {
                expiries.push(expires_at);
            }
        }
    }
    (heads, expiries, hasher.finish())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Why a read failed: the repository is gone (so the resource *list* changed),
/// or this instance cannot read it right now (transient). The distinction is
/// what lets a subscription emit `list_changed` honestly instead of guessing
/// (D64).
pub(super) enum PollError {
    Gone,
    Other(ErrorData),
}

impl PollError {
    pub(super) fn into_error_data(self) -> ErrorData {
        match self {
            Self::Gone => ErrorData::invalid_params("no such repository", None),
            Self::Other(e) => e,
        }
    }
}

pub(super) async fn refs_level(
    st: &Arc<AppState>,
    owner: &str,
    repo: &str,
) -> Result<(u64, Arc<walgit_wal::RepoHandle>), ErrorData> {
    refs_level_poll(st, owner, repo)
        .await
        .map_err(PollError::into_error_data)
}

async fn refs_level_poll(
    st: &Arc<AppState>,
    owner: &str,
    repo: &str,
) -> Result<(u64, Arc<walgit_wal::RepoHandle>), PollError> {
    let id = walgit_git::RepoId::new(owner, repo)
        .map_err(|e| PollError::Other(ErrorData::invalid_params(format!("{owner}/{repo}: {e}"), None)))?;
    let handle = st.registry.open(&id).await.map_err(|e| match e {
        walgit_wal::WalError::NotFound => PollError::Gone,
        other => PollError::Other(ErrorData::internal_error(
            format!("opening {id}: {other}"),
            None,
        )),
    })?;
    // `read_log` (the WAL resource) and the manifest read both need the refs
    // phase; taking the guard here and dropping it immediately keeps the seq
    // consistent with the guard's own sync without holding it across I/O.
    {
        let _guard = handle.sync_refs().await.map_err(|e| {
            PollError::Other(ErrorData::internal_error(
                format!("syncing refs of {id}: {e}"),
                None,
            ))
        })?;
    }
    Ok((handle.applied_seq(), handle))
}

/// `resources/read`: the resource body as one JSON text part.
pub async fn read(
    st: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    principal: &str,
    uri_str: &str,
) -> Result<ResourceContents, ErrorData> {
    let uri = parse(uri_str)?;
    let call = Call::new(st, headers, principal);
    let (owner, repo) = (uri.owner().to_string(), uri.repo().to_string());
    let base = format!("/{}/{}", enc(&owner), enc(&repo));
    let (seq, handle) = refs_level(st, &owner, &repo).await?;
    let version = version_at(st, headers, principal, &uri, seq, &handle)
        .await
        .map_err(PollError::into_error_data)?;
    let body = match &uri {
        Uri::Refs { .. } => {
            let summary = call
                .get(&format!("{base}/api/refs"))
                .await
                .map_err(|e| api_err(&e))?;
            let branches = call
                .get(&format!(
                    "{base}/api/refs/branches{}",
                    query(&[("n", Some("100".into()))])
                ))
                .await
                .map_err(|e| api_err(&e))?;
            let tags = call
                .get(&format!(
                    "{base}/api/refs/tags{}",
                    query(&[("n", Some("100".into()))])
                ))
                .await
                .map_err(|e| api_err(&e))?;
            json!({
                "repo": format!("{owner}/{repo}"),
                "seq": seq,
                "head": summary.get("head").cloned().unwrap_or(Value::Null),
                "branches": branches.get("refs").cloned().unwrap_or_else(|| json!([])),
                "branches_more": branches.get("more").cloned().unwrap_or(json!(false)),
                "tags": tags.get("refs").cloned().unwrap_or_else(|| json!([])),
                "tags_more": tags.get("more").cloned().unwrap_or(json!(false)),
            })
        }
        Uri::Wal { from, .. } => {
            let (entries, head_seq) = wal_page(&handle, *from).await?;
            let first = entries
                .first()
                .and_then(|e| e.get("seq"))
                .and_then(Value::as_u64)
                .unwrap_or(*from);
            let last = entries
                .last()
                .and_then(|e| e.get("seq"))
                .and_then(Value::as_u64)
                .unwrap_or(first);
            json!({
                "repo": format!("{owner}/{repo}"),
                "head_seq": head_seq,
                "from": first,
                "to": last,
                "entries": entries,
                // Computed from the page actually returned, not from the
                // requested `from`: `from = 0` means "the newest page", where
                // there is by definition nothing newer to page to.
                "more": head_seq > last,
            })
        }
        // The projection *is* the board body: one build serves the read, the
        // version and every subscriber's next tick (see [`projection`]).
        Uri::Board { .. } => {
            projection(st, headers, principal, &uri, &handle)
                .await
                .map_err(PollError::into_error_data)?
                .board
                .clone()
        }
        Uri::Thread { thread, .. } => call
            .get(&format!("{base}/api/collab/threads/{}", enc(thread)))
            .await
            .map_err(|e| api_err(&e))?,
    };
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.entry("seq").or_insert(json!(seq));
    }
    Ok(ResourceContents::TextResourceContents {
        uri: uri_str.to_string(),
        mime_type: Some(JSON_MIME.to_string()),
        text: serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
        // The version a subscriber compares: `web/SKILL.md` tells clients to
        // read `_meta.version` and re-read when it moved, so it must be the
        // exact string `resources/subscribe` notifies about — same function,
        // not a second spelling of it.
        meta: Some(meta(&version)),
    })
}

/// `_meta` carrying the resource version (the value `resources/subscribe`
/// notifies about).
fn meta(version: &str) -> MetaObject {
    let mut map = serde_json::Map::new();
    map.insert("version".to_string(), Value::String(version.to_string()));
    MetaObject(map)
}

/// The WAL window from `from` (0 = the newest page), bounded by [`WAL_PAGE`].
pub async fn wal_page(
    handle: &Arc<walgit_wal::RepoHandle>,
    from: u64,
) -> Result<(Vec<Value>, u64), ErrorData> {
    let head = handle.applied_seq();
    let start = if from == 0 {
        head.saturating_sub(WAL_PAGE - 1).max(1)
    } else {
        from
    };
    let end = start.saturating_add(WAL_PAGE - 1).min(head);
    let entries = handle
        .read_log(start, Some(end))
        .await
        .map_err(|e| ErrorData::internal_error(format!("reading the WAL log: {e}"), None))?;
    Ok((entries.iter().map(entry_json).collect(), head))
}

/// One `LogEntry` as JSON: what moved, who wrote it, and which pack/checkpoint
/// it installed — the facts an agent needs to act, not the proto.
pub(super) fn entry_json(e: &walgit_proto::v1::LogEntry) -> Value {
    use walgit_proto::v1::EntryKind;
    let kind = EntryKind::try_from(e.kind).map_or("unspecified", |k| match k {
        EntryKind::Unspecified => "unspecified",
        EntryKind::Push => "push",
        EntryKind::Compact => "compact",
        EntryKind::RefUpdate => "ref_update",
        EntryKind::Checkpoint => "checkpoint",
        EntryKind::Settings => "settings",
    });
    let refs: Vec<Value> = e
        .txn
        .as_ref()
        .map(|t| {
            t.updates
                .iter()
                .map(|u| {
                    json!({
                        "name": u.name,
                        "old": u.old_oid,
                        "new": u.new_oid,
                        "symbolic_target": u.new_symbolic_target,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "seq": e.seq,
        "kind": kind,
        "at": e.created_at.as_ref().map(|t| t.seconds),
        "writer": e.writer,
        "refs": refs,
        "pack": e.pack.as_ref().map(|p| json!({
            "checksum": p.checksum,
            "bytes": p.pack_size,
            "objects": p.object_count,
            "tier": p.tier,
        })),
        "supersedes": e.supersedes,
        "checkpoint": e.checkpoint.as_ref().map(|c| json!({
            "seq": c.seq,
            "key": c.key,
        })),
        "meta": e.meta,
    })
}

/// An inner API failure as a JSON-RPC error, keeping the API's own sentence
/// (it is the one that names the fix).
pub fn api_err(e: &ApiError) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection_of(witness: u64, expiries: &[i64]) -> Projection {
        Projection {
            collab: 1,
            witness,
            board: json!({}),
            heads: HashMap::from([("card-1".to_string(), "oid1".to_string())]),
            expiries: expiries.to_vec(),
        }
    }

    /// The bug this replaced (`walgit-mcp-resource-version-churn-20261101`): the
    /// board version carried `now / 60`, so every board subscriber was woken
    /// once a minute forever without anything changing.
    #[test]
    fn a_board_with_no_live_claim_has_no_clock_in_its_version() {
        let board = projection_of(7, &[]);
        assert_eq!(board_version(&board, 600), "b:0000000000000007");
        assert_eq!(
            board_version(&board, 60 * 60 * 24),
            "b:0000000000000007",
            "a day later, still nothing to wake about"
        );
    }

    /// A claim's TTL lapsing moves no ref, so the version has to carry it — but
    /// only the *earliest live* one, and only until it lapses.
    #[test]
    fn the_board_version_follows_the_earliest_live_claim() {
        let board = projection_of(7, &[700, 650, 900]);
        assert_eq!(
            board_version(&board, 600),
            "b:0000000000000007:claim:650"
        );
        assert_eq!(
            board_version(&board, 650),
            "b:0000000000000007:claim:650",
            "the last live instant still counts as live"
        );
        assert_eq!(
            board_version(&board, 651),
            "b:0000000000000007:claim:700",
            "when the earliest lapses the version moves to the next one"
        );
        assert_eq!(
            board_version(&board, 901),
            "b:0000000000000007",
            "with no claim left the version is the cards' witness again"
        );
    }

    /// A thread's version is the thread's own tip: a write anywhere else in the
    /// repository must move nothing (that was the second half of the bug —
    /// subscribers to a card woke on every `ci-artifacts` ref).
    #[test]
    fn a_thread_version_is_its_own_tip() {
        assert_eq!(thread_version(Some("abc")), "h:abc");
        assert_eq!(thread_version(None), "h:-");
    }

    /// The board document is the one index both versions are computed from, so
    /// parsing it wrong silently reintroduces the churn.
    #[test]
    fn the_board_index_carries_heads_witness_and_claim_expiries() {
        let board = json!({"columns": [{"name": "Open", "cards": [
            {"id": "card-1", "last_oid": "oid1", "status": "open", "owner": "", "claim": {"expires_at": 700}},
            {"id": "card-2", "last_oid": "oid2", "status": "in-progress", "owner": "alice", "claim": null},
            {"id": "card-3"}
        ]}]});
        let (heads, expiries, witness) = index_board(&board);
        assert_eq!(heads.get("card-1").map(String::as_str), Some("oid1"));
        assert_eq!(heads.get("card-2").map(String::as_str), Some("oid2"));
        assert_eq!(heads.len(), 2, "a card without a tip names no thread");
        assert_eq!(expiries, vec![700]);

        // The witness moves with a card's identity and with nothing else.
        let moved = json!({"columns": [{"name": "Open", "cards": [
            {"id": "card-1", "last_oid": "oid1", "status": "open", "owner": ""},
            {"id": "card-2", "last_oid": "oid3", "status": "in-progress", "owner": "alice"}
            ,{"id": "card-3"}
        ]}]});
        assert_ne!(index_board(&moved).2, witness, "a new tip is a new witness");
        let renamed = json!({"columns": [{"name": "Open", "cards": [
            {"id": "card-1", "last_oid": "oid1", "status": "open", "owner": ""},
            {"id": "card-2", "last_oid": "oid2", "status": "in-progress", "owner": "bob"}
            ,{"id": "card-3"}
        ]}]});
        assert_ne!(index_board(&renamed).2, witness, "a new owner is a new witness");
        let same = json!({"columns": [{"name": "Open", "cards": [
            {"id": "card-1", "last_oid": "oid1", "status": "open", "owner": ""},
            {"id": "card-2", "last_oid": "oid2", "status": "in-progress", "owner": "alice"}
            ,{"id": "card-3"}
        ]}]});
        assert_eq!(index_board(&same).2, witness, "the same board is the same witness");
    }

    #[test]
    fn parses_the_four_kinds() {
        assert_eq!(
            parse("walgit://refs/acme/widgets").unwrap(),
            Uri::Refs {
                owner: "acme".into(),
                repo: "widgets".into()
            }
        );
        assert_eq!(
            parse("walgit://wal/acme/widgets?from=42").unwrap(),
            Uri::Wal {
                owner: "acme".into(),
                repo: "widgets".into(),
                from: 42
            }
        );
        assert_eq!(
            parse("walgit://wal/acme/widgets").unwrap(),
            Uri::Wal {
                owner: "acme".into(),
                repo: "widgets".into(),
                from: 0
            }
        );
        assert_eq!(
            parse("walgit://collab/board/acme/widgets").unwrap(),
            Uri::Board {
                owner: "acme".into(),
                repo: "widgets".into()
            }
        );
        assert_eq!(
            parse("walgit://collab/thread/acme/widgets/card-42").unwrap(),
            Uri::Thread {
                owner: "acme".into(),
                repo: "widgets".into(),
                thread: "card-42".into()
            }
        );
        // Percent-encoded segments decode before use.
        assert_eq!(
            parse("walgit://refs/acme/we%20b").unwrap(),
            Uri::Refs {
                owner: "acme".into(),
                repo: "we b".into()
            }
        );
        assert!(parse("https://refs/acme/x").is_err());
        assert!(parse("walgit://refs/acme").is_err());
        assert!(parse("walgit://nope/acme/x").is_err());
        assert!(parse("walgit://wal/acme/x?from=abc").is_err());
    }

    #[test]
    fn templates_cover_every_kind() {
        let list = templates();
        assert_eq!(list.len(), 4);
        assert!(
            list.iter()
                .any(|t| t.uri_template == "walgit://refs/{owner}/{repo}")
        );
        assert!(
            list.iter()
                .any(|t| t.uri_template == "walgit://wal/{owner}/{repo}{?from}")
        );
    }
}
