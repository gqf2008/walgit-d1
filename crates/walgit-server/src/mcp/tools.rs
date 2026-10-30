//! The `/mcp` tool surface (D64).
//!
//! **The tools are walgit's own JSON API, called in-process.** Each one builds
//! the request the bundled UI would build, runs it through this process's
//! router ([`super::api::Call`]), and returns the answer. Nothing is
//! reimplemented, so placement (D30's 503 for a repo this host does not serve),
//! freshness (every read revalidates), authz (`actor == caller`) and the
//! response shapes cannot drift from the HTTP contract.
//!
//! The names, argument names and semantics are the ones the agent-side adapter
//! (`walgit mcp`, D49) exposes, so a model that learned one lane can use the
//! other: `repo_refs {repo, kind}`, `repo_tree {repo, rev, path}`, and so on, with
//! `repo` as `"<owner>/<repo>"`. Two differences are inherent and stated in the
//! descriptions: this lane always names a repository (it serves a host, not one
//! checkout), and it reads the WAL through the store rather than a local clone.
//!
//! **Write tools need the caller's own key** (D64): a collab entry counts only
//! when it verifies against the key registered for its own actor, so
//! `collab_entry` / `collab_claim` are registered only when `mcp.key_dir` holds
//! `<actor>.ed25519`. Without it the surface is read-only, and `tools/list`
//! never advertises what `tools/call` would refuse (D49).

use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::{json, Value};

use super::api::{enc, enc_path, query, ApiError as CallError, Call};
use super::signing;
use crate::AppState;

/// A registered tool.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// Needs a caller key (`mcp.key_dir`) — a write tool.
    pub write: bool,
    pub schema: Value,
}

/// Why a tool call did not produce a result. The spec's split: a wrong
/// *request* is `-32602` (the model can fix it), a tool that ran and failed is
/// `isError` content the model must read.
#[derive(Debug)]
pub enum ToolError {
    InvalidParams(String),
    Execution(String),
}

impl ToolError {
    fn invalid(m: impl Into<String>) -> Self {
        ToolError::InvalidParams(m.into())
    }
    /// A tool ran and failed. The API's own sentence is kept — it is the one
    /// that names the fix.
    fn exec(e: CallError) -> Self {
        ToolError::Execution(e.message)
    }
    /// A failure from the collab loader (the same `ApiError` the HTTP surface
    /// answers with) — its own sentence already names the fix.
    fn from_server(e: &crate::error::ApiError) -> Self {
        ToolError::Execution(e.message())
    }
}

/// The descriptions are load-bearing: a model reads them instead of the docs,
/// so they carry the rules a wrong call would violate (bytes never travel over
/// MCP; a card's ownership comes from `collab_claim`; …).
const REPO_ARG: &str = "repository as \"<owner>/<repo>\"";

/// Every tool this instance registers. `write` tools appear only when a key
/// directory is configured.
pub fn defs(write: bool) -> Vec<Tool> {
    let mut out = vec![
        Tool {
            name: "repo_list",
            description: "List the repositories on this host.",
            write: false,
            schema: schema(&json!({}), &json!([])),
        },
        Tool {
            name: "repo_owners",
            description: "List owners, or one owner's repositories.",
            write: false,
            schema: schema(&json!({"owner": {"type": "string"}}), &json!([])),
        },
        Tool {
            name: "repo_refs",
            description: "Refs of a repository: the `head` summary by default, or one namespace \
                          (`branches` | `tags` | `all` | `collab`, paged by `after`).",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "kind": {"type": "string", "description": "head (default) | branches | tags | all | collab"},
                    "after": {"type": "string", "description": "page cursor: the last ref name seen"},
                }), &json!(["repo"])),
        },
        Tool {
            name: "repo_resolve",
            description: "Resolve a revision (branch, tag, partial sha) to an object id.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision to resolve"},
                }), &json!(["repo", "rev"])),
        },
        Tool {
            name: "repo_tree",
            description: "One directory of the tree at a revision.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision (branch, tag or sha)"},
                    "path": {"type": "string", "description": "directory path inside the tree (default: root)"},
                }), &json!(["repo", "rev"])),
        },
        Tool {
            name: "repo_blob",
            description: "One file's blob envelope (text contents; binary files report `binary: true`). \
                          Bytes do not travel over MCP — clone/fetch those with git and the bundle-uri recipes.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision (branch, tag or sha)"},
                    "path": {"type": "string", "description": "file path"},
                }), &json!(["repo", "rev", "path"])),
        },
        Tool {
            name: "repo_commits",
            description: "Recent commits of a ref (optionally for one path), newest first.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "ref": {"type": "string", "description": "ref name (default HEAD)"},
                    "path": {"type": "string", "description": "restrict history to this path"},
                    "skip": {"type": "integer", "description": "commits to skip (paging)"},
                    "n": {"type": "integer", "description": "page size, 1..=200 (default 35)"},
                }), &json!(["repo"])),
        },
        Tool {
            name: "repo_diff",
            description: "Diff between two revisions (patch by default).",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "from": {"type": "string", "description": "base revision"},
                    "to": {"type": "string", "description": "target revision"},
                    "format": {"type": "string", "description": "patch (default) | stat | name-status"},
                }), &json!(["repo", "from", "to"])),
        },
        Tool {
            name: "collab_board",
            description: "The D1 work-unit board (threads projected under the repo's board definition). \
                          Read-only: moving a card is a signed `status` entry.",
            write: false,
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: "collab_thread",
            description: "One collab thread: parent-ordered entries with per-entry signature verification.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "id": {"type": "string", "description": "thread id"},
                }), &json!(["repo", "id"])),
        },
        Tool {
            name: "collab_report",
            description: "Read-only dashboard over refs/collab/*: threads, PR status, verification health, activity.",
            write: false,
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: "wal_ls",
            description: "WAL entries of a repository (the append-only source of truth), seq-ordered. \
                          `head_seq` is what a resource subscription's version reports.",
            write: false,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "from": {"type": "integer", "description": "first seq (default: the newest page)"},
                    "to": {"type": "integer", "description": "last seq (default: from + 199)"},
                }), &json!(["repo"])),
        },
    ];
    if write {
        out.push(Tool {
            name: "collab_entry",
            description: "Append one signed collab entry (issue/comment/patch/review/merge_result/status) and \
                          return its oid and ref. The server signs as `actor` with the key in `mcp.key_dir` and \
                          refuses when that key is not the one registered for the actor (the entry could never \
                          verify, and the log is append-only). Rules the board depends on: a thread needs an \
                          `issue` to be a card; ownership comes from `collab_claim` (or, without claims, a \
                          `status` naming an `owner`); `patch` carries base/head refs; only verified `approve` \
                          reviews count for the merge rule.",
            write: true,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "kind": {"type": "string", "enum": ["issue", "comment", "patch", "review", "merge_result", "status"]},
                    "id": {"type": "string", "description": "thread id, shared by every entry"},
                    "actor": {"type": "string", "description": "the principal the entry is written as; it must be the authenticated caller and have a key in `mcp.key_dir`"},
                    "body": {"type": "string", "description": "the entry body as a JSON object string"},
                    "parent": {"type": "string", "description": "previous entry's oid (default: the thread head; \"\" for a root)"},
                    "base": {"type": "string", "description": "patch only"},
                    "head": {"type": "string", "description": "patch only"},
                }), &json!(["repo", "kind", "id", "actor", "body"])),
        });
        out.push(Tool {
            name: "collab_claim",
            description: "Claim a work card before working on it (D1 protocol §8.4): publish a signed \
                          `card_claim`, re-read the collab log, and succeed only for the deterministic winner \
                          (earliest live claim by ts/actor/oid). A losing call returns `isError` whose text \
                          contains `stand-down:` and names the holder and expiry — do not start work. \
                          Duplicate claims are legal, duplicate execution is not; a claim expires after `ttl` seconds.",
            write: true,
            schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "id": {"type": "string", "description": "card thread id"},
                    "actor": {"type": "string", "description": "the principal claiming the card; it must be the authenticated caller and have a key in `mcp.key_dir`"},
                    "ttl": {"type": "integer", "description": "claim lifetime in seconds (120..=86400, default 300)"},
                    "worktree": {"type": "string", "description": "worktree the holder will work in"},
                    "branch": {"type": "string", "description": "branch the holder will work on"},
                    "work": {"type": "string", "description": "one-line plan"},
                    "session": {"type": "string", "description": "session id for restart recovery (default: this MCP session)"},
                }), &json!(["repo", "id", "actor"])),
        });
    }
    out
}

fn schema(properties: &Value, required: &Value) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The tool surface as one closed set. Every tool has a name, a tier and an arm
/// in [`run`]; the dispatch match is exhaustive over this enum, so a tool can
/// neither be advertised without an implementation nor implemented without
/// being advertised (the test below pins the schema table to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    RepoList,
    RepoOwners,
    RepoRefs,
    RepoResolve,
    RepoTree,
    RepoBlob,
    RepoCommits,
    RepoDiff,
    CollabBoard,
    CollabThread,
    CollabReport,
    WalLs,
    CollabEntry,
    CollabClaim,
}

const ALL: [Kind; 14] = [
    Kind::RepoList,
    Kind::RepoOwners,
    Kind::RepoRefs,
    Kind::RepoResolve,
    Kind::RepoTree,
    Kind::RepoBlob,
    Kind::RepoCommits,
    Kind::RepoDiff,
    Kind::CollabBoard,
    Kind::CollabThread,
    Kind::CollabReport,
    Kind::WalLs,
    Kind::CollabEntry,
    Kind::CollabClaim,
];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::RepoList => "repo_list",
            Self::RepoOwners => "repo_owners",
            Self::RepoRefs => "repo_refs",
            Self::RepoResolve => "repo_resolve",
            Self::RepoTree => "repo_tree",
            Self::RepoBlob => "repo_blob",
            Self::RepoCommits => "repo_commits",
            Self::RepoDiff => "repo_diff",
            Self::CollabBoard => "collab_board",
            Self::CollabThread => "collab_thread",
            Self::CollabReport => "collab_report",
            Self::WalLs => "wal_ls",
            Self::CollabEntry => "collab_entry",
            Self::CollabClaim => "collab_claim",
        }
    }

    /// The write tier: advertised (and callable) only when this host can sign
    /// for a caller (`mcp.key_dir`), per D49.
    fn write(self) -> bool {
        matches!(self, Self::CollabEntry | Self::CollabClaim)
    }

    fn parse(name: &str) -> Option<Self> {
        ALL.into_iter().find(|k| k.name() == name)
    }
}

/// Hold `arguments` to the schema the tool advertises — the same rules the
/// adapter applies, so a call that fails on one lane fails on the other.
pub fn validate_arguments(spec: &Tool, args: &Value) -> Result<(), ToolError> {
    let Some(obj) = args.as_object() else {
        return Err(ToolError::invalid("`arguments` must be an object"));
    };
    let props = spec.schema.get("properties").and_then(Value::as_object);
    for (key, value) in obj {
        let Some(field) = props.and_then(|p| p.get(key)) else {
            return Err(ToolError::invalid(format!(
                "unknown argument `{key}` for `{}`",
                spec.name
            )));
        };
        if value.is_null() {
            return Err(ToolError::invalid(format!(
                "`{key}` must be a {0}, not null",
                field.get("type").and_then(Value::as_str).unwrap_or("value")
            )));
        }
        let declared = field.get("type").and_then(Value::as_str);
        let ok = match declared {
            Some("string") => value.is_string(),
            Some("integer") => value.as_u64().is_some(),
            Some("boolean") => value.is_boolean(),
            _ => true,
        };
        if !ok {
            return Err(ToolError::invalid(format!(
                "`{key}` must be a {}",
                declared.unwrap_or("value")
            )));
        }
        // Empty is meaningless for every argument except `collab_entry.parent`,
        // where "" is the documented "this is a root entry" marker.
        if key != "parent" && value.as_str().is_some_and(|v| v.trim().is_empty()) {
            return Err(ToolError::invalid(format!("`{key}` must not be empty")));
        }
    }
    if let Some(required) = spec.schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            match obj.get(name) {
                Some(v) if !v.is_null() => {}
                _ => return Err(ToolError::invalid(format!("`{name}` is required"))),
            }
        }
    }
    Ok(())
}

/// One `tools/call` → the MCP tool result (`content` + optional `isError`).
pub async fn call(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    let write = st.cfg.mcp.key_dir.is_some();
    let Some(spec) = defs(write).into_iter().find(|t| t.name == name) else {
        return Err(ToolError::invalid(format!("unknown tool: {name}")));
    };
    validate_arguments(&spec, args)?;
    run(st, headers, principal, session, name, args).await
}

/// The tool's answer as the one content block MCP carries: pretty JSON as text.
/// A model reads it, and the JSON is the same body the HTTP API sends.
pub fn as_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn repo_arg(args: &Value) -> Result<(String, String), ToolError> {
    let repo = args
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid("`repo` is required (\"<owner>/<repo>\")"))?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| ToolError::invalid(format!("`repo` must be \"<owner>/<repo>\", got {repo:?}")))?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(ToolError::invalid(format!(
            "`repo` must be \"<owner>/<repo>\", got {repo:?}"
        )));
    }
    Ok((owner.to_string(), name.to_string()))
}

fn str_arg<'v>(args: &'v Value, key: &str) -> Option<&'v str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

async fn run(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    let call = Call::new(st, headers);
    let p = |path: String| async move { call.get(&path).await.map_err(ToolError::exec) };
    let kind = Kind::parse(name)
        .ok_or_else(|| ToolError::invalid(format!("unknown tool `{name}`")))?;
    // `tools/list` hides the write tier when this host cannot sign for a caller;
    // a call by name must not reach what the list hides (D49).
    if kind.write() && st.cfg.mcp.key_dir.is_none() {
        return Err(ToolError::invalid(format!("unknown tool `{name}`")));
    }
    match kind {
        Kind::RepoList => {
            let mut repos = st
                .registry
                .list()
                .await
                .map_err(|e| ToolError::Execution(format!("listing repositories: {e}")))?;
            repos.sort_by_key(std::string::ToString::to_string);
            Ok(json!({
                "repos": repos.iter().map(|r| json!({
                    "repo": format!("{}/{}", r.owner(), r.name()),
                })).collect::<Vec<_>>()
            }))
        }
        Kind::RepoOwners => match str_arg(args, "owner") {
            Some(owner) => p(format!("/services/api/owners/{}", enc(owner)))
                .await
                .map(|v| json!({ "owner": owner, "repos": v })),
            None => p("/services/api/owners".to_string())
                .await
                .map(|v| json!({ "owners": v })),
        },
        Kind::RepoRefs => {
            let (owner, repo) = repo_arg(args)?;
            let base = format!("/{}/{}", enc(&owner), enc(&repo));
            match str_arg(args, "kind") {
                None | Some("head") => p(format!("{base}/api/refs")).await,
                Some(kind @ ("branches" | "tags" | "all" | "collab")) => {
                    let after = str_arg(args, "after").map(std::string::ToString::to_string);
                    p(format!("{base}/api/refs/{}", enc(kind)) + &query(&[("after", after)])).await
                }
                Some(other) => Err(ToolError::invalid(format!(
                    "`kind` must be head | branches | tags | all | collab, got {other:?}"
                ))),
            }
        }
        Kind::RepoResolve => {
            let (owner, repo) = repo_arg(args)?;
            let rev = str_arg(args, "rev").ok_or_else(|| ToolError::invalid("`rev` is required"))?;
            p(format!(
                "/{}/{}/api/resolve/{}",
                enc(&owner),
                enc(&repo),
                enc_path(rev)
            ))
            .await
        }
        Kind::RepoTree => {
            let (owner, repo) = repo_arg(args)?;
            let rev = str_arg(args, "rev").ok_or_else(|| ToolError::invalid("`rev` is required"))?;
            let mut spec = enc(rev);
            if let Some(path) = str_arg(args, "path") {
                spec.push('/');
                spec.push_str(&enc_path(path));
            }
            p(format!("/{}/{}/api/tree/{}", enc(&owner), enc(&repo), spec)).await
        }
        Kind::RepoBlob => {
            let (owner, repo) = repo_arg(args)?;
            let rev = str_arg(args, "rev").ok_or_else(|| ToolError::invalid("`rev` is required"))?;
            let path = str_arg(args, "path").ok_or_else(|| ToolError::invalid("`path` is required"))?;
            p(format!(
                "/{}/{}/api/blob/{}/{}",
                enc(&owner),
                enc(&repo),
                enc(rev),
                enc_path(path)
            ))
            .await
        }
        Kind::RepoCommits => {
            let (owner, repo) = repo_arg(args)?;
            let q = query(&[
                ("ref", str_arg(args, "ref").map(std::string::ToString::to_string)),
                ("path", str_arg(args, "path").map(std::string::ToString::to_string)),
                ("skip", u64_arg(args, "skip").map(|n| n.to_string())),
                ("n", u64_arg(args, "n").map(|n| n.to_string())),
            ]);
            p(format!("/{}/{}/api/commits{}", enc(&owner), enc(&repo), q)).await
        }
        Kind::RepoDiff => {
            let (owner, repo) = repo_arg(args)?;
            let from = str_arg(args, "from").ok_or_else(|| ToolError::invalid("`from` is required"))?;
            let to = str_arg(args, "to").ok_or_else(|| ToolError::invalid("`to` is required"))?;
            let q = query(&[
                ("from", Some(from.to_string())),
                ("to", Some(to.to_string())),
                ("format", str_arg(args, "format").map(std::string::ToString::to_string)),
            ]);
            p(format!("/{}/{}/api/diff{}", enc(&owner), enc(&repo), q)).await
        }
        Kind::CollabBoard => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/collab/board", enc(&owner), enc(&repo))).await
        }
        Kind::CollabThread => {
            let (owner, repo) = repo_arg(args)?;
            let id = str_arg(args, "id").ok_or_else(|| ToolError::invalid("`id` is required"))?;
            p(format!(
                "/{}/{}/api/collab/threads/{}",
                enc(&owner),
                enc(&repo),
                enc(id)
            ))
            .await
        }
        Kind::CollabReport => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/collab/report", enc(&owner), enc(&repo))).await
        }
        Kind::WalLs => {
            let (owner, repo) = repo_arg(args)?;
            let (_, handle) = super::resources::refs_level(st, &owner, &repo)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            // One window definition, shared with the `walgit://wal/…` resource:
            // `from` unset means the newest page, and the span is `WAL_PAGE`.
            let from = u64_arg(args, "from").unwrap_or(0);
            let (entries, head_seq) = super::resources::wal_page(&handle, from)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            let first = entries
                .first()
                .and_then(|e| e.get("seq"))
                .and_then(Value::as_u64)
                .unwrap_or(head_seq);
            let last = entries
                .last()
                .and_then(|e| e.get("seq"))
                .and_then(Value::as_u64)
                .unwrap_or(first);
            // An explicit `to` narrows the page to what the caller asked for.
            let (entries, last) = match u64_arg(args, "to") {
                Some(to) if to >= first && to < last => {
                    let kept = entries
                        .into_iter()
                        .filter(|e| e.get("seq").and_then(Value::as_u64).unwrap_or(0) <= to)
                        .collect::<Vec<_>>();
                    (kept, to)
                }
                _ => (entries, last),
            };
            Ok(json!({
                "repo": format!("{owner}/{repo}"),
                "head_seq": head_seq,
                "from": first,
                "to": last,
                "more": last < head_seq,
                "entries": entries,
            }))
        }
        Kind::CollabEntry => {
            let (owner, repo) = repo_arg(args)?;
            write_entry(st, headers, principal, &owner, &repo, args).await
        }
        Kind::CollabClaim => {
            let (owner, repo) = repo_arg(args)?;
            write_claim(st, headers, principal, session, &owner, &repo, args).await
        }
    }
}

/// `collab_entry`: build the entry, sign it as the caller's own actor, verify
/// the signature against the *registered* key, then publish it through the
/// ordinary collab API (which keeps `actor == caller`, the `done`-transition
/// gate, the policy and the size cap single-sourced).
async fn write_entry(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    owner: &str,
    repo: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    use walgit_wal::collab::{sign_entry, Entry, EntryRefs};
    let kind = str_arg(args, "kind").ok_or_else(|| ToolError::invalid("`kind` is required"))?;
    let id = str_arg(args, "id").ok_or_else(|| ToolError::invalid("`id` is required"))?;
    let actor = str_arg(args, "actor").ok_or_else(|| ToolError::invalid("`actor` is required"))?;
    let body = str_arg(args, "body").ok_or_else(|| ToolError::invalid("`body` is required"))?;
    let body: Value = serde_json::from_str(body)
        .map_err(|e| ToolError::invalid(format!("`body` must be a JSON object string: {e}")))?;
    if !body.is_object() {
        return Err(ToolError::invalid("`body` must be a JSON object string"));
    }
    require_own_actor(principal, actor, "write as")?;
    signing::ref_segment("actor", actor).map_err(ToolError::invalid)?;
    let key = signing::key_for(&st.cfg.mcp, actor)
        .await
        .map_err(ToolError::Execution)?;
    let key_path = signing::key_path(&st.cfg.mcp, actor).map_err(ToolError::Execution)?;

    let state = super::api::collab_state(st, headers, owner, repo)
        .await
        .map_err(|e| ToolError::from_server(&e))?;
    let thread_entries: Vec<&walgit_wal::collab::EntryRef> =
        state.entries.iter().filter(|e| e.entry.id == id).collect();
    let parent = match args.get("parent").and_then(Value::as_str) {
        // `parent: ""` is the explicit root marker; an absent `parent` chains
        // onto the thread head, the way the CLI's `collab entry` does.
        Some(p) => p.to_string(),
        None => walgit_wal::collab::thread(&thread_entries)
            .last()
            .map_or_else(String::new, |e| e.oid.clone()),
    };
    let refs = match (str_arg(args, "base"), str_arg(args, "head")) {
        (None, None) => None,
        (base, head) => Some(EntryRefs {
            base: base.map(std::string::ToString::to_string),
            head: head.map(std::string::ToString::to_string),
        }),
    };
    let mut entry = Entry {
        version: 1,
        kind: kind.to_string(),
        id: id.to_string(),
        actor: actor.to_string(),
        ts: chrono::Utc::now().timestamp(),
        parent,
        refs,
        body,
        sig: String::new(),
    };
    entry.sig = sign_entry(&mut entry, &key);
    // Verify against the registered key *before* publishing: a mismatched seed
    // would mint an entry that never verifies, and the log is append-only.
    signing::check_binding(
        actor,
        state.principals.get(actor).map(String::as_str),
        &entry,
        &key_path,
    )
    .map_err(ToolError::Execution)?;

    let entry_json =
        serde_json::to_value(&entry).map_err(|e| ToolError::Execution(e.to_string()))?;
    let published = Call::new(st, headers)
        .post(
            &format!("/{}/{}/api/collab/entries", enc(owner), enc(repo)),
            &json!({ "entry": entry_json }),
        )
        .await
        .map_err(ToolError::exec)?;
    Ok(json!({
        "repo": format!("{owner}/{repo}"),
        "id": id,
        "kind": kind,
        "actor": actor,
        "parent": entry.parent,
        "oid": published.get("oid").cloned().unwrap_or(Value::Null),
        "ref": published.get("ref").cloned().unwrap_or(Value::Null),
        "seq": published.get("seq").cloned().unwrap_or(Value::Null),
    }))
}

/// `collab_claim`: the three steps of D1 §8.4 — read, publish, re-read and let
/// only the deterministic winner proceed. Identical to `walgit collab claim`
/// (`crates/walgit-cli/src/collab_cmd.rs`), over the same aggregation core.
async fn write_claim(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &str,
    session: &str,
    owner: &str,
    repo: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    use walgit_wal::collab::{
        card_claim_governs, card_claim_view, card_status, non_card_kind, thread,
        CARD_CLAIM_TTL_MAX_SECS, CARD_CLAIM_TTL_MIN_SECS,
    };
    let id = str_arg(args, "id").ok_or_else(|| ToolError::invalid("`id` is required"))?;
    let actor = str_arg(args, "actor").ok_or_else(|| ToolError::invalid("`actor` is required"))?;
    require_own_actor(principal, actor, "claim as")?;
    signing::ref_segment("actor", actor).map_err(ToolError::invalid)?;
    let ttl = u64_arg(args, "ttl").unwrap_or(300);
    if !(CARD_CLAIM_TTL_MIN_SECS..=CARD_CLAIM_TTL_MAX_SECS).contains(&ttl) {
        return Err(ToolError::invalid(format!(
            "`ttl` {ttl} outside [{CARD_CLAIM_TTL_MIN_SECS}, {CARD_CLAIM_TTL_MAX_SECS}] seconds"
        )));
    }
    let session = str_arg(args, "session").unwrap_or(session).to_string();
    if session.len() > 128 {
        return Err(ToolError::invalid(format!(
            "`session` is {} bytes, over the 128-byte cap",
            session.len()
        )));
    }
    let key = signing::key_for(&st.cfg.mcp, actor)
        .await
        .map_err(ToolError::Execution)?;
    let key_path = signing::key_path(&st.cfg.mcp, actor).map_err(ToolError::Execution)?;

    // One read of the collab state → the card's status, its claim view, and the
    // key registered for `actor`.
    let read = |st: Arc<AppState>, headers: HeaderMap, id: String, actor: String| async move {
        let state = super::api::collab_state(&st, &headers, owner, repo)
            .await
            .map_err(|e| ToolError::from_server(&e))?;
        let thread_entries: Vec<&walgit_wal::collab::EntryRef> =
            state.entries.iter().filter(|e| e.entry.id == id).collect();
        if thread_entries.is_empty() {
            return Err(ToolError::Execution(format!("no collab thread {id}")));
        }
        if thread_entries
            .iter()
            .all(|e| non_card_kind(e.entry.kind.as_str()))
        {
            return Err(ToolError::Execution(format!(
                "thread {id} has no work-card entries — a CI run or bare claims are not claimable"
            )));
        }
        let ordered = thread(&thread_entries);
        let status = card_status(&ordered);
        let now = chrono::Utc::now().timestamp();
        let view = card_claim_view(&thread_entries, &state.principals, now);
        let registered = state.principals.get(&actor).cloned();
        Ok::<_, ToolError>((status, view, registered))
    };

    // Step 1: read the log as it is now, and refuse what can never verify.
    let (status, view, registered) = read(st.clone(), headers.clone(), id.to_string(), actor.to_string()).await?;
    signing::check_binding(actor, registered.as_deref(), &claim_entry(id, actor, &key, json!({})), &key_path)
        .map_err(ToolError::Execution)?;
    if !card_claim_governs(&status) {
        return Err(ToolError::Execution(format!(
            "card {id} is {status} — not claimable; only open/in-progress cards take claims"
        )));
    }
    if let Some(winner) = &view.winner {
        return Err(stand_down(id, winner));
    }

    // Step 2: publish. `parent = ""` — a claim is a new round of the race, not
    // a reply; two concurrent claims are two roots and both stay.
    let mut body = serde_json::Map::new();
    body.insert("ttl".into(), json!(ttl));
    for (field, value) in [
        ("worktree", str_arg(args, "worktree")),
        ("branch", str_arg(args, "branch")),
        ("work", str_arg(args, "work")),
        ("session", Some(session.as_str()).filter(|s| !s.is_empty())),
    ] {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            body.insert(field.into(), json!(value));
        }
    }
    let entry = claim_entry(id, actor, &key, Value::Object(body));
    let entry_json = serde_json::to_value(&entry).map_err(|e| ToolError::Execution(e.to_string()))?;
    let published = Call::new(st, headers)
        .post(
            &format!("/{}/{}/api/collab/entries", enc(owner), enc(repo)),
            &json!({ "entry": entry_json }),
        )
        .await
        .map_err(ToolError::exec)?;
    let oid = published
        .get("oid")
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
        .ok_or_else(|| ToolError::Execution("the publish answer carried no oid".to_string()))?;

    // Step 3: the convergence point. Re-read; only the deterministic winner may
    // start work.
    let (status, view, _) = read(st.clone(), headers.clone(), id.to_string(), actor.to_string()).await?;
    if !card_claim_governs(&status) {
        return Err(ToolError::Execution(format!(
            "card {id} became {status} while the claim was publishing — not claimable; the claim stays in the log"
        )));
    }
    match &view.winner {
        Some(w) if w.oid == oid => Ok(json!({
            "claimed": true,
            "repo": format!("{owner}/{repo}"),
            "id": id,
            "owner": w.actor,
            "claim": w.oid,
            "expires_at": w.expires_at(),
            "session": w.session,
            "resumed": false,
        })),
        Some(w) => Err(stand_down(id, w)),
        None if view.claims.iter().any(|c| c.oid == oid) => Err(ToolError::Execution(format!(
            "claim {oid} expired between publish and re-read; retry with a larger `ttl`"
        ))),
        None => Err(ToolError::Execution(format!(
            "claim {oid} was published but is not a live verified claim — check that principal \
             {actor} is registered and that `mcp.key_dir` holds the seed registered for it"
        ))),
    }
}

/// A `card_claim` root entry (`parent = ""`, `refs = None`), signed.
fn claim_entry(id: &str, actor: &str, key: &ed25519_dalek::SigningKey, body: Value) -> walgit_wal::collab::Entry {
    let mut entry = walgit_wal::collab::Entry {
        version: 1,
        kind: walgit_wal::collab::CARD_CLAIM_KIND.to_string(),
        id: id.to_string(),
        actor: actor.to_string(),
        ts: chrono::Utc::now().timestamp(),
        parent: String::new(),
        refs: None,
        body,
        sig: String::new(),
    };
    entry.sig = walgit_wal::collab::sign_entry(&mut entry, key);
    entry
}

/// The write is the caller's own act (the inner API enforces the same rule;
/// this fails before a signature is produced, with a sentence that says why).
///
/// There is deliberately no `"anon"` exemption: `auth.mode = "none"` already
/// names the caller `anon`, and a `tokens[]` row *called* `anon` must not become
/// a way to write as somebody else. The API's `actor == caller` is the gate.
fn require_own_actor(principal: &str, actor: &str, verb: &str) -> Result<(), ToolError> {
    if actor == principal {
        return Ok(());
    }
    Err(ToolError::Execution(format!(
        "refusing to {verb} {actor}: this MCP session is authenticated as {principal}. \
         An entry must be posted by its own principal"
    )))
}

/// A losing claim is `isError` content with the `stand-down:` marker the skill
/// documents, so an agent stops instead of working a card someone else holds.
fn stand_down(id: &str, winner: &walgit_wal::collab::CardClaim) -> ToolError {
    ToolError::Execution(format!(
        "stand-down: card {id} is held by {} until {} (claim {}); do not start work\n{}",
        winner.actor,
        winner.expires_at(),
        winner.oid,
        serde_json::to_string_pretty(&json!({
            "claimed": false,
            "id": id,
            "owner": winner.actor,
            "claim": winner.oid,
            "expires_at": winner.expires_at(),
        }))
        .unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_instances_advertise_no_write_tool() {
        let names: Vec<&str> = defs(false).into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"repo_tree"));
        assert!(!names.contains(&"collab_entry"));
        assert!(!names.contains(&"collab_claim"));
        let writable: Vec<&str> = defs(true).into_iter().map(|t| t.name).collect();
        assert!(writable.contains(&"collab_entry"));
        assert!(writable.contains(&"collab_claim"));
    }

    #[test]
    fn every_schema_declares_an_object_and_its_required_fields() {
        for t in defs(true) {
            assert_eq!(t.schema["type"], "object", "{}", t.name);
            let props = t.schema["properties"].as_object().unwrap();
            for req in t.schema["required"].as_array().unwrap() {
                let req = req.as_str().unwrap();
                assert!(props.contains_key(req), "{}: {req} not in properties", t.name);
            }
            // `repo` is the argument every repository tool names.
            if t.name != "repo_list" && t.name != "repo_owners" {
                assert!(props.contains_key("repo"), "{}: no repo argument", t.name);
            }
        }
    }

    #[test]
    fn arguments_are_held_to_the_schema() {
        let spec = defs(false)
            .into_iter()
            .find(|t| t.name == "repo_tree")
            .unwrap();
        assert!(validate_arguments(&spec, &json!({"repo": "o/r", "rev": "main"})).is_ok());
        // Unknown field.
        assert!(validate_arguments(&spec, &json!({"repo": "o/r", "rev": "main", "x": 1})).is_err());
        // Missing required.
        assert!(validate_arguments(&spec, &json!({"repo": "o/r"})).is_err());
        // Wrong type / null / empty / not an object.
        assert!(validate_arguments(&spec, &json!({"repo": 1, "rev": "main"})).is_err());
        assert!(validate_arguments(&spec, &json!({"repo": null, "rev": "main"})).is_err());
        assert!(validate_arguments(&spec, &json!({"repo": "  ", "rev": "main"})).is_err());
        assert!(validate_arguments(&spec, &json!([])).is_err());
    }

    #[test]
    fn repo_argument_splits_exactly_once() {
        assert_eq!(
            repo_arg(&json!({"repo": "acme/widgets"})).unwrap(),
            ("acme".to_string(), "widgets".to_string())
        );
        assert!(repo_arg(&json!({"repo": "widgets"})).is_err());
        assert!(repo_arg(&json!({"repo": "acme/we/x"})).is_err());
        assert!(repo_arg(&json!({"repo": "/x"})).is_err());
        assert!(repo_arg(&json!({})).is_err());
    }

    #[test]
    fn a_write_must_be_the_callers_own_actor() {
        assert!(require_own_actor("worker-1", "worker-1", "write as").is_ok());
        // `mode = "none"` presents every caller as `anon`, which is a name like
        // any other — it earns no exemption.
        assert!(require_own_actor("anon", "anon", "write as").is_ok());
        assert!(require_own_actor("anon", "coordinator", "write as").is_err());
        let e = require_own_actor("worker-1", "coordinator", "write as").unwrap_err();
        assert!(format!("{e:?}").contains("coordinator"), "{e:?}");
    }

    #[test]
    fn the_schema_table_and_the_dispatch_table_name_the_same_tools() {
        // `defs` carries descriptions and schemas; `Kind` carries names, tiers
        // and dispatch. This is what keeps the two from drifting apart.
        let from_defs: Vec<&str> = defs(true).into_iter().map(|t| t.name).collect();
        let from_kind: Vec<&str> = ALL.iter().map(|k| k.name()).collect();
        assert_eq!(from_defs, from_kind, "defs() and Kind disagree");
        for t in defs(true) {
            let kind = Kind::parse(t.name).expect("defs() named an unknown tool");
            assert_eq!(t.write, kind.write(), "{}", t.name);
        }
        // The read tier is exactly the tools that are not writes.
        let read: Vec<&str> = defs(false).into_iter().map(|t| t.name).collect();
        assert!(read.iter().all(|n| !Kind::parse(n).unwrap().write()));
        assert_eq!(read.len(), ALL.len() - 2);
    }
}
