//! The `/mcp` tool surface (D64).
//!
//! **The tools are walgit's own JSON API, called in-process.** Each one builds
//! the request the bundled UI would build, runs it through this process's
//! router ([`super::api::Call`]), and returns the answer. Nothing is
//! reimplemented, so placement (D30's 503 for a repo this host does not serve),
//! freshness (every read revalidates), authz (`actor == caller`) and the
//! response shapes cannot drift from the HTTP contract.
//!
//! `repo_refs {repo, kind}`, `repo_tree {repo, rev, path}`, and so on, with
//! `repo` as `"<owner>/<repo>"`. Every tool names its repository (this lane
//! serves a host, not one checkout) and reads through the store rather than a
//! local clone.
//!
//! **The surface is read-only.** The D1 write tools moved to `wg mcp` (D70),
//! which takes entries the caller already signed; this process never holds a
//! collaborator's key (D64).

use std::sync::Arc;

use axum::http::HeaderMap;
use serde_json::{json, Value};

use super::api::{enc, enc_path, query, ApiError as CallError, Call};
use crate::AppState;

/// A registered tool.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
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
    /// that names the fix — and the status travels appended to it: an
    /// `isError` result has no status field, and `… (503)` is the part that
    /// tells the model the next attempt is not futile (cc-ai-store-503).
    fn exec(e: &CallError) -> Self {
        ToolError::Execution(format!("{} ({})", e.message, e.status.as_u16()))
    }
}

/// The descriptions are load-bearing: a model reads them instead of the docs,
/// so they carry the rules a wrong call would violate (bytes never travel over
/// MCP; a card's ownership is a `status` entry naming an owner, or a live
/// `card_claim`; …).
const REPO_ARG: &str = "repository as \"<owner>/<repo>\"";

/// Every tool this instance registers — one per endpoint of the host's JSON
/// API, plus the WAL read that has no endpoint.
/// The advertised surface. Every `name` comes from [`Kind::name`], so the schema
/// table and the dispatch table cannot disagree about what a tool is called —
/// in either direction (a name here with no dispatch arm, or the reverse).
pub fn defs() -> Vec<Tool> {
    let out = vec![
        Tool {
            name: Kind::RepoList.name(),
            description: "List the repositories on this host.",
                schema: schema(&json!({}), &json!([])),
        },
        Tool {
            name: Kind::RepoOwners.name(),
            description: "List owners, or one owner's repositories.",
                schema: schema(&json!({"owner": {"type": "string"}}), &json!([])),
        },
        Tool {
            name: Kind::RepoRefs.name(),
            description: "Refs of a repository: the `head` summary by default, or one namespace \
                          (`branches` | `tags` | `all` | `collab`, paged by `after`).",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "kind": {"type": "string", "description": "head (default) | branches | tags | all | collab"},
                    "after": {"type": "string", "description": "page cursor: the last ref name seen"},
                }), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoResolve.name(),
            description: "Resolve a revision (branch, tag, partial sha) to an object id.",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision to resolve"},
                }), &json!(["repo", "rev"])),
        },
        Tool {
            name: Kind::RepoTree.name(),
            description: "One directory of the tree at a revision.",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision (branch, tag or sha)"},
                    "path": {"type": "string", "description": "directory path inside the tree (default: root)"},
                }), &json!(["repo", "rev"])),
        },
        Tool {
            name: Kind::RepoBlob.name(),
            description: "One file's blob envelope (text contents; binary files report `binary: true`). \
                          Bytes do not travel over MCP — clone/fetch those with git and the bundle-uri recipes.",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "rev": {"type": "string", "description": "revision (branch, tag or sha)"},
                    "path": {"type": "string", "description": "file path"},
                }), &json!(["repo", "rev", "path"])),
        },
        Tool {
            name: Kind::RepoCommits.name(),
            description: "Recent commits of a ref (optionally for one path), newest first.",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "ref": {"type": "string", "description": "ref or revision (default HEAD); `rev` is an accepted alias"},
                    "path": {"type": "string", "description": "restrict history to this path"},
                    "skip": {"type": "integer", "description": "commits to skip (paging)"},
                    "n": {"type": "integer", "description": "page size, 1..=200 (default 35)"},
                }), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoCommit.name(),
            description: "One commit in full: parents, author/committer, message and its diff stat.",
            schema: schema(&json!({
                "repo": {"type": "string", "description": REPO_ARG},
                "sha": {"type": "string", "description": "a full or abbreviated commit id"},
            }), &json!(["repo", "sha"])),
        },
        Tool {
            name: Kind::RepoDiff.name(),
            description: "Diff between two revisions (patch by default).",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "from": {"type": "string", "description": "base revision"},
                    "to": {"type": "string", "description": "target revision"},
                    "format": {"type": "string", "description": "patch (default) | stat | name-status"},
                }), &json!(["repo", "from", "to"])),
        },
        Tool {
            name: Kind::RepoMergeBase.name(),
            description: "The merge base of two revisions (empty `base` when they share no history).",
            schema: schema(&json!({
                "repo": {"type": "string", "description": REPO_ARG},
                "from": {"type": "string", "description": "first revision (branch, tag or sha)"},
                "to": {"type": "string", "description": "second revision"},
            }), &json!(["repo", "from", "to"])),
        },
        Tool {
            name: Kind::RepoOverview.name(),
            description: "One repository at a glance: default branch, tip commit, ref counts, size and \
                          the maintainer's last units — the page a first look starts from.",
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoBlame.name(),
            description: "Per-line blame of one file at one revision (commit, author, time per line).",
            schema: schema(&json!({
                "repo": {"type": "string", "description": REPO_ARG},
                "rev": {"type": "string", "description": "revision"},
                "path": {"type": "string", "description": "path inside the tree"},
            }), &json!(["repo", "rev", "path"])),
        },
        Tool {
            name: Kind::RepoTasks.name(),
            description: "Long-running work of this host for this repository (id, kind, state, last \
                          packets). A task not listed here is not running on this instance; \
                          `ops` starts one.",
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoOps.name(),
            description: "The maintenance operations this host can start for the repository \
                          (`fsck`, `gc`, `repair`, `compact`, `rev-index`, `checkpoint`, `collab-fold`, …), \
                          each with its parameters and whether it mutates. Listing only: starting one is \
                          an operator act (`walgit repo op-start`), not an agent tool.",
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoPolicy.name(),
            description: "The repository's push policy as stored (`docs/POLICY.md`), or `{}` when \
                          nothing is frozen. Read-only: writing it requires an admin credential.",
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: Kind::RepoSettings.name(),
            description: "The **effective** per-repo settings (host config ⊕ env ⊕ the repo's own \
                          SETTINGS entry), restricted to the sections a repository may set \
                          (`[bundles]`, `[maintenance]`, `[compaction]`, `[upstream]`). Read-only.",
            schema: schema(&json!({"repo": {"type": "string", "description": REPO_ARG}}), &json!(["repo"])),
        },
        Tool {
            name: Kind::WalLs.name(),
            description: "WAL entries of a repository (the append-only source of truth), seq-ordered. \
                          `head_seq` is what a resource subscription's version reports.",
                schema: schema(&json!({
                    "repo": {"type": "string", "description": REPO_ARG},
                    "from": {"type": "integer", "description": "first seq (default: the newest page)"},
                    "to": {"type": "integer", "description": "last seq (default: from + 199)"},
                }), &json!(["repo"])),
        },
    ];
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
    RepoCommit,
    RepoDiff,
    RepoMergeBase,
    RepoOverview,
    RepoBlame,
    RepoTasks,
    RepoOps,
    RepoPolicy,
    RepoSettings,
    WalLs,
}

/// Every tool, in advertisement order.
const ALL: [Kind; 17] = [
    Kind::RepoList,
    Kind::RepoOwners,
    Kind::RepoRefs,
    Kind::RepoResolve,
    Kind::RepoTree,
    Kind::RepoBlob,
    Kind::RepoCommits,
    Kind::RepoCommit,
    Kind::RepoDiff,
    Kind::RepoMergeBase,
    Kind::RepoOverview,
    Kind::RepoBlame,
    Kind::RepoTasks,
    Kind::RepoOps,
    Kind::RepoPolicy,
    Kind::RepoSettings,
    Kind::WalLs,
];

impl Kind {
    /// Every tool — the one list the schema table, the dispatch match and the
    /// tests all read.
    const ALL: [Kind; 17] = ALL;

    fn name(self) -> &'static str {
        match self {
            Self::RepoList => "repo_list",
            Self::RepoOwners => "repo_owners",
            Self::RepoRefs => "repo_refs",
            Self::RepoResolve => "repo_resolve",
            Self::RepoTree => "repo_tree",
            Self::RepoBlob => "repo_blob",
            Self::RepoCommits => "repo_commits",
            Self::RepoCommit => "repo_commit",
            Self::RepoDiff => "repo_diff",
            Self::RepoMergeBase => "repo_merge_base",
            Self::RepoOverview => "repo_overview",
            Self::RepoBlame => "repo_blame",
            Self::RepoTasks => "repo_tasks",
            Self::RepoOps => "repo_ops",
            Self::RepoPolicy => "repo_policy",
            Self::RepoSettings => "repo_settings",
            Self::WalLs => "wal_ls",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
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
                "value"
            )));
        }
        // `type` is either a name or a list of them (JSON Schema allows the
        // list form; no current tool needs it, but the validator must not be
        // a weaker parser than the schema it advertises).
        let mut declared: Vec<&str> = Vec::new();
        match field.get("type") {
            Some(Value::String(one)) => declared.push(one),
            Some(Value::Array(many)) => declared.extend(many.iter().filter_map(Value::as_str)),
            _ => {}
        }
        let ok = declared.is_empty()
            || declared.contains(&match value {
                Value::String(_) => "string",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
                Value::Null => "null",
            });
        // `integer` is the one name a JSON value does not carry itself.
        let ok = ok || (declared.contains(&"integer") && value.as_u64().is_some());
        if !ok {
            return Err(ToolError::invalid(format!(
                "`{key}` must be a {}",
                if declared.is_empty() { "value".to_string() } else { declared.join(" or ") }
            )));
        }
        // Empty is meaningless for every argument here: the entry itself
        // travels as `entry`, and its `parent: ""` root marker is inside it.
        if value.as_str().is_some_and(|v| v.trim().is_empty()) {
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
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    let Some(spec) = defs().into_iter().find(|t| t.name == name) else {
        return Err(ToolError::invalid(format!("unknown tool: {name}")));
    };
    validate_arguments(&spec, args)?;
    run(st, headers, principal, name, args).await
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
    name: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    let call = Call::new(st, headers, principal);
    let p = |path: String| async move {
        call.get(&path).await.map_err(|e| ToolError::exec(&e))
    };
    let kind = Kind::parse(name)
        .ok_or_else(|| ToolError::invalid(format!("unknown tool `{name}`")))?;
    match kind {
        Kind::RepoList => {
            let mut repos = st
                .registry
                .list()
                .await
                .map_err(|e| {
                    // Same shape as `ToolError::exec`: sentence + status, so a
                    // transient store failure reads as `… (503)` here too.
                    ToolError::Execution(format!(
                        "listing repositories: {}",
                        crate::error::ApiError::from(&e).message_with_status()
                    ))
                })?;
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
            // `rev` is accepted as the alias for `ref`: every other repo tool
            // names the revision `rev`, and an agent that just used `repo_tree`
            // should not have to learn a second word for the same thing.
            let rev = str_arg(args, "ref").or_else(|| str_arg(args, "rev"));
            let q = query(&[
                ("ref", rev.map(std::string::ToString::to_string)),
                ("path", str_arg(args, "path").map(std::string::ToString::to_string)),
                ("skip", u64_arg(args, "skip").map(|n| n.to_string())),
                ("n", u64_arg(args, "n").map(|n| n.to_string())),
            ]);
            p(format!("/{}/{}/api/commits{}", enc(&owner), enc(&repo), q)).await
        }
        Kind::RepoCommit => {
            let (owner, repo) = repo_arg(args)?;
            let sha = str_arg(args, "sha").ok_or_else(|| ToolError::invalid("`sha` is required"))?;
            p(format!(
                "/{}/{}/api/commit/{}",
                enc(&owner),
                enc(&repo),
                enc(sha)
            ))
            .await
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
        Kind::RepoMergeBase => {
            let (owner, repo) = repo_arg(args)?;
            let from = str_arg(args, "from").ok_or_else(|| ToolError::invalid("`from` is required"))?;
            let to = str_arg(args, "to").ok_or_else(|| ToolError::invalid("`to` is required"))?;
            let q = query(&[
                ("from", Some(from.to_string())),
                ("to", Some(to.to_string())),
            ]);
            p(format!("/{}/{}/api/merge-base{}", enc(&owner), enc(&repo), q)).await
        }
        Kind::RepoOverview => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/overview", enc(&owner), enc(&repo))).await
        }
        Kind::RepoBlame => {
            let (owner, repo) = repo_arg(args)?;
            let rev = str_arg(args, "rev").ok_or_else(|| ToolError::invalid("`rev` is required"))?;
            let path = str_arg(args, "path").ok_or_else(|| ToolError::invalid("`path` is required"))?;
            p(format!(
                "/{}/{}/api/blame/{}/{}",
                enc(&owner),
                enc(&repo),
                enc(rev),
                enc_path(path)
            ))
            .await
        }
        Kind::RepoTasks => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/tasks", enc(&owner), enc(&repo))).await
        }
        Kind::RepoOps => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/ops", enc(&owner), enc(&repo))).await
        }
        Kind::RepoPolicy => {
            let (owner, repo) = repo_arg(args)?;
            p(format!("/{}/{}/api/policy", enc(&owner), enc(&repo))).await
        }
        Kind::RepoSettings => {
            let (owner, repo) = repo_arg(args)?;
            // The effective document is TOML, not JSON: wrap it as one field
            // instead of letting a JSON decoder decide it is an error.
            let toml = Call::new(st, headers, principal)
                .get_text(&format!(
                    "/{}/{}/api/settings/effective",
                    enc(&owner),
                    enc(&repo)
                ))
                .await
                .map_err(|e| ToolError::exec(&e))?;
            Ok(json!({ "repo": format!("{owner}/{repo}"), "toml": toml }))
        }
        Kind::WalLs => {
            let (owner, repo) = repo_arg(args)?;
            let (_, handle) = super::resources::refs_level(st, &owner, &repo)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            // One window definition, shared with the `walgit://wal/…` resource.
            // `from` unset (or 0) means the newest page; `to` is a real bound —
            // answering past it would hand a model log lines it excluded.
            let from = u64_arg(args, "from").unwrap_or(0);
            let to = u64_arg(args, "to");
            let page = super::resources::wal_page(&handle, from, to)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            Ok(page.to_json(&format!("{owner}/{repo}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_declares_an_object_and_its_required_fields() {
        for t in defs() {
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
        let spec = defs()
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
    fn the_schema_table_and_the_dispatch_table_name_the_same_tools() {
        // `defs` carries descriptions and schemas; `Kind` carries names and
        // dispatch. This is what keeps the two from drifting apart.
        let from_defs: Vec<&str> = defs().into_iter().map(|t| t.name).collect();
        let from_kind: Vec<&str> = ALL.iter().map(|k| k.name()).collect();
        assert_eq!(from_defs, from_kind, "defs() and Kind disagree");
        for name in &from_defs {
            assert!(Kind::parse(name).is_some(), "defs() named an unknown tool: {name}");
        }
    }
}
