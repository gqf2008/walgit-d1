//! `walgit mcp` — a **client-side** MCP adapter (Model Context Protocol) over
//! stdio, so an agent host can discover and call walgit as tools instead of
//! shelling out or hand-rolling HTTP.
//!
//! Two decisions shape this file:
//!
//! * **It lives in the client, not the server.** MCP is a host-spawned process
//!   in practice; a server-side endpoint would be a second API contract to keep
//!   in sync with `web/API.md`, the CLI and the SDK. The adapter talks to any
//!   configured host with the same `--config`/token the CLI already uses.
//! * **Tools *are* the existing CLI.** Each call runs this same binary's
//!   subcommand and returns its output, so there is exactly one implementation of
//!   every operation — no schema drift, and anything the CLI learns the adapter
//!   learns. (If the CLI ever returns structured values, swapping the fork for
//!   an in-process call is mechanical.)
//!
//! Destructive operations are **not reachable**: `gc`/`reclaim`, `compact`,
//! `import`, per-repo settings/policy writes, ref deletion and force-push are
//! absent from the table AND from the dispatch match, so neither `tools/list`
//! nor a hand-written `tools/call` can reach them. Writing (a signed collab
//! entry) is opt-in via `--allow-write`.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Latest revision this adapter implements; the client's requested one is
/// echoed when we know it (the spec lets the server choose and the client
/// decide).
const PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

#[derive(Debug, Clone)]
pub struct Options {
    /// Local checkout the `collab_*` tools read (and write, with `--allow-write`).
    pub repo: PathBuf,
    /// The CLI config handed to every child invocation.
    pub config: PathBuf,
    /// Register the writing tools (`collab_entry`).
    pub allow_write: bool,
    /// Signing key for `collab_entry` (an Ed25519 seed file).
    pub key: Option<PathBuf>,
    /// Remote the entry is pushed to (default `origin`).
    pub remote: String,
}

/// One tool: what the host sees, plus the flag that keeps it out of the
/// read-only surface.
struct Tool {
    name: &'static str,
    description: &'static str,
    write: bool,
    schema: Value,
}

fn tool_defs() -> Vec<Tool> {
    let repo_arg = json!({"type": "string", "description": "`owner/name` on the host"});
    let schema = |props: Value, required: Value| json!({"type": "object", "properties": props, "required": required, "additionalProperties": false});
    vec![
        Tool {
            name: "repo_list",
            description: "List every repository on this host.",
            write: false,
            schema: schema(json!({}), json!([])),
        },
        Tool {
            name: "repo_owners",
            description: "List owners, or one owner's repositories.",
            write: false,
            schema: schema(json!({"owner": {"type": "string"}}), json!([])),
        },
        Tool {
            name: "repo_refs",
            description: "Branches/tags/refs of a repository over HTTP (head summary, or one namespace).",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "kind": {"type": "string", "description": "branches | tags | all | collab"}}),
                json!(["repo"]),
            ),
        },
        Tool {
            name: "repo_resolve",
            description: "Resolve a revision (branch, tag, partial sha) to an object id.",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "rev": {"type": "string"}}),
                json!(["repo", "rev"]),
            ),
        },
        Tool {
            name: "repo_tree",
            description: "One directory of the tree at a revision.",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "rev": {"type": "string"}, "path": {"type": "string"}}),
                json!(["repo", "rev"]),
            ),
        },
        Tool {
            name: "repo_blob",
            description: "One file's blob envelope (text contents; binary files report `binary: true`). Bytes do \
                 not travel over MCP — clone/fetch those with git and the bundle-uri recipes.",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "rev": {"type": "string"}, "path": {"type": "string"}}),
                json!(["repo", "rev", "path"]),
            ),
        },
        Tool {
            name: "repo_commits",
            description: "Recent commits of a ref (optionally for one path), newest first.",
            write: false,
            schema: schema(
                json!({
                    "repo": repo_arg,
                    "ref": {"type": "string"},
                    "path": {"type": "string"},
                    "n": {"type": "integer", "description": "page size"},
                    "skip": {"type": "integer"}
                }),
                json!(["repo"]),
            ),
        },
        Tool {
            name: "repo_diff",
            description: "Diff between two revisions (patch by default).",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "from": {"type": "string"}, "to": {"type": "string"}, "format": {"type": "string"}}),
                json!(["repo", "from", "to"]),
            ),
        },
        Tool {
            name: "collab_ls",
            description: "Thread ids in the local checkout's collab inbox.",
            write: false,
            schema: schema(json!({}), json!([])),
        },
        Tool {
            name: "collab_thread",
            description: "One collab thread: parent-ordered entries with per-entry signature verification.",
            write: false,
            schema: schema(json!({"id": {"type": "string"}}), json!(["id"])),
        },
        Tool {
            name: "collab_pr",
            description: "Aggregated PR view + merge-rule evaluation for one thread.",
            write: false,
            schema: schema(json!({"id": {"type": "string"}}), json!(["id"])),
        },
        Tool {
            name: "collab_board",
            description: "The work-unit board (threads projected under `.walgit/board.toml`). Read-only: \
                 moving a card is a signed `status` entry.",
            write: false,
            schema: schema(json!({}), json!([])),
        },
        Tool {
            name: "collab_report",
            description: "Read-only dashboard: threads, PR status, verification health, activity.",
            write: false,
            schema: schema(json!({}), json!([])),
        },
        Tool {
            name: "ci_status",
            description: "Every CI run in the checkout's collab log, aggregated.",
            write: false,
            schema: schema(json!({}), json!([])),
        },
        Tool {
            name: "wal_ls",
            description: "WAL entries of a repository (the append-only source of truth).",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "from": {"type": "integer"}, "to": {"type": "integer"}}),
                json!(["repo"]),
            ),
        },
        Tool {
            name: "collab_entry",
            description: "Append one signed collab entry (issue/comment/patch/review/merge_result/status) and its \
                 oid. Rules the board depends on: a thread needs an `issue` AND a `status` naming an \
                 `owner`, otherwise it projects as an unowned open card; `patch` carries base/head refs; \
                 only verified `approve` reviews count for the merge rule.",
            write: true,
            schema: schema(
                json!({
                    "kind": {"type": "string", "enum": ["issue", "comment", "patch", "review", "merge_result", "status"]},
                    "id": {"type": "string", "description": "thread id, shared by every entry"},
                    "actor": {"type": "string"},
                    "parent": {"type": "string", "description": "previous entry's oid (\"\" for a root)"},
                    "body": {"type": "string", "description": "the entry body as a JSON object string"},
                    "base": {"type": "string", "description": "patch only"},
                    "head": {"type": "string", "description": "patch only"}
                }),
                json!(["kind", "id", "actor", "body"]),
            ),
        },
    ]
}

/// Run the stdio server: newline-delimited JSON-RPC in, newline-delimited
/// responses out. A notification (no `id`) produces no reply.
pub async fn run(opts: Options) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line, &opts).await {
            out.write_all(reply.as_bytes()).await?;
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
    }
    Ok(())
}

/// One request line → one reply line (`None` for notifications). Split out from
/// the transport so the protocol is testable without spawning anything.
async fn handle_line(line: &str, opts: &Options) -> Option<String> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_reply(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            ));
        }
    };
    // Envelope first: a request that is not JSON-RPC 2.0 is `-32600`, not "an
    // unknown method" (which would send the caller hunting for a typo).
    if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: `jsonrpc` must be \"2.0\"",
        ));
    }
    let id = req.get("id").cloned();
    if let Some(i) = &id
        && !(i.is_string() || i.is_number() || i.is_null())
    {
        return Some(error_reply(
            Value::Null,
            -32600,
            "invalid request: `id` must be a string or a number",
        ));
    }
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        // A notification that is malformed is still a notification: no reply.
        id.as_ref()?;
        return Some(error_reply(
            id.unwrap_or(Value::Null),
            -32600,
            "invalid request: `method` must be a string",
        ));
    };
    // Notifications (no id) never get a reply — including `notifications/*`.
    id.as_ref()?;
    let id = id.unwrap_or(Value::Null);
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = if KNOWN_PROTOCOLS.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSION
            };
            Some(result_reply(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    // `instructions` sits next to `serverInfo`, not inside it —
                    // that is where hosts read it.
                    "instructions":
                        "Tools are the walgit CLI (one implementation, no second schema). Read-only \
                         unless the server was started with `--allow-write`; destructive operations \
                         are not exposed at all.",
                    "serverInfo": {"name": "walgit", "version": env!("CARGO_PKG_VERSION")}
                }),
            ))
        }
        "ping" => Some(result_reply(id, json!({}))),
        "tools/list" => {
            let tools: Vec<Value> = tool_defs()
                .iter()
                .filter(|t| opts.allow_write || !t.write)
                .map(|t| {
                    json!({"name": t.name, "description": t.description, "inputSchema": t.schema})
                })
                .collect();
            Some(result_reply(id, json!({"tools": tools})))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_reply(
                    id,
                    -32602,
                    "invalid params: `name` must be a string",
                ));
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(match call_tool(name, &args, opts).await {
                Ok(text) => result_reply(id, json!({"content": [{"type": "text", "text": text}]})),
                // A request the model can repair is a protocol error; a tool that
                // ran and failed is content with `isError`.
                Err(ToolError::InvalidParams(m)) => error_reply(id, -32602, &m),
                Err(ToolError::Execution(m)) => result_reply(
                    id,
                    json!({"content": [{"type": "text", "text": m}], "isError": true}),
                ),
            })
        }
        other => Some(error_reply(
            id,
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

/// Build the child argv for `name`. Unknown names — including every destructive
/// operation, which has no arm here — are refused before anything runs.
/// Why a `tools/call` did not produce a result.
#[derive(Debug)]
enum ToolError {
    /// The *request* is wrong — unknown tool, bad or missing arguments, a
    /// writing tool without `--allow-write`. JSON-RPC `-32602`, because the
    /// model asked for something that does not exist (and can fix it), not
    /// because a tool ran and failed.
    InvalidParams(String),
    /// The tool ran and failed: reported as `isError: true` content.
    Execution(String),
}

/// How long a single tool call may take, and how much output it may produce.
/// Both exist because the *host session* is the thing at risk: a wedged child
/// used to block the whole MCP loop with no way out.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const TOOL_OUTPUT_MAX: usize = 1024 * 1024;

async fn call_tool(name: &str, args: &Value, opts: &Options) -> Result<String, ToolError> {
    let Some(spec) = tool_defs().into_iter().find(|t| t.name == name) else {
        return Err(ToolError::InvalidParams(format!("unknown tool: {name}")));
    };
    if spec.write && !opts.allow_write {
        return Err(ToolError::InvalidParams(format!(
            "`{name}` writes; start the server with `--allow-write` to register it"
        )));
    }
    let argv = argv_for(name, args, opts)?;
    run_child(&argv, &opts.config).await
}

/// Run this same binary with `argv`, bounded in both time and output.
///
/// `kill_on_drop` matters as much as the timeout: when the deadline fires the
/// child is killed rather than left holding a pipe, and a child that floods
/// stdout is stopped by the capped reads filling its pipe (the deadline then
/// fires instead of memory ballooning).
async fn run_child(argv: &[String], config: &std::path::Path) -> Result<String, ToolError> {
    use tokio::io::AsyncReadExt;

    let exe = std::env::current_exe()
        .map_err(|e| ToolError::Execution(format!("locating walgit: {e}")))?;
    let mut child = tokio::process::Command::new(&exe)
        .arg("--config")
        .arg(config)
        .args(argv)
        // Never the MCP stream: the child's stdin must not be able to consume it.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::Execution(format!("running walgit {}: {e}", argv.join(" "))))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = {
        let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ToolError::Execution(
                "child stdout/stderr were not captured".to_string(),
            ));
        };
        let read = async {
            let mut out = out.take(TOOL_OUTPUT_MAX as u64 + 1);
            let mut err = err.take(TOOL_OUTPUT_MAX as u64 + 1);
            let _ = tokio::join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr),);
            child.wait().await
        };
        match tokio::time::timeout(TOOL_TIMEOUT, read).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                return Err(ToolError::Execution(format!(
                    "walgit {}: {e}",
                    argv.join(" ")
                )));
            }
            Err(_) => {
                return Err(ToolError::Execution(format!(
                    "walgit {} timed out after {}s",
                    argv.join(" "),
                    TOOL_TIMEOUT.as_secs()
                )));
            }
        }
    };
    if stdout.len() > TOOL_OUTPUT_MAX || stderr.len() > TOOL_OUTPUT_MAX {
        return Err(ToolError::Execution(format!(
            "walgit {} produced more than {} KiB — run the CLI directly for bulk output",
            argv.join(" "),
            TOOL_OUTPUT_MAX / 1024
        )));
    }
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    if status.success() {
        Ok(stdout)
    } else {
        Err(ToolError::Execution(format!(
            "walgit {} failed ({status}): {}",
            argv.join(" "),
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        )))
    }
}

/// Build the child argv.
///
/// Two rules keep a caller-supplied string from becoming *syntax*:
///
/// * every flag value is joined with `=`, so `--repo=--url=…` is a value and can
///   never be read as the next option (this is what let a tool argument reach
///   clap's own `--url`/`--config`/`--token`);
/// * caller positionals go **after `--`**, which ends option parsing for that
///   subcommand, so a `repo` of `--help` stays a repository name.
///
/// The subcommand words themselves are ours, never the caller's, which is why
/// no argument can reach an operation that has no arm in the table below.
fn argv_for(name: &str, args: &Value, opts: &Options) -> Result<Vec<String>, ToolError> {
    let req = |key: &str| -> Result<String, ToolError> {
        match args.get(key).and_then(Value::as_str) {
            Some(v) if !v.trim().is_empty() => Ok(v.to_string()),
            _ => Err(ToolError::InvalidParams(format!(
                "`{key}` is required and must be a non-empty string"
            ))),
        }
    };
    let opt =
        |key: &str| -> Option<String> { args.get(key).and_then(Value::as_str).map(str::to_string) };
    let one_of = |key: &str, allowed: &[&str]| -> Result<Option<String>, ToolError> {
        match opt(key) {
            None => Ok(None),
            Some(v) if allowed.contains(&v.as_str()) => Ok(Some(v)),
            Some(v) => Err(ToolError::InvalidParams(format!(
                "`{key}` must be one of {allowed:?}, got `{v}`"
            ))),
        }
    };
    let count = |key: &str, max: u64| -> Result<Option<u64>, ToolError> {
        match args.get(key) {
            None => Ok(None),
            Some(v) => v.as_u64().filter(|n| *n <= max).map(Some).ok_or_else(|| {
                ToolError::InvalidParams(format!("`{key}` must be an integer <= {max}"))
            }),
        }
    };
    // `--flag=value`: the value can start with `-` and still be a value.
    let flag = |name: &str, value: &str| format!("{name}={value}");
    let repo_path = opts.repo.display().to_string();

    let argv: Vec<String> = match name {
        "repo_list" => vec!["repo".into(), "list".into()],
        "repo_owners" => {
            let mut v = vec!["repo".into(), "owners".into()];
            if let Ok(owner) = req("owner") {
                v.push("--".into());
                v.push(owner);
            }
            v
        }
        "repo_refs" => {
            let mut v = vec!["repo".into(), "refs".into(), "--".into(), req("repo")?];
            if let Some(kind) = one_of("kind", &["branches", "tags", "all", "collab"])? {
                v.push(kind);
            }
            v
        }
        "repo_resolve" => vec![
            "repo".into(),
            "resolve".into(),
            "--".into(),
            req("repo")?,
            req("rev")?,
        ],
        "repo_tree" => {
            let mut v = vec![
                "repo".into(),
                "tree".into(),
                "--".into(),
                req("repo")?,
                req("rev")?,
            ];
            if let Some(path) = opt("path") {
                v.push(path);
            }
            v
        }
        "repo_blob" => vec![
            "repo".into(),
            "blob".into(),
            "--".into(),
            req("repo")?,
            req("rev")?,
            req("path")?,
        ],
        "repo_commits" => {
            let mut v = vec!["repo".into(), "commits".into()];
            if let Some(r) = opt("ref") {
                v.push(flag("--ref", &r));
            }
            if let Some(n) = count("n", 1000)? {
                v.push(flag("--n", &n.to_string()));
            }
            if let Some(skip) = count("skip", 1_000_000)? {
                v.push(flag("--skip", &skip.to_string()));
            }
            if let Some(p) = opt("path") {
                v.push(flag("--path", &p));
            }
            v.push("--".into());
            v.push(req("repo")?);
            v
        }
        "repo_diff" => {
            let mut v = vec!["repo".into(), "diff".into()];
            if let Some(f) = opt("format") {
                v.push(flag("--format", &f));
            }
            v.push("--".into());
            v.push(req("repo")?);
            v.push(req("from")?);
            v.push(req("to")?);
            v
        }
        "collab_ls" => vec!["collab".into(), "ls".into(), flag("--repo", &repo_path)],
        "collab_thread" => vec![
            "collab".into(),
            "thread".into(),
            flag("--repo", &repo_path),
            "--".into(),
            req("id")?,
        ],
        "collab_pr" => vec![
            "collab".into(),
            "pr".into(),
            flag("--repo", &repo_path),
            "--".into(),
            req("id")?,
        ],
        "collab_board" => vec!["collab".into(), "board".into(), flag("--repo", &repo_path)],
        "collab_report" => vec!["collab".into(), "report".into(), flag("--repo", &repo_path)],
        "ci_status" => vec!["ci".into(), "status".into(), flag("--repo", &repo_path)],
        "wal_ls" => {
            let mut v = vec!["wal".into(), "ls".into()];
            if let Some(from) = count("from", u64::MAX)? {
                v.push(flag("--from", &from.to_string()));
            }
            if let Some(to) = count("to", u64::MAX)? {
                v.push(flag("--to", &to.to_string()));
            }
            v.push("--".into());
            v.push(req("repo")?);
            v
        }
        "collab_entry" => {
            let key = opts.key.as_ref().ok_or_else(|| {
                ToolError::InvalidParams(
                    "`collab_entry` needs `--key <ed25519 seed file>` on the server".into(),
                )
            })?;
            let kind = one_of(
                "kind",
                &[
                    "issue",
                    "comment",
                    "patch",
                    "review",
                    "merge_result",
                    "status",
                ],
            )?
            .ok_or_else(|| ToolError::InvalidParams("`kind` is required".into()))?;
            let mut v = vec![
                "collab".into(),
                "entry".into(),
                flag("--repo", &repo_path),
                flag("--kind", &kind),
                flag("--id", &req("id")?),
                flag("--actor", &req("actor")?),
                flag("--body", &req("body")?),
                flag("--key", &key.display().to_string()),
                flag("--push", &opts.remote),
            ];
            if let Some(parent) = opt("parent") {
                v.push(flag("--parent", &parent));
            }
            if let Some(base) = opt("base") {
                v.push(flag("--base", &base));
            }
            if let Some(head) = opt("head") {
                v.push(flag("--head", &head));
            }
            v
        }
        other => return Err(ToolError::InvalidParams(format!("unknown tool: {other}"))),
    };
    Ok(argv)
}

fn result_reply(id: Value, result: Value) -> String {
    // Built by insertion (not `json!`) so the values are consumed here.
    let mut body = serde_json::Map::new();
    body.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    body.insert("id".to_string(), id);
    body.insert("result".to_string(), result);
    Value::Object(body).to_string()
}

fn error_reply(id: Value, code: i64, message: &str) -> String {
    let mut error = serde_json::Map::new();
    error.insert("code".to_string(), Value::from(code));
    error.insert("message".to_string(), Value::String(message.to_string()));
    let mut body = serde_json::Map::new();
    body.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    body.insert("id".to_string(), id);
    body.insert("error".to_string(), Value::Object(error));
    Value::Object(body).to_string()
}

#[cfg(test)]
mod tests {
    use super::{Options, PROTOCOL_VERSION, ToolError, argv_for, handle_line, tool_defs};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    fn opts(allow_write: bool) -> Options {
        Options {
            repo: PathBuf::from("."),
            config: PathBuf::from("/nonexistent/walgit.toml"),
            allow_write,
            key: Some(PathBuf::from("/nonexistent/key")),
            remote: "origin".into(),
        }
    }

    async fn reply(line: &str, allow_write: bool) -> Value {
        let out = handle_line(line, &opts(allow_write))
            .await
            .expect("a reply");
        serde_json::from_str(&out).expect("valid JSON")
    }

    #[test]
    fn the_tool_surface_is_exactly_the_read_only_list() {
        // An exact allowlist, not a keyword scan: adding a tool has to be a
        // deliberate edit here too.
        let names: Vec<&str> = tool_defs().iter().map(|t| t.name).collect();
        let expected = [
            "repo_list",
            "repo_owners",
            "repo_refs",
            "repo_resolve",
            "repo_tree",
            "repo_blob",
            "repo_commits",
            "repo_diff",
            "collab_ls",
            "collab_thread",
            "collab_pr",
            "collab_board",
            "collab_report",
            "ci_status",
            "wal_ls",
            "collab_entry",
        ];
        assert_eq!(names, expected);
        assert_eq!(
            tool_defs()
                .iter()
                .filter(|t| t.write)
                .map(|t| t.name)
                .collect::<Vec<_>>(),
            vec!["collab_entry"],
            "exactly one writing tool"
        );
        // Bytes must not travel over MCP: no tool may expose the raw channel.
        for tool in tool_defs() {
            let schema = tool.schema.to_string();
            assert!(
                !schema.contains("\"raw\""),
                "{} exposes raw bytes",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn writing_tools_need_the_flag() {
        let names = |v: &Value| -> Vec<String> {
            v["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        };
        let read_only = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, false).await;
        assert!(names(&read_only).contains(&"repo_tree".to_string()));
        assert!(!names(&read_only).contains(&"collab_entry".to_string()));

        let writable = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, true).await;
        assert!(names(&writable).contains(&"collab_entry".to_string()));

        // The gate must hold on the call path too, not just in the listing.
        let denied = reply(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"collab_entry","arguments":{"kind":"comment","id":"t","actor":"a","body":"{}"}}}"#,
            false,
        )
        .await;
        assert_eq!(denied["error"]["code"], -32602, "{denied}");
        assert!(
            denied["result"].is_null(),
            "a gated tool must not return content"
        );
    }

    #[tokio::test]
    async fn unknown_tools_and_bad_arguments_are_protocol_errors() {
        let unknown = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gc","arguments":{}}}"#,
            true,
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
        assert!(
            unknown["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );

        // A tool that exists but is missing a required argument is the caller's
        // mistake, not a tool failure.
        let missing = reply(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"repo_tree","arguments":{}}}"#,
            false,
        )
        .await;
        assert_eq!(missing["error"]["code"], -32602, "{missing}");
        assert!(missing["result"].is_null());

        // An enum-ish argument is validated where the CLI would only sigh later.
        let bad_kind = reply(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"repo_refs","arguments":{"repo":"o/r","kind":"../etc"}}}"#,
            false,
        )
        .await;
        assert_eq!(bad_kind["error"]["code"], -32602, "{bad_kind}");
    }

    /// The injection test the first review asked for: a caller string must never
    /// land in an *option* position, where clap would honour it (`--url=…` used
    /// to redirect the request; `--help` used to answer with a help text).
    #[test]
    fn caller_strings_cannot_become_clap_options() {
        let o = opts(false);
        let argv = argv_for(
            "repo_tree",
            &json!({"repo": "--url=http://127.0.0.1:9", "rev": "owner/repo", "path": "main"}),
            &o,
        )
        .expect("argv");
        // `--` before the positionals: everything after it is a value.
        let dash = argv
            .iter()
            .position(|a| a == "--")
            .expect("a `--` separator");
        assert!(argv[dash + 1..].contains(&"--url=http://127.0.0.1:9".to_string()));
        assert!(
            !argv[..dash].iter().any(|a| a.starts_with("--url")),
            "the injected option reached option position: {argv:?}"
        );

        // `--help` as a repository name is a value too, never a request for help.
        let argv = argv_for("repo_refs", &json!({"repo": "--help"}), &o).expect("argv");
        let dash = argv
            .iter()
            .position(|a| a == "--")
            .expect("a `--` separator");
        assert_eq!(argv[dash + 1], "--help");

        // Flag *values* are joined with `=`, so they can carry a leading dash
        // without being read as the next option.
        let argv = argv_for(
            "repo_commits",
            &json!({"repo": "o/r", "ref": "--url=http://127.0.0.1:9"}),
            &o,
        )
        .expect("argv");
        assert!(
            argv.contains(&"--ref=--url=http://127.0.0.1:9".to_string()),
            "{argv:?}"
        );
    }

    #[tokio::test]
    async fn handshake_echoes_a_known_protocol() {
        let asked = json!({"protocolVersion": "2024-11-05"});
        let line =
            json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": asked}).to_string();
        let v = reply(&line, false).await;
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        // `instructions` belongs to the result, next to `serverInfo` (not inside).
        assert!(v["result"]["instructions"].is_string(), "{v}");
        assert!(v["result"]["serverInfo"]["instructions"].is_null(), "{v}");
        assert_eq!(v["result"]["serverInfo"]["name"], "walgit");

        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}).to_string();
        assert_eq!(
            reply(&line, false).await["result"]["protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn protocol_errors_are_jsonrpc_shaped() {
        let bad = handle_line("{not json", &opts(false))
            .await
            .expect("a reply");
        let v: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], Value::Null);

        let v = reply(r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#, false).await;
        assert_eq!(v["error"]["code"], -32601);

        // A malformed envelope is a *bad request*, not an unknown method.
        let v = reply(r#"{"id":1,"method":"tools/list"}"#, false).await;
        assert_eq!(v["error"]["code"], -32600, "{v}");
        let v = reply(r#"{"jsonrpc":"1.0","id":1,"method":"tools/list"}"#, false).await;
        assert_eq!(v["error"]["code"], -32600, "{v}");
        let v = reply(
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"tools/list"}"#,
            false,
        )
        .await;
        assert_eq!(v["error"]["code"], -32600, "{v}");

        // Notifications never get a reply.
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &opts(false)
            )
            .await
            .is_none()
        );
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#, false).await["result"],
            json!({})
        );
    }

    #[test]
    fn empty_required_arguments_are_rejected() {
        let err = argv_for(
            "repo_tree",
            &json!({"repo": "  ", "rev": "main"}),
            &opts(false),
        )
        .expect_err("rejected");
        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "should be a protocol error"
        );
    }
}
