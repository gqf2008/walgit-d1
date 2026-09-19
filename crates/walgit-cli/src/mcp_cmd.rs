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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::proc_group::{kill_tree, spawn_in_own_group};

/// Latest revision this adapter implements; the client's requested one is
/// echoed when we know it (the spec lets the server choose and the client
/// decide).
const PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const MIN_SUBSCRIBE_INTERVAL_MS: u64 = 1000;
pub(crate) const DEFAULT_SUBSCRIBE_INTERVAL_MS: u64 = 5000;
pub(crate) const DEFAULT_MAX_SUBSCRIPTIONS: u64 = 32;
const SUBSCRIBE_FAILURE_LIMIT: u32 = 5;
const SUBSCRIBE_BACKOFF_MAX_MS: u64 = 60_000;
const NOTIFY_QUEUE_CAPACITY: usize = 128;
const REF_SAMPLE_BYTES: usize = 64 * 1024;

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
    /// Poll interval for resource subscriptions. Values below one second are
    /// rejected on `resources/subscribe` with `-32602`.
    pub subscribe_interval_ms: u64,
    /// Maximum live resource subscriptions. Zero is rejected; attempts to add
    /// more than this many live subscriptions fail rather than being dropped.
    pub max_subscriptions: u64,
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

    tracing::debug!("mcp stdio ready");
    let (tx, mut rx) = mpsc::channel::<String>(NOTIFY_QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err()
                || out.write_all(b"\n").await.is_err()
                || out.flush().await.is_err()
            {
                break;
            }
        }
    });
    let session = Session::new(opts, tx.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let mut signalled = false;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.context("reading stdin")? else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(reply) = session.handle_line(&line).await
                    && tx.send(reply).await.is_err()
                {
                    break;
                }
            }
            () = wait_shutdown(&mut shutdown_rx) => {
                signalled = true;
                break;
            }
        }
    }
    session.shutdown().await;
    drop(session);
    drop(tx);
    let _ = writer.await;
    if signalled {
        // `tokio::signal`'s driver can keep the runtime alive after a catch-up
        // signal on macOS even though every MCP task and child has stopped.
        // Shutdown is complete here, so terminate the process deterministically.
        std::process::exit(0);
    }
    Ok(())
}

struct Subscription {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Default)]
struct CollabSignal {
    generation: u64,
    digest: String,
    error: Option<String>,
}

struct CollabPuller {
    signal: watch::Sender<CollabSignal>,
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

struct Session {
    opts: Options,
    notify_tx: mpsc::Sender<String>,
    subscriptions: parking_lot::Mutex<HashMap<String, Subscription>>,
    collab: parking_lot::Mutex<Option<CollabPuller>>,
}

impl Session {
    fn new(opts: Options, notify_tx: mpsc::Sender<String>) -> Self {
        Self {
            opts,
            notify_tx,
            subscriptions: parking_lot::Mutex::new(HashMap::new()),
            collab: parking_lot::Mutex::new(None),
        }
    }

    /// One request line → one reply line (`None` for notifications). Split out
    /// from the transport so the protocol is testable without spawning anything.
    async fn handle_line(&self, line: &str) -> Option<String> {
        let opts = &self.opts;
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
            // A *missing or non-string* method is an invalid request, not a
            // notification — JSON-RPC 2.0 requires a method even for notifications.
            // Staying silent would leave the client waiting for a reply that never
            // comes, so answer with `id: null` as the spec prescribes.
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
                if !params.is_object() {
                    return Some(error_reply(
                        id,
                        -32602,
                        "invalid params: `params` must be an object",
                    ));
                }
                let asked = match params.get("protocolVersion") {
                    None => None,
                    Some(Value::String(version)) => Some(version.as_str()),
                    Some(_) => {
                        return Some(error_reply(
                            id,
                            -32602,
                            "invalid params: `protocolVersion` must be a string",
                        ));
                    }
                };
                let version = match asked {
                    Some(version) if KNOWN_PROTOCOLS.contains(&version) => version,
                    _ => PROTOCOL_VERSION,
                };
                Some(result_reply(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {
                            "tools": {"listChanged": false},
                            "resources": {"subscribe": true, "listChanged": true},
                            "logging": {}
                        },
                        // `instructions` sits next to `serverInfo`, not inside it —
                        // that is where hosts read it.
                        "instructions":
                            "Tools are the walgit CLI (one implementation, no second schema). Read-only \
                             unless the server was started with `--allow-write`; destructive operations \
                             are not exposed at all. Resource subscriptions are client-side polling: \
                             per-instance and best-effort, so keep durable cursors in the caller.",
                        "serverInfo": {"name": "walgit", "version": env!("CARGO_PKG_VERSION")}
                    }),
                ))
            }
            "ping" => Some(result_reply(id, json!({}))),
            "tools/list" => {
                if !params.is_null() && !params.is_object() {
                    return Some(error_reply(
                        id,
                        -32602,
                        "invalid params: `params` must be an object",
                    ));
                }
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
                if !params.is_object() {
                    return Some(error_reply(
                        id,
                        -32602,
                        "invalid params: `params` must be an object",
                    ));
                }
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error_reply(
                        id,
                        -32602,
                        "invalid params: `name` must be a string",
                    ));
                };
                // `arguments` is defined as an object; an array or scalar used to
                // execute the tool with the value silently ignored.
                let args = match params.get("arguments") {
                    None => json!({}),
                    Some(v) if v.is_object() => v.clone(),
                    Some(_) => {
                        return Some(error_reply(
                            id,
                            -32602,
                            "invalid params: `arguments` must be an object",
                        ));
                    }
                };
                Some(match call_tool(name, &args, opts).await {
                    Ok(text) => {
                        result_reply(id, json!({"content": [{"type": "text", "text": text}]}))
                    }
                    // A request the model can repair is a protocol error; a tool that
                    // ran and failed is content with `isError`.
                    Err(ToolError::InvalidParams(m)) => error_reply(id, -32602, &m),
                    Err(ToolError::Execution(m)) => result_reply(
                        id,
                        json!({"content": [{"type": "text", "text": m}], "isError": true}),
                    ),
                })
            }
            "resources/list" => {
                if !params.is_null() && !params.is_object() {
                    return Some(error_reply(
                        id,
                        -32602,
                        "invalid params: `params` must be an object",
                    ));
                }
                Some(result_reply(
                    id,
                    json!({"resources": self.resource_list().await}),
                ))
            }
            "resources/read" => {
                let uri = match resource_uri_param(&params) {
                    Ok(uri) => uri,
                    Err(e) => return Some(error_reply(id, e.code(), &e.message())),
                };
                match read_resource_uri(opts, &uri, None).await {
                    Ok(snapshot) => Some(result_reply(id, snapshot.response(&uri))),
                    Err(e) => Some(error_reply(id, e.code(), &e.message())),
                }
            }
            "resources/subscribe" => {
                let uri = match resource_uri_param(&params) {
                    Ok(uri) => uri,
                    Err(e) => return Some(error_reply(id, e.code(), &e.message())),
                };
                match self.subscribe(&uri).await {
                    Ok(()) => Some(result_reply(id, json!({}))),
                    Err(e) => Some(error_reply(id, e.code(), &e.message())),
                }
            }
            "resources/unsubscribe" => {
                let uri = match resource_uri_param(&params) {
                    Ok(uri) => uri,
                    Err(e) => return Some(error_reply(id, e.code(), &e.message())),
                };
                match self.unsubscribe(&uri).await {
                    Ok(()) => Some(result_reply(id, json!({}))),
                    Err(e) => Some(error_reply(id, e.code(), &e.message())),
                }
            }
            other => Some(error_reply(
                id,
                -32601,
                &format!("method not found: {other}"),
            )),
        }
    }

    async fn subscribe(&self, uri: &str) -> Result<(), ResourceError> {
        validate_subscription_options(&self.opts)?;
        let parsed = ResourceUri::parse(uri)?;
        {
            let mut subscriptions = self.subscriptions.lock();
            subscriptions.retain(|_, sub| !sub.task.is_finished());
            if subscriptions.contains_key(uri) {
                return Ok(());
            }
            let live = u64::try_from(subscriptions.len()).unwrap_or(u64::MAX);
            if live >= self.opts.max_subscriptions {
                return Err(ResourceError::InvalidParams(format!(
                    "`--max-subscriptions` limit ({} live subscriptions) reached",
                    self.opts.max_subscriptions
                )));
            }
        }

        let version = probe_resource_version_initial(&self.opts, &parsed, None).await?;
        let collab_rx = matches!(
            parsed,
            ResourceUri::Board { .. } | ResourceUri::Thread { .. }
        )
        .then(|| self.ensure_collab_signal());
        let (cancel, cancel_rx) = watch::channel(false);
        let task = spawn_subscription(
            uri.to_string(),
            parsed,
            self.opts.clone(),
            self.notify_tx.clone(),
            cancel_rx,
            version,
            collab_rx,
        );
        let mut subscriptions = self.subscriptions.lock();
        subscriptions.retain(|_, sub| !sub.task.is_finished());
        if subscriptions.contains_key(uri) {
            let _ = cancel.send(true);
            let _ = task.await;
            return Ok(());
        }
        let live = u64::try_from(subscriptions.len()).unwrap_or(u64::MAX);
        if live >= self.opts.max_subscriptions {
            let _ = cancel.send(true);
            let _ = task.await;
            return Err(ResourceError::InvalidParams(format!(
                "`--max-subscriptions` limit ({} live subscriptions) reached",
                self.opts.max_subscriptions
            )));
        }
        subscriptions.insert(uri.to_string(), Subscription { cancel, task });
        Ok(())
    }

    async fn unsubscribe(&self, uri: &str) -> Result<(), ResourceError> {
        ResourceUri::parse(uri)?;
        let sub = self.subscriptions.lock().remove(uri);
        if let Some(sub) = sub {
            let _ = sub.cancel.send(true);
            let _ = sub.task.await;
        }
        Ok(())
    }

    async fn shutdown(&self) {
        let collab = self.collab.lock().take();
        if let Some(collab) = &collab {
            let _ = collab.cancel.send(true);
        }
        let subscriptions = {
            let mut guard = self.subscriptions.lock();
            std::mem::take(&mut *guard)
        };
        for sub in subscriptions.values() {
            let _ = sub.cancel.send(true);
        }
        for sub in subscriptions.into_values() {
            let _ = sub.task.await;
        }
        if let Some(collab) = collab {
            let _ = collab.task.await;
        }
    }

    fn ensure_collab_signal(&self) -> watch::Receiver<CollabSignal> {
        let mut guard = self.collab.lock();
        if let Some(puller) = guard.as_ref() {
            return puller.signal.subscribe();
        }
        let (signal, _) = watch::channel(CollabSignal::default());
        let (cancel, cancel_rx) = watch::channel(false);
        let task = spawn_collab_puller(self.opts.clone(), signal.clone(), cancel_rx);
        let receiver = signal.subscribe();
        *guard = Some(CollabPuller {
            signal,
            cancel,
            task,
        });
        receiver
    }

    async fn resource_list(&self) -> Vec<Value> {
        let Some((owner, repo)) = discover_repo_identity(&self.opts, None).await else {
            return Vec::new();
        };
        let mut resources = vec![
            resource_item(
                &format!("walgit://refs/{owner}/{repo}"),
                &format!("Refs for {owner}/{repo}"),
                "Refs observed by `git ls-remote --refs`; `_meta.version` is a digest of the ref set.",
            ),
            resource_item(
                &format!("walgit://wal/{owner}/{repo}?from=0"),
                &format!("WAL for {owner}/{repo}"),
                "Retained WAL entries at or after `from`; `_meta.version` is the manifest head seq.",
            ),
            resource_item(
                &format!("walgit://collab/board/{owner}/{repo}"),
                &format!("Collab board for {owner}/{repo}"),
                "The deterministic board projection; `_meta.version` hashes the board bytes.",
            ),
        ];
        let _ = refresh_collab_refs(&self.opts, None).await;
        let repo_path = self.opts.repo.display().to_string();
        let argv = vec![
            "collab".to_string(),
            "ls".to_string(),
            format!("--repo={repo_path}"),
        ];
        if let Ok(list) = run_walgit(&argv, &self.opts.config, None).await {
            for id in list.lines().map(str::trim).filter(|id| !id.is_empty()) {
                resources.push(resource_item(
                    &format!("walgit://collab/thread/{owner}/{repo}/{id}"),
                    &format!("Collab thread {id}"),
                    "One parent-ordered thread; `_meta.version` is the head entry oid.",
                ));
            }
        }
        resources
    }
}

/// One request line → one reply line (`None` for notifications). Kept as a
/// free function for the stateless protocol tests; production owns one
/// `Session` for the lifetime of the stdio loop.
#[cfg(test)]
async fn handle_line(line: &str, opts: &Options) -> Option<String> {
    let (tx, _rx) = mpsc::channel(1);
    let session = Session::new(opts.clone(), tx);
    let reply = session.handle_line(line).await;
    session.shutdown().await;
    reply
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
    validate_arguments(&spec, args)?;
    let argv = argv_for(name, args, opts)?;
    run_child(&argv, &opts.config).await
}

/// Hold `arguments` to the schema the tool advertises: an object, known fields,
/// declared types, required present. `additionalProperties: false` is a promise
/// to the model, so it is enforced rather than merely declared — a field the
/// model invents (or mistypes) must come back as `-32602`, not be dropped on the
/// floor.
fn validate_arguments(spec: &Tool, args: &Value) -> Result<(), ToolError> {
    let Some(obj) = args.as_object() else {
        return Err(ToolError::InvalidParams(
            "`arguments` must be an object".into(),
        ));
    };
    let props = spec.schema.get("properties").and_then(Value::as_object);
    for (key, value) in obj {
        let Some(field) = props.and_then(|p| p.get(key)) else {
            return Err(ToolError::InvalidParams(format!(
                "unknown argument `{key}` for `{}`",
                spec.name
            )));
        };
        if value.is_null() {
            return Err(ToolError::InvalidParams(format!(
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
            return Err(ToolError::InvalidParams(format!(
                "`{key}` must be a {}",
                declared.unwrap_or("value")
            )));
        }
        // Empty is meaningless for every argument except `collab_entry.parent`,
        // where "" is the documented "this is a root entry" marker.
        if key != "parent" && value.as_str().is_some_and(|v| v.trim().is_empty()) {
            return Err(ToolError::InvalidParams(format!(
                "`{key}` must not be empty"
            )));
        }
    }
    if let Some(required) = spec.schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            match obj.get(name) {
                Some(v) if !v.is_null() => {}
                _ => return Err(ToolError::InvalidParams(format!("`{name}` is required"))),
            }
        }
    }
    Ok(())
}

/// Run this same binary with `argv`, bounded in both time and output.
async fn run_child(argv: &[String], config: &std::path::Path) -> Result<String, ToolError> {
    let exe = std::env::current_exe()
        .map_err(|e| ToolError::Execution(format!("locating walgit: {e}")))?;
    let mut args = vec![format!("--config={}", config.display())];
    args.extend_from_slice(argv);
    run_child_with(&exe, &args, TOOL_TIMEOUT, TOOL_OUTPUT_MAX).await
}

/// Read one stream, stopping the moment it passes `cap` — **not** waiting for
/// EOF, because an endless writer never closes its pipe (a `join!` of two
/// `read_to_end`s deadlocks exactly there: stdout floods forever while stderr
/// stays open and silent). Returns the bytes plus "this one ran over".
async fn read_capped<R>(mut r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = vec![0_u8; 16 * 1024];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok((buf, false));
        }
        if buf.len() + n > cap {
            // `get` rather than indexing: the workspace lints forbid slicing.
            if let Some(head) = chunk.get(..cap.saturating_sub(buf.len())) {
                buf.extend_from_slice(head);
            }
            return Ok((buf, true));
        }
        if let Some(head) = chunk.get(..n) {
            buf.extend_from_slice(head);
        }
    }
}

/// The bounded exec itself, split out so the two limits are testable with a cheap
/// command (`sh -c yes` / `sh -c 'sleep 30'`) instead of a real walgit operation.
///
/// * `timeout` caps the wall clock; on expiry the child is killed and reaped.
/// * `cap` caps **each** of stdout/stderr. The reads stop at `cap + 1` bytes, so a
///   flooding child is cut off immediately (it then blocks on a full pipe and is
///   killed here) rather than being waited on until the deadline.
async fn run_child_with(
    exe: &std::path::Path,
    argv: &[String],
    timeout: std::time::Duration,
    cap: usize,
) -> Result<String, ToolError> {
    run_child_with_cancel_inner(exe, argv, timeout, cap, None).await
}

async fn run_child_with_cancel(
    exe: &std::path::Path,
    argv: &[String],
    timeout: std::time::Duration,
    cap: usize,
    cancel: watch::Receiver<bool>,
) -> Result<String, ToolError> {
    run_child_with_cancel_inner(exe, argv, timeout, cap, Some(cancel)).await
}

async fn run_child_with_cancel_inner(
    exe: &std::path::Path,
    argv: &[String],
    timeout: std::time::Duration,
    cap: usize,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ToolError> {
    let (keep_cancel_tx, local_cancel_rx) = watch::channel(false);
    let mut cancel_rx = cancel.unwrap_or(local_cancel_rx);
    let _keep_cancel_tx = keep_cancel_tx;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(argv)
        // Never the MCP stream: the child's stdin must not be able to consume it.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    spawn_in_own_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("running `{}`: {e}", argv.join(" "))))?;

    let shown = argv.join(" ");
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(ToolError::Execution(format!(
            "`{shown}`: stdout/stderr were not captured"
        )));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut over = false;
    let deadline = tokio::time::Instant::now() + timeout;
    {
        let mut out_fut = std::pin::pin!(read_capped(out, cap));
        let mut err_fut = std::pin::pin!(read_capped(err, cap));
        let (mut got_out, mut got_err) = (false, false);
        while !(got_out && got_err) {
            tokio::select! {
                r = &mut out_fut, if !got_out => {
                    let (buf, exceeded) = r.map_err(|e| ToolError::Execution(format!("reading `{shown}`: {e}")))?;
                    stdout = buf;
                    got_out = true;
                    over |= exceeded;
                }
                r = &mut err_fut, if !got_err => {
                    let (buf, exceeded) = r.map_err(|e| ToolError::Execution(format!("reading `{shown}`: {e}")))?;
                    stderr = buf;
                    got_err = true;
                    over |= exceeded;
                }
                () = wait_cancelled(&mut cancel_rx) => {
                    kill_tree(&mut child);
                    let _ = child.wait().await;
                    return Err(ToolError::Execution(format!("`{shown}` was cancelled")));
                }
                () = tokio::time::sleep_until(deadline) => {
                    kill_tree(&mut child);
                    let _ = child.wait().await;
                    return Err(ToolError::Execution(format!(
                        "`{shown}` timed out after {}s",
                        timeout.as_secs()
                    )));
                }
            }
            if over {
                break;
            }
        }
    }
    if over {
        kill_tree(&mut child);
        let _ = child.wait().await;
        return Err(ToolError::Execution(format!(
            "`{shown}` produced more than {} KiB — run the CLI directly for bulk output",
            cap / 1024
        )));
    }
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Err(ToolError::Execution(format!("waiting for `{shown}`: {e}")));
        }
        Err(_) => {
            kill_tree(&mut child);
            let _ = child.wait().await;
            return Err(ToolError::Execution(format!(
                "`{shown}` timed out after {}s",
                timeout.as_secs()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    if status.success() {
        Ok(stdout)
    } else {
        Err(ToolError::Execution(format!(
            "`{shown}` failed ({status}): {}",
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
    // Absent is "not provided"; anything present must be a string. Explicit
    // null is rejected too: the schemas are not nullable, and treating null as
    // absent made `path: null` diverge from the typed validation above.
    let opt = |key: &str| -> Result<Option<String>, ToolError> {
        match args.get(key) {
            None => Ok(None),
            Some(Value::String(v)) => Ok(Some(v.clone())),
            Some(_) => Err(ToolError::InvalidParams(format!(
                "`{key}` must be a string"
            ))),
        }
    };
    let one_of = |key: &str, allowed: &[&str]| -> Result<Option<String>, ToolError> {
        match opt(key)? {
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
            if let Some(owner) = opt("owner")? {
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
            if let Some(path) = opt("path")? {
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
            if let Some(r) = opt("ref")? {
                v.push(flag("--ref", &r));
            }
            if let Some(n) = count("n", 1000)? {
                v.push(flag("--n", &n.to_string()));
            }
            if let Some(skip) = count("skip", 1_000_000)? {
                v.push(flag("--skip", &skip.to_string()));
            }
            if let Some(p) = opt("path")? {
                v.push(flag("--path", &p));
            }
            v.push("--".into());
            v.push(req("repo")?);
            v
        }
        "repo_diff" => {
            let mut v = vec!["repo".into(), "diff".into()];
            if let Some(f) = opt("format")? {
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
            if let Some(parent) = opt("parent")? {
                v.push(flag("--parent", &parent));
            }
            if let Some(base) = opt("base")? {
                v.push(flag("--base", &base));
            }
            if let Some(head) = opt("head")? {
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

fn resource_uri_param(params: &Value) -> Result<String, ResourceError> {
    let Some(params) = params.as_object() else {
        return Err(ResourceError::InvalidParams(
            "invalid params: `params` must be an object".into(),
        ));
    };
    match params.get("uri").and_then(Value::as_str) {
        Some(uri) if !uri.trim().is_empty() => Ok(uri.to_string()),
        _ => Err(ResourceError::InvalidParams(
            "invalid params: `uri` must be a non-empty string".into(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResourceUri {
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

impl ResourceUri {
    fn parse(uri: &str) -> Result<Self, ResourceError> {
        let raw = uri.strip_prefix("walgit://").ok_or_else(|| {
            ResourceError::InvalidParams("resource uri must start with `walgit://`".into())
        })?;
        let (path, query) = raw
            .split_once('?')
            .map_or((raw, None), |(p, q)| (p, Some(q)));
        let mut parts = path.split('/');
        match parts.next() {
            Some("refs") => {
                let owner = valid_component(parts.next())?;
                let repo = valid_component(parts.next())?;
                if parts.next().is_some() || query.is_some() {
                    return Err(ResourceError::InvalidParams(
                        "invalid refs resource uri".into(),
                    ));
                }
                Ok(Self::Refs { owner, repo })
            }
            Some("wal") => {
                let owner = valid_component(parts.next())?;
                let repo = valid_component(parts.next())?;
                if parts.next().is_some() {
                    return Err(ResourceError::InvalidParams(
                        "invalid WAL resource uri".into(),
                    ));
                }
                let from = parse_from_query(query)?;
                Ok(Self::Wal { owner, repo, from })
            }
            Some("collab") => {
                let kind = parts.next();
                let owner = valid_component(parts.next())?;
                let repo = valid_component(parts.next())?;
                if query.is_some() {
                    return Err(ResourceError::InvalidParams(
                        "invalid collab resource uri".into(),
                    ));
                }
                match kind {
                    Some("board") if parts.next().is_none() => Ok(Self::Board { owner, repo }),
                    Some("thread") => {
                        let thread = valid_component(parts.next())?;
                        if parts.next().is_some() {
                            return Err(ResourceError::InvalidParams(
                                "invalid collab thread uri".into(),
                            ));
                        }
                        Ok(Self::Thread {
                            owner,
                            repo,
                            thread,
                        })
                    }
                    _ => Err(ResourceError::InvalidParams(
                        "invalid collab resource uri".into(),
                    )),
                }
            }
            _ => Err(ResourceError::InvalidParams(
                "unknown walgit resource uri".into(),
            )),
        }
    }

    fn owner_repo(&self) -> (&str, &str) {
        match self {
            Self::Refs { owner, repo }
            | Self::Wal { owner, repo, .. }
            | Self::Board { owner, repo }
            | Self::Thread { owner, repo, .. } => (owner, repo),
        }
    }
}

fn valid_component(value: Option<&str>) -> Result<String, ResourceError> {
    let Some(value) = value.filter(|v| !v.is_empty() && *v != "." && *v != "..") else {
        return Err(ResourceError::InvalidParams(
            "resource uri contains an invalid path component".into(),
        ));
    };
    Ok(value.to_string())
}

fn parse_from_query(query: Option<&str>) -> Result<u64, ResourceError> {
    let Some(query) = query else {
        return Ok(0);
    };
    let Some(value) = query.strip_prefix("from=") else {
        return Err(ResourceError::InvalidParams(
            "WAL resource query must be `?from=<seq>`".into(),
        ));
    };
    if value.contains('&') {
        return Err(ResourceError::InvalidParams(
            "WAL resource accepts only the `from` query parameter".into(),
        ));
    }
    value.parse::<u64>().map_err(|_| {
        ResourceError::InvalidParams("WAL resource `from` must be a non-negative integer".into())
    })
}

fn validate_subscription_options(opts: &Options) -> Result<(), ResourceError> {
    if opts.subscribe_interval_ms < MIN_SUBSCRIBE_INTERVAL_MS {
        return Err(ResourceError::InvalidParams(format!(
            "`--subscribe-interval-ms` must be >= {MIN_SUBSCRIBE_INTERVAL_MS}"
        )));
    }
    if opts.max_subscriptions == 0 {
        return Err(ResourceError::InvalidParams(
            "`--max-subscriptions` must be >= 1".into(),
        ));
    }
    Ok(())
}

fn subscription_delay_ms(interval_ms: u64, failures: u32) -> u64 {
    if failures == 0 {
        return interval_ms;
    }
    let shift = failures.min(16);
    interval_ms
        .saturating_mul(1_u64 << shift)
        .min(SUBSCRIBE_BACKOFF_MAX_MS)
}

struct VersionTracker {
    last: String,
}

impl VersionTracker {
    fn new(last: String) -> Self {
        Self { last }
    }

    fn changed(&mut self, version: &str) -> bool {
        if self.last == version {
            return false;
        }
        self.last = version.to_string();
        true
    }
}

struct ResourceSnapshot {
    body: Value,
    version: String,
}

impl ResourceSnapshot {
    fn response(&self, uri: &str) -> Value {
        let text = json!({
            "data": self.body,
            "_meta": {"version": self.version}
        })
        .to_string();
        json!({
            "contents": [{
                "uri": uri,
                "mimeType": "application/json",
                "text": text,
                "_meta": {"version": self.version}
            }]
        })
    }
}

#[derive(Debug)]
enum ResourceError {
    InvalidParams(String),
    NotFound(String),
    Execution(String),
}

impl ResourceError {
    fn code(&self) -> i64 {
        match self {
            Self::InvalidParams(_) => -32602,
            Self::NotFound(_) => -32002,
            Self::Execution(_) => -32603,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidParams(message) | Self::NotFound(message) | Self::Execution(message) => {
                message.clone()
            }
        }
    }
}

async fn read_resource_uri(
    opts: &Options,
    uri: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<ResourceSnapshot, ResourceError> {
    let parsed = ResourceUri::parse(uri)?;
    let (owner, repo) = parsed.owner_repo();
    ensure_identity(opts, owner, repo, cancel.clone()).await?;
    match parsed {
        ResourceUri::Refs { .. } => read_refs(opts, owner, repo, cancel).await,
        ResourceUri::Wal { from, .. } => read_wal(opts, owner, repo, from, cancel).await,
        ResourceUri::Board { .. } => {
            refresh_collab_refs(opts, cancel.clone()).await?;
            read_board(opts, cancel).await
        }
        ResourceUri::Thread { thread, .. } => {
            refresh_collab_refs(opts, cancel.clone()).await?;
            read_thread(opts, &thread, cancel).await
        }
    }
}

/// Initial version probe for `resources/subscribe`. Collab resources first
/// perform one ref-level fetch, then read only their local version object.
async fn probe_resource_version_initial(
    opts: &Options,
    parsed: &ResourceUri,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    probe_version_for_parsed(opts, parsed, cancel, true).await
}

/// Cheap poll probe for non-collab resources. Collab subscriptions use the
/// shared ref-level puller's signal and only recompute their local version when
/// that signal changes.
async fn probe_resource_version(
    opts: &Options,
    uri: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    let parsed = ResourceUri::parse(uri)?;
    probe_version_for_parsed(opts, &parsed, cancel, false).await
}

async fn probe_version_for_parsed(
    opts: &Options,
    parsed: &ResourceUri,
    cancel: Option<watch::Receiver<bool>>,
    refresh_collab: bool,
) -> Result<String, ResourceError> {
    let (owner, repo) = parsed.owner_repo();
    ensure_identity(opts, owner, repo, cancel.clone()).await?;
    match parsed {
        ResourceUri::Refs { .. } => probe_refs_version(opts, owner, repo, cancel).await,
        ResourceUri::Wal { .. } => probe_wal_version(opts, owner, repo, cancel).await,
        ResourceUri::Board { .. } | ResourceUri::Thread { .. } => {
            if refresh_collab {
                refresh_collab_refs(opts, cancel.clone()).await?;
            }
            probe_collab_version_local(opts, parsed, cancel).await
        }
    }
}

async fn probe_collab_version_local(
    opts: &Options,
    parsed: &ResourceUri,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    match parsed {
        ResourceUri::Board { .. } => probe_board_version_local(opts, cancel).await,
        ResourceUri::Thread { thread, .. } => {
            probe_thread_version_local(opts, thread, cancel).await
        }
        _ => Err(ResourceError::InvalidParams(
            "collab probe used for a non-collab resource".into(),
        )),
    }
}

async fn probe_refs_version(
    opts: &Options,
    owner: &str,
    repo: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    let args = vec![
        "-C".to_string(),
        opts.repo.display().to_string(),
        "ls-remote".to_string(),
        "--refs".to_string(),
        opts.remote.clone(),
    ];
    run_git_refs_streaming(&args, cancel)
        .await
        .map(|probe| probe.version)
        .map_err(|e| classify_resource_error(&format!("walgit://refs/{owner}/{repo}"), e))
}

async fn probe_wal_version(
    opts: &Options,
    owner: &str,
    repo: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    wal_head(opts, owner, repo, cancel)
        .await
        .map(|head| head.to_string())
}

async fn probe_board_version_local(
    opts: &Options,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    let argv = vec![
        "collab".into(),
        "board".into(),
        format!("--repo={}", opts.repo.display()),
        "--format=hash".into(),
    ];
    let out = run_walgit(&argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error("walgit://collab/board", e))?;
    let hash = out.trim();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ResourceError::Execution(
            "collab board --format=hash returned an invalid digest".into(),
        ));
    }
    Ok(hash.to_string())
}

async fn probe_thread_version_local(
    opts: &Options,
    thread: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    let argv = vec![
        "collab".into(),
        "thread-head".into(),
        format!("--repo={}", opts.repo.display()),
        "--".into(),
        thread.to_string(),
    ];
    let out = run_walgit(&argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error(&format!("collab thread {thread}"), e))?;
    let head = out.trim();
    if head.is_empty() {
        return Err(ResourceError::Execution(
            "collab thread-head reported no head oid".into(),
        ));
    }
    Ok(head.to_string())
}

async fn read_refs(
    opts: &Options,
    owner: &str,
    repo: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<ResourceSnapshot, ResourceError> {
    let args = vec![
        "-C".to_string(),
        opts.repo.display().to_string(),
        "ls-remote".to_string(),
        "--refs".to_string(),
        opts.remote.clone(),
    ];
    let probe = run_git_refs_streaming(&args, cancel)
        .await
        .map_err(|e| classify_resource_error(&format!("walgit://refs/{owner}/{repo}"), e))?;
    let refs: Vec<Value> = probe
        .lines
        .iter()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some(json!({"name": name, "oid": oid}))
        })
        .collect();
    let body = json!({
        "remote": opts.remote,
        "refs": refs,
        "total": probe.count,
        "truncated": probe.truncated || probe.count > probe.lines.len() as u64
    });
    Ok(ResourceSnapshot {
        body,
        version: probe.version,
    })
}

async fn read_wal(
    opts: &Options,
    owner: &str,
    repo: &str,
    from: u64,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<ResourceSnapshot, ResourceError> {
    let id = format!("{owner}/{repo}");
    let uri = format!("walgit://wal/{id}?from={from}");
    let head_seq = wal_head(opts, owner, repo, cancel.clone()).await?;
    let ls_argv = vec![
        "wal".into(),
        "ls".into(),
        format!("--from={from}"),
        format!("--to={head_seq}"),
        "--".into(),
        id,
    ];
    let out = run_walgit(&ls_argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error(&uri, e))?;
    let entries: Vec<Value> = out
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let seq = fields.next()?.parse::<u64>().ok()?;
            let kind = fields.next().unwrap_or_default();
            Some(json!({"seq": seq, "kind": kind, "summary": fields.collect::<Vec<_>>().join(" ")}))
        })
        .collect();
    let body = json!({"from": from, "head_seq": head_seq, "entries": entries, "text": out});
    Ok(ResourceSnapshot {
        body,
        version: head_seq.to_string(),
    })
}

async fn read_board(
    opts: &Options,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<ResourceSnapshot, ResourceError> {
    let argv = vec![
        "collab".into(),
        "board".into(),
        format!("--repo={}", opts.repo.display()),
        "--format=json".into(),
    ];
    let out = run_walgit(&argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error("walgit://collab/board", e))?;
    let board: Value = serde_json::from_str(&out).map_err(resource_execution)?;
    let version = sha256_hex(&serde_json::to_vec(&board).map_err(resource_execution)?);
    Ok(ResourceSnapshot {
        body: json!({"board": board}),
        version,
    })
}

async fn read_thread(
    opts: &Options,
    thread: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<ResourceSnapshot, ResourceError> {
    let argv = vec![
        "collab".into(),
        "thread".into(),
        format!("--repo={}", opts.repo.display()),
        "--".into(),
        thread.to_string(),
    ];
    let out = run_walgit(&argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error(&format!("collab thread {thread}"), e))?;
    let entries: Value = serde_json::from_str(&out).map_err(resource_execution)?;
    let version = entries
        .as_array()
        .and_then(|entries| entries.last())
        .and_then(|entry| entry.get("oid"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ResourceError::Execution("collab thread reported no head oid".into()))?;
    Ok(ResourceSnapshot {
        body: json!({"thread_id": thread, "entries": entries}),
        version,
    })
}

async fn wal_head(
    opts: &Options,
    owner: &str,
    repo: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<u64, ResourceError> {
    let id = format!("{owner}/{repo}");
    let uri = format!("walgit://wal/{id}");
    let argv = vec![
        "wal".into(),
        "head".into(),
        "--fresh".into(),
        "--".into(),
        id,
    ];
    let out = run_walgit(&argv, &opts.config, cancel)
        .await
        .map_err(|e| classify_resource_error(&uri, e))?;
    out.trim().parse::<u64>().map_err(|_| {
        ResourceError::Execution("walgit wal head did not report a sequence number".into())
    })
}

async fn refresh_collab_refs(
    opts: &Options,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<(), ResourceError> {
    let args = vec![
        "-C".to_string(),
        opts.repo.display().to_string(),
        "fetch".to_string(),
        "-q".to_string(),
        "--prune".to_string(),
        opts.remote.clone(),
        "+refs/collab/inbox/*:refs/collab/inbox/*".to_string(),
        "+refs/collab/meta/*:refs/collab/meta/*".to_string(),
    ];
    run_git(&args, cancel)
        .await
        .map(|_| ())
        .map_err(|e| classify_resource_error("refs/collab/*", e))
}

async fn collab_remote_digest(
    opts: &Options,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ResourceError> {
    let args = vec![
        "-C".to_string(),
        opts.repo.display().to_string(),
        "ls-remote".to_string(),
        "--refs".to_string(),
        opts.remote.clone(),
        "refs/collab/inbox/*".to_string(),
        "refs/collab/meta/*".to_string(),
    ];
    run_git_refs_streaming(&args, cancel)
        .await
        .map(|probe| probe.version)
        .map_err(|e| classify_resource_error("refs/collab/*", e))
}

async fn ensure_identity(
    opts: &Options,
    owner: &str,
    repo: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<(), ResourceError> {
    let Some((actual_owner, actual_repo)) = discover_repo_identity(opts, cancel).await else {
        return Err(ResourceError::InvalidParams(format!(
            "cannot determine {owner}/{repo} from --repo's `{}` remote",
            opts.remote
        )));
    };
    if actual_owner != owner || actual_repo != repo {
        return Err(ResourceError::InvalidParams(format!(
            "resource uri names {owner}/{repo}, but --repo's `{}` remote is {actual_owner}/{actual_repo}",
            opts.remote
        )));
    }
    Ok(())
}

async fn discover_repo_identity(
    opts: &Options,
    cancel: Option<watch::Receiver<bool>>,
) -> Option<(String, String)> {
    let args = vec![
        "-C".to_string(),
        opts.repo.display().to_string(),
        "remote".to_string(),
        "get-url".to_string(),
        opts.remote.clone(),
    ];
    let remote = run_git(&args, cancel).await.ok()?;
    parse_remote_identity(remote.trim())
}

fn parse_remote_identity(remote: &str) -> Option<(String, String)> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let path = if let Some((_, rest)) = remote.split_once("://") {
        rest.split_once('/').map_or(rest, |(_, path)| path)
    } else if let Some((_, path)) = remote
        .split_once('@')
        .and_then(|(_, rest)| rest.split_once(':'))
    {
        path
    } else {
        remote
    };
    let mut parts = path.rsplit('/').filter(|part| !part.is_empty());
    let repo = parts.next()?.to_string();
    let owner = parts.next()?.to_string();
    if owner.is_empty()
        || repo.is_empty()
        || owner == "."
        || owner == ".."
        || repo == "."
        || repo == ".."
    {
        return None;
    }
    Some((owner, repo))
}

async fn run_walgit(
    argv: &[String],
    config: &Path,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ToolError> {
    let exe = std::env::current_exe()
        .map_err(|e| ToolError::Execution(format!("locating walgit: {e}")))?;
    let mut args = vec![format!("--config={}", config.display())];
    args.extend_from_slice(argv);
    match cancel {
        Some(cancel) => {
            run_child_with_cancel(&exe, &args, TOOL_TIMEOUT, TOOL_OUTPUT_MAX, cancel).await
        }
        None => run_child_with(&exe, &args, TOOL_TIMEOUT, TOOL_OUTPUT_MAX).await,
    }
}

async fn run_git(
    args: &[String],
    cancel: Option<watch::Receiver<bool>>,
) -> Result<String, ToolError> {
    match cancel {
        Some(cancel) => {
            run_child_with_cancel(
                Path::new("git"),
                args,
                Duration::from_secs(10),
                64 * 1024,
                cancel,
            )
            .await
        }
        None => run_child_with(Path::new("git"), args, Duration::from_secs(10), 64 * 1024).await,
    }
}

#[derive(Debug)]
struct RefsProbe {
    version: String,
    lines: Vec<String>,
    count: u64,
    truncated: bool,
}

async fn run_git_refs_streaming(
    args: &[String],
    cancel: Option<watch::Receiver<bool>>,
) -> Result<RefsProbe, ToolError> {
    use sha2::Digest as _;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (keep_cancel_tx, local_cancel_rx) = watch::channel(false);
    let mut cancel_rx = cancel.unwrap_or(local_cancel_rx);
    let _keep_cancel_tx = keep_cancel_tx;
    let mut cmd = tokio::process::Command::new(Path::new("git"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    spawn_in_own_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("running `{}`: {e}", args.join(" "))))?;
    let shown = args.join(" ");
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(ToolError::Execution(format!(
            "`{shown}`: stdout/stderr were not captured"
        )));
    };
    let mut stdout = BufReader::new(out).lines();
    let mut stderr_fut = std::pin::pin!(read_capped(err, 64 * 1024));
    let mut hasher = sha2::Sha256::new();
    let mut lines = Vec::new();
    let mut sample_bytes = 0usize;
    let mut count = 0u64;
    let mut truncated = false;
    let mut stderr = Vec::new();
    let mut over = false;
    let (mut out_done, mut err_done) = (false, false);
    let deadline = tokio::time::Instant::now() + TOOL_TIMEOUT;
    while !(out_done && err_done) {
        tokio::select! {
            line = stdout.next_line(), if !out_done => {
                match line.map_err(|e| ToolError::Execution(format!("reading `{shown}`: {e}")))? {
                    Some(line) => {
                        hasher.update(line.as_bytes());
                        hasher.update(b"\n");
                        count = count.saturating_add(1);
                        let bytes = line.len().saturating_add(1);
                        if !truncated {
                            if sample_bytes.saturating_add(bytes) <= REF_SAMPLE_BYTES {
                                sample_bytes = sample_bytes.saturating_add(bytes);
                                lines.push(line);
                            } else {
                                truncated = true;
                            }
                        }
                    }
                    None => out_done = true,
                }
            }
            r = &mut stderr_fut, if !err_done => {
                let (buf, exceeded) = r.map_err(|e| ToolError::Execution(format!("reading `{shown}`: {e}")))?;
                stderr = buf;
                over |= exceeded;
                err_done = true;
            }
            () = wait_cancelled(&mut cancel_rx) => {
                kill_tree(&mut child);
                let _ = child.wait().await;
                return Err(ToolError::Execution(format!("`{shown}` was cancelled")));
            }
            () = tokio::time::sleep_until(deadline) => {
                kill_tree(&mut child);
                let _ = child.wait().await;
                return Err(ToolError::Execution(format!(
                    "`{shown}` timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                )));
            }
        }
    }
    if over {
        kill_tree(&mut child);
        let _ = child.wait().await;
        return Err(ToolError::Execution(format!(
            "`{shown}` produced more than 64 KiB of stderr"
        )));
    }
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(ToolError::Execution(format!("waiting for `{shown}`: {e}"))),
        Err(_) => {
            kill_tree(&mut child);
            let _ = child.wait().await;
            return Err(ToolError::Execution(format!(
                "`{shown}` timed out after {}s",
                TOOL_TIMEOUT.as_secs()
            )));
        }
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(ToolError::Execution(format!(
            "`{shown}` failed ({status}): {}",
            stderr.trim()
        )));
    }
    Ok(RefsProbe {
        version: hex::encode(hasher.finalize()),
        lines,
        count,
        truncated,
    })
}

fn classify_resource_error(uri: &str, err: ToolError) -> ResourceError {
    let message = match err {
        ToolError::InvalidParams(message) | ToolError::Execution(message) => message,
    };
    let lower = message.to_ascii_lowercase();
    let missing = lower.contains("404")
        || lower.contains("not found")
        || lower.contains("no entries for thread")
        || lower.contains("does not appear to be a git repository");
    if missing {
        ResourceError::NotFound(format!("{uri}: {message}"))
    } else {
        ResourceError::Execution(message)
    }
}

fn resource_execution(error: serde_json::Error) -> ResourceError {
    ResourceError::Execution(format!("serializing resource: {error}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn resource_item(uri: &str, name: &str, description: &str) -> Value {
    json!({
        "uri": uri,
        "name": name,
        "description": description,
        "mimeType": "application/json"
    })
}

fn notification(method: &str, params: Option<Value>) -> String {
    let mut body = json!({"jsonrpc": "2.0", "method": method});
    if let Some(params) = params {
        body["params"] = params;
    }
    body.to_string()
}

fn updated_notification(uri: &str) -> String {
    notification("notifications/resources/updated", Some(json!({"uri": uri})))
}

fn subscription_failure_notification(uri: &str, error: &str) -> String {
    notification(
        "notifications/message",
        Some(json!({
            "level": "error",
            "logger": "walgit.mcp",
            "data": {"uri": uri, "error": error}
        })),
    )
}

fn spawn_subscription(
    uri: String,
    parsed: ResourceUri,
    opts: Options,
    notify_tx: mpsc::Sender<String>,
    mut cancel: watch::Receiver<bool>,
    initial_version: String,
    collab_rx: Option<watch::Receiver<CollabSignal>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = VersionTracker::new(initial_version);
        let mut failures = 0_u32;
        let mut last_collab_generation = None;
        loop {
            let delay =
                Duration::from_millis(subscription_delay_ms(opts.subscribe_interval_ms, failures));
            tokio::select! {
                () = wait_cancelled(&mut cancel) => break,
                () = tokio::time::sleep(delay) => {}
            }
            if *cancel.borrow() {
                break;
            }
            let result = if let Some(collab_rx) = &collab_rx {
                let signal = collab_rx.borrow().clone();
                if signal.generation == 0 && signal.digest.is_empty() && signal.error.is_none() {
                    continue;
                }
                if last_collab_generation == Some(signal.generation) {
                    continue;
                }
                last_collab_generation = Some(signal.generation);
                if let Some(error) = signal.error {
                    Err(ResourceError::Execution(error))
                } else {
                    probe_collab_version_local(&opts, &parsed, Some(cancel.clone())).await
                }
            } else {
                probe_resource_version(&opts, &uri, Some(cancel.clone())).await
            };
            if *cancel.borrow() {
                break;
            }
            match result {
                Ok(version) => {
                    failures = 0;
                    if tracker.changed(&version) {
                        send_notification(&notify_tx, updated_notification(&uri));
                    }
                }
                Err(ResourceError::NotFound(_)) => {
                    send_notification(
                        &notify_tx,
                        notification("notifications/resources/list_changed", None),
                    );
                    break;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if failures >= SUBSCRIBE_FAILURE_LIMIT {
                        send_notification(
                            &notify_tx,
                            subscription_failure_notification(&uri, &error.message()),
                        );
                        break;
                    }
                }
            }
        }
    })
}

fn spawn_collab_puller(
    opts: Options,
    signal: watch::Sender<CollabSignal>,
    mut cancel: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut generation = 0_u64;
        let mut last_digest: Option<String> = None;
        let mut failures = 0_u32;
        loop {
            match collab_remote_digest(&opts, Some(cancel.clone())).await {
                Ok(digest) => {
                    failures = 0;
                    if last_digest.as_deref() != Some(digest.as_str()) {
                        match refresh_collab_refs(&opts, Some(cancel.clone())).await {
                            Ok(()) => {
                                last_digest = Some(digest.clone());
                                generation = generation.saturating_add(1);
                                let _ = signal.send(CollabSignal {
                                    generation,
                                    digest,
                                    error: None,
                                });
                            }
                            Err(error) => {
                                failures = 1;
                                generation = generation.saturating_add(1);
                                let _ = signal.send(CollabSignal {
                                    generation,
                                    digest: last_digest.clone().unwrap_or_default(),
                                    error: Some(error.message()),
                                });
                            }
                        }
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    generation = generation.saturating_add(1);
                    let _ = signal.send(CollabSignal {
                        generation,
                        digest: last_digest.clone().unwrap_or_default(),
                        error: Some(error.message()),
                    });
                }
            }
            let delay =
                Duration::from_millis(subscription_delay_ms(opts.subscribe_interval_ms, failures));
            tokio::select! {
                () = wait_cancelled(&mut cancel) => break,
                () = tokio::time::sleep(delay) => {}
            }
        }
    })
}

fn send_notification(notify_tx: &mpsc::Sender<String>, message: String) {
    if notify_tx.try_send(message).is_err() {
        tracing::warn!("dropping MCP notification: output queue is full or closed");
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

async fn wait_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.changed().await;
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_SUBSCRIPTIONS, DEFAULT_SUBSCRIBE_INTERVAL_MS, Options, PROTOCOL_VERSION,
        ResourceError, ResourceUri, Session, Subscription, ToolError, VersionTracker, argv_for,
        handle_line, run_git_refs_streaming, subscription_delay_ms, tool_defs,
        validate_subscription_options,
    };
    use serde_json::{Value, json};
    #[cfg(unix)]
    use std::path::Path;
    use std::path::PathBuf;
    use tokio::sync::{mpsc, watch};

    fn opts(allow_write: bool) -> Options {
        Options {
            repo: PathBuf::from("."),
            config: PathBuf::from("/nonexistent/walgit.toml"),
            allow_write,
            key: Some(PathBuf::from("/nonexistent/key")),
            remote: "origin".into(),
            subscribe_interval_ms: DEFAULT_SUBSCRIBE_INTERVAL_MS,
            max_subscriptions: DEFAULT_MAX_SUBSCRIPTIONS,
        }
    }

    async fn reply(line: &str, allow_write: bool) -> Value {
        let out = handle_line(line, &opts(allow_write))
            .await
            .expect("a reply");
        serde_json::from_str(&out).expect("valid JSON")
    }

    fn git(args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    /// A shell that records its own PID and a forked grandchild's PID before it
    /// (optionally) floods stdout or sleeps forever. The outer shell waits for
    /// both writes, so the test observes a ready process tree rather than a
    /// scheduling race.
    #[cfg(unix)]
    fn pid_command(pid_file: &Path, flood: bool) -> Vec<String> {
        let child = if flood {
            "echo $$ >> \"$1\"; while :; do printf x; done"
        } else {
            "echo $$ >> \"$1\"; exec sleep 30"
        };
        let script = format!(
            "echo $$ > \"$1\"\nsh -c {} sh \"$1\" &\nwhile [ \"$(wc -l < \"$1\" | tr -d ' ')\" -lt 2 ]; do sleep 0.01; done\nwait",
            shell_quote(child)
        );
        vec![
            "-c".to_string(),
            script,
            "sh".to_string(),
            pid_file.to_string_lossy().into_owned(),
        ]
    }

    #[cfg(unix)]
    #[allow(unsafe_code)] // kill(pid, 0) is the Unix process-existence probe.
    fn process_exists(pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 performs access checks only; it never signals the
        // process or mutates any state.
        if unsafe { libc::kill(pid, 0) } == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }

    #[cfg(unix)]
    fn assert_processes_are_gone(pid_file: &Path) {
        let raw = std::fs::read_to_string(pid_file).expect("pid file");
        let pids: Vec<u32> = raw
            .lines()
            .map(|line| line.trim().parse().expect("pid"))
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "both shell and grandchild wrote PIDs: {raw:?}"
        );
        for pid in pids {
            for _ in 0..100 {
                if !process_exists(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(!process_exists(pid), "PID {pid} survived the tree kill");
        }
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

        let line = json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": 123}}).to_string();
        let v = reply(&line, false).await;
        assert_eq!(v["error"]["code"], -32602, "{v}");
        assert!(v["result"].is_null(), "{v}");
    }

    #[tokio::test]
    async fn tools_list_params_are_typed() {
        let v = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":[]}"#,
            false,
        )
        .await;
        assert_eq!(v["error"]["code"], -32602, "{v}");
        assert!(v["result"].is_null(), "{v}");
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

    // The limits themselves, with a command that cannot cooperate: `yes` never
    // stops on its own, and `sleep 30` outlives the deadline. Without the
    // cap-and-kill these two tests hang, so a regression here is loud.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_flooding_tool_is_cut_off_and_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pids");
        let args = pid_command(&pid_file, true);
        let err = super::run_child_with(
            std::path::Path::new("sh"),
            &args,
            std::time::Duration::from_secs(30),
            4096,
        )
        .await
        .expect_err("cut off");
        assert!(format!("{err:?}").contains("more than"), "{err:?}");
        assert_processes_are_gone(&pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_wedged_tool_times_out_and_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pids");
        let args = pid_command(&pid_file, false);
        let started = std::time::Instant::now();
        let err = super::run_child_with(
            std::path::Path::new("sh"),
            &args,
            std::time::Duration::from_millis(300),
            4096,
        )
        .await
        .expect_err("timeout");
        assert!(format!("{err:?}").contains("timed out"), "{err:?}");
        // The deadline fired and the child was killed — not `sleep` finishing.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_processes_are_gone(&pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_wedged_tool_kills_the_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pids");
        let args = pid_command(&pid_file, false);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let watched_pid_file = pid_file.clone();
        tokio::spawn(async move {
            loop {
                let lines = std::fs::read_to_string(&watched_pid_file)
                    .map(|text| text.lines().count())
                    .unwrap_or(0);
                if lines >= 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let _ = cancel_tx.send(true);
        });
        let err = super::run_child_with_cancel(
            std::path::Path::new("sh"),
            &args,
            std::time::Duration::from_secs(30),
            4096,
            cancel_rx,
        )
        .await
        .expect_err("cancelled");
        assert!(format!("{err:?}").contains("cancelled"), "{err:?}");
        assert_processes_are_gone(&pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_quick_tool_under_both_limits_succeeds() {
        let args = vec!["-c".to_string(), "printf hello".to_string()];
        let out = super::run_child_with(
            std::path::Path::new("sh"),
            &args,
            std::time::Duration::from_secs(5),
            4096,
        )
        .await
        .expect("under both limits");
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn bad_arguments_are_rejected_before_anything_runs() {
        // `arguments` must be an object, fields must exist and be typed, and an
        // empty string is meaningless everywhere except `collab_entry.parent`.
        for (label, args) in [
            ("array arguments", json!([])),
            ("unknown field", json!({"nope": 1})),
            ("wrong type", json!({"repo": "o/r", "rev": 123})),
            ("required null", json!({"repo": null, "rev": "main"})),
            (
                "optional null",
                json!({"repo": "o/r", "rev": "main", "path": null}),
            ),
            ("empty repo", json!({"repo": "   ", "rev": "main"})),
        ] {
            let err = super::call_tool("repo_tree", &args, &opts(false))
                .await
                .expect_err(label);
            assert!(
                matches!(err, ToolError::InvalidParams(_)),
                "{label}: {err:?}"
            );
        }
        // …and the root marker stays legal.
        let ok = super::argv_for(
            "collab_entry",
            &json!({
                "kind": "comment",
                "id": "t",
                "actor": "a",
                "body": "{}",
                "parent": ""
            }),
            &opts(true),
        )
        .expect("root parent is legal");
        assert!(ok.iter().any(|a| a == "--parent="), "{ok:?}");
    }

    #[test]
    fn optional_owner_is_not_silently_dropped_when_mistyped() {
        for owner in [json!(123), Value::Null] {
            let err = argv_for("repo_owners", &json!({"owner": owner}), &opts(false))
                .expect_err("mistyped owner");
            assert!(matches!(err, ToolError::InvalidParams(_)), "{err:?}");
        }
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

    #[test]
    fn resource_uri_parses_the_wire_shape() {
        assert_eq!(
            ResourceUri::parse("walgit://refs/acme/repo").expect("refs"),
            ResourceUri::Refs {
                owner: "acme".into(),
                repo: "repo".into()
            }
        );
        assert_eq!(
            ResourceUri::parse("walgit://wal/acme/repo?from=42").expect("wal"),
            ResourceUri::Wal {
                owner: "acme".into(),
                repo: "repo".into(),
                from: 42
            }
        );
        assert!(matches!(
            ResourceUri::parse("walgit://wal/acme/repo?from=nope"),
            Err(ResourceError::InvalidParams(_))
        ));
    }

    #[test]
    fn version_tracker_only_reports_changes() {
        let mut tracker = VersionTracker::new("v1".into());
        assert!(!tracker.changed("v1"));
        assert!(tracker.changed("v2"));
        assert!(!tracker.changed("v2"));
        assert!(tracker.changed("v1"));
    }

    #[test]
    fn version_hash_is_stable_and_content_sensitive() {
        assert_eq!(super::sha256_hex(b"same"), super::sha256_hex(b"same"));
        assert_ne!(super::sha256_hex(b"same"), super::sha256_hex(b"different"));
    }

    #[test]
    fn failure_notification_is_a_logging_message_with_the_uri_and_error() {
        let message = super::subscription_failure_notification("walgit://refs/o/r", "probe failed");
        let value: Value = serde_json::from_str(&message).expect("json");
        assert_eq!(value["method"], "notifications/message");
        assert_eq!(value["params"]["level"], "error");
        assert_eq!(value["params"]["data"]["uri"], "walgit://refs/o/r");
        assert_eq!(value["params"]["data"]["error"], "probe failed");
    }

    #[test]
    fn a_valid_uri_with_a_missing_target_maps_to_resource_not_found() {
        let parsed =
            ResourceUri::parse("walgit://collab/thread/o/r/missing").expect("valid resource uri");
        assert!(matches!(parsed, ResourceUri::Thread { .. }));
        let error = ResourceError::NotFound("no entries for thread missing".into());
        assert_eq!(error.code(), -32002, "{error:?}");
        assert!(error.message().contains("missing"));
    }

    #[tokio::test]
    async fn missing_repo_identity_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut options = opts(false);
        options.repo = dir.path().to_path_buf();
        let error = super::ensure_identity(&options, "o", "r", None)
            .await
            .expect_err("no origin must be rejected");
        assert!(
            matches!(error, ResourceError::InvalidParams(_)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn refs_probe_streams_more_than_64_kib() {
        use std::io::Write as _;
        use std::process::Stdio;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        git(&["init", "-b", "main", repo.to_str().expect("utf8 path")]);
        git(&[
            "-C",
            repo.to_str().expect("utf8 path"),
            "config",
            "user.name",
            "t",
        ]);
        git(&[
            "-C",
            repo.to_str().expect("utf8 path"),
            "config",
            "user.email",
            "t@e",
        ]);
        std::fs::write(repo.join("x"), "x").expect("write");
        git(&["-C", repo.to_str().expect("utf8 path"), "add", "x"]);
        git(&["-C", repo.to_str().expect("utf8 path"), "commit", "-m", "x"]);
        let oid = git(&["-C", repo.to_str().expect("utf8 path"), "rev-parse", "HEAD"]);
        let oid = oid.trim();
        let mut stdin = String::new();
        for index in 0..2000 {
            stdin.push_str(&format!("create refs/heads/r{index} {oid}\n"));
        }
        let mut child = std::process::Command::new("git")
            .args([
                "-C",
                repo.to_str().expect("utf8 path"),
                "update-ref",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn git update-ref");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write refs");
        assert!(child.wait().expect("wait").success());

        let args = vec![
            "-C".into(),
            repo.display().to_string(),
            "ls-remote".into(),
            "--refs".into(),
            repo.display().to_string(),
        ];
        let probe = run_git_refs_streaming(&args, None)
            .await
            .expect("streaming refs probe");
        assert!(
            probe.count > 1400,
            "refs output was not large enough: {probe:?}"
        );
        assert_eq!(probe.version.len(), 64);
        assert!(probe.truncated || probe.lines.len() < probe.count as usize);
    }

    #[test]
    fn subscription_options_reject_bad_limits_and_backoff_is_bounded() {
        let mut o = opts(false);
        o.subscribe_interval_ms = 999;
        assert!(matches!(
            validate_subscription_options(&o),
            Err(ResourceError::InvalidParams(message)) if message.contains("subscribe-interval-ms")
        ));

        o.subscribe_interval_ms = 1000;
        o.max_subscriptions = 0;
        assert!(matches!(
            validate_subscription_options(&o),
            Err(ResourceError::InvalidParams(message)) if message.contains("max-subscriptions")
        ));

        assert_eq!(subscription_delay_ms(1000, 0), 1000);
        assert_eq!(subscription_delay_ms(1000, 1), 2000);
        assert_eq!(subscription_delay_ms(1000, 2), 4000);
        assert_eq!(subscription_delay_ms(1000, 99), 60_000);
    }

    #[tokio::test]
    async fn subscription_count_over_the_limit_is_rejected() {
        let (tx, _rx) = mpsc::channel(1);
        let mut o = opts(false);
        o.max_subscriptions = 1;
        let session = Session::new(o, tx);
        let (cancel, mut cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = cancel_rx.changed().await;
        });
        session
            .subscriptions
            .lock()
            .insert("walgit://refs/o/held".into(), Subscription { cancel, task });

        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/subscribe",
            "params": {"uri": "walgit://refs/o/new"}
        })
        .to_string();
        let reply: Value =
            serde_json::from_str(&session.handle_line(&line).await.expect("reply")).expect("json");
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("max-subscriptions"),
            "{reply}"
        );
        session.shutdown().await;
    }
}
