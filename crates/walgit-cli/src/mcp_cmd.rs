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
    let schema = |props: Value, required: Value| {
        json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
    };
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
            schema: schema(json!({"repo": repo_arg, "rev": {"type": "string"}}), json!(["repo", "rev"])),
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
            description: "One file's blob envelope (text contents; `raw` for the byte channel URL behaviour).",
            write: false,
            schema: schema(
                json!({"repo": repo_arg, "rev": {"type": "string"}, "path": {"type": "string"}, "raw": {"type": "boolean"}}),
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
            description:
                "The work-unit board (threads projected under `.walgit/board.toml`). Read-only: \
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
            description:
                "Append one signed collab entry (issue/comment/patch/review/merge_result/status) and its \
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
        if let Some(reply) = handle_line(&line, &opts) {
            out.write_all(reply.as_bytes()).await?;
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
    }
    Ok(())
}

/// One request line → one reply line (`None` for notifications). Split out from
/// the transport so the protocol is testable without spawning anything.
fn handle_line(line: &str, opts: &Options) -> Option<String> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error_reply(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or_default();
    // Notifications (no id) never get a reply — including `notifications/*`.
    if id.is_none() {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
            let version = if KNOWN_PROTOCOLS.contains(&asked) { asked } else { PROTOCOL_VERSION };
            Some(result_reply(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "walgit",
                        "version": env!("CARGO_PKG_VERSION"),
                        "instructions": "Tools are the walgit CLI. Writing tools need `walgit mcp --allow-write`."
                    }
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
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Some(match call_tool(&name, &args, opts) {
                Ok(text) => result_reply(id, json!({"content": [{"type": "text", "text": text}]})),
                Err(e) => result_reply(
                    id,
                    json!({"content": [{"type": "text", "text": e}], "isError": true}),
                ),
            })
        }
        other => Some(error_reply(id, -32601, &format!("method not found: {other}"))),
    }
}

/// Build the child argv for `name`. Unknown names — including every destructive
/// operation, which has no arm here — are refused before anything runs.
fn call_tool(name: &str, args: &Value, opts: &Options) -> Result<String, String> {
    let read_only = !tool_defs().iter().any(|t| t.name == name && t.write);
    if tool_defs().iter().all(|t| t.name != name) {
        return Err(format!("unknown tool: {name}"));
    }
    if !read_only && !opts.allow_write {
        return Err(format!(
            "`{name}` writes; start the server with `--allow-write` to register it"
        ));
    }
    let argv = argv_for(name, args, opts)?;
    let exe = std::env::current_exe().map_err(|e| format!("locating walgit: {e}"))?;
    let out = std::process::Command::new(&exe)
        .arg("--config")
        .arg(&opts.config)
        .args(&argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running walgit {}: {e}", argv.join(" ")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "walgit {} failed ({}): {}",
            argv.join(" "),
            out.status,
            if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() }
        ))
    }
}

fn argv_for(name: &str, args: &Value, opts: &Options) -> Result<Vec<String>, String> {
    let s = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("`{key}` is required"))
    };
    let opt = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let path = || opts.repo.display().to_string();
    let mut v: Vec<String> = match name {
        "repo_list" => vec!["repo".into(), "list".into()],
        "repo_owners" => {
            let mut v = vec!["repo".into(), "owners".into()];
            if let Some(o) = opt("owner") {
                v.push(o);
            }
            v
        }
        "repo_refs" => {
            let mut v = vec!["repo".into(), "refs".into(), s("repo")?];
            if let Some(k) = opt("kind") {
                v.push(k);
            }
            v
        }
        "repo_resolve" => vec!["repo".into(), "resolve".into(), s("repo")?, s("rev")?],
        "repo_tree" => {
            let mut v = vec!["repo".into(), "tree".into(), s("repo")?, s("rev")?];
            if let Some(p) = opt("path") {
                v.push(p);
            }
            v
        }
        "repo_blob" => {
            let mut v = vec!["repo".into(), "blob".into(), s("repo")?, s("rev")?, s("path")?];
            if args.get("raw").and_then(Value::as_bool).unwrap_or(false) {
                v.push("--raw".into());
            }
            v
        }
        "repo_commits" => {
            let mut v = vec!["repo".into(), "commits".into(), s("repo")?];
            if let Some(r) = opt("ref") {
                v.push("--ref".into());
                v.push(r);
            }
            if let Some(n) = args.get("n").and_then(Value::as_u64) {
                v.push("--n".into());
                v.push(n.to_string());
            }
            if let Some(k) = args.get("skip").and_then(Value::as_u64) {
                v.push("--skip".into());
                v.push(k.to_string());
            }
            if let Some(p) = opt("path") {
                v.push("--path".into());
                v.push(p);
            }
            v
        }
        "repo_diff" => {
            let mut v = vec!["repo".into(), "diff".into(), s("repo")?, s("from")?, s("to")?];
            if let Some(f) = opt("format") {
                v.push("--format".into());
                v.push(f);
            }
            v
        }
        "collab_ls" => vec!["collab".into(), "ls".into(), "--repo".into(), path()],
        "collab_thread" => vec![
            "collab".into(),
            "thread".into(),
            s("id")?,
            "--repo".into(),
            path(),
        ],
        "collab_pr" => vec!["collab".into(), "pr".into(), s("id")?, "--repo".into(), path()],
        "collab_board" => vec!["collab".into(), "board".into(), "--repo".into(), path()],
        "collab_report" => vec!["collab".into(), "report".into(), "--repo".into(), path()],
        "ci_status" => vec![
            "ci".into(),
            "status".into(),
            "--repo".into(),
            path(),
        ],
        "wal_ls" => {
            let mut v = vec!["wal".into(), "ls".into(), s("repo")?];
            if let Some(f) = args.get("from").and_then(Value::as_u64) {
                v.push("--from".into());
                v.push(f.to_string());
            }
            if let Some(t) = args.get("to").and_then(Value::as_u64) {
                v.push("--to".into());
                v.push(t.to_string());
            }
            v
        }
        "collab_entry" => {
            let key = opts
                .key
                .as_ref()
                .ok_or("`collab_entry` needs `--key <ed25519 seed file>` on the server")?
                .display()
                .to_string();
            let mut v = vec![
                "collab".into(),
                "entry".into(),
                "--repo".into(),
                path(),
                "--kind".into(),
                s("kind")?,
                "--id".into(),
                s("id")?,
                "--actor".into(),
                s("actor")?,
                "--body".into(),
                s("body")?,
                "--key".into(),
                key,
                "--push".into(),
                opts.remote.clone(),
            ];
            if let Some(p) = opt("parent") {
                v.push("--parent".into());
                v.push(p);
            }
            if let Some(b) = opt("base") {
                v.push("--base".into());
                v.push(b);
            }
            if let Some(h) = opt("head") {
                v.push("--head".into());
                v.push(h);
            }
            v
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    v.retain(|seg| !seg.is_empty() || true);
    Ok(v)
}

fn result_reply(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error_reply(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::{Options, PROTOCOL_VERSION, handle_line, tool_defs};
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

    fn reply(line: &str, allow_write: bool) -> Value {
        let out = handle_line(line, &opts(allow_write)).expect("a reply");
        serde_json::from_str(&out).expect("valid JSON")
    }

    #[test]
    fn destructive_operations_are_not_on_the_menu() {
        // The safety property is "not reachable", not "discouraged": no tool may
        // name a destructive operation, and `tools/call` has no arm for one.
        let banned = ["gc", "reclaim", "compact", "import", "delete", "policy", "settings"];
        for tool in tool_defs() {
            let lowered = tool.name.to_ascii_lowercase();
            for word in banned {
                assert!(!lowered.contains(word), "tool {} looks destructive", tool.name);
            }
            assert!(!tool.description.trim().is_empty(), "{} has no description", tool.name);
        }
        let err = super::call_tool("gc", &json!({}), &opts(true)).expect_err("refused");
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[test]
    fn writing_tools_need_the_flag() {
        let read_only = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, false);
        let names: Vec<&str> = read_only["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"repo_tree"));
        assert!(!names.contains(&"collab_entry"), "write tool leaked: {names:?}");

        let writable = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, true);
        let names: Vec<&str> = writable["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"collab_entry"));

        // …and calling it without the flag is refused, not silently allowed.
        let err = super::call_tool("collab_entry", &json!({"kind":"comment"}), &opts(false))
            .expect_err("refused");
        assert!(err.contains("--allow-write"), "{err}");
    }

    #[test]
    fn handshake_echoes_a_known_protocol() {
        let asked = json!({"protocolVersion": "2024-11-05"});
        let line = json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": asked}).to_string();
        let v = reply(&line, false);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "walgit");

        // An unknown revision gets ours, and the client decides.
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}).to_string();
        assert_eq!(reply(&line, false)["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn protocol_errors_are_jsonrpc_shaped() {
        let bad = handle_line("{not json", &opts(false)).expect("a reply");
        let v: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], Value::Null);

        let v = reply(r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#, false);
        assert_eq!(v["error"]["code"], -32601);

        // Notifications never get a reply.
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &opts(false)).is_none());
        assert_eq!(reply(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#, false)["result"], json!({}));
    }
}
