---
name: walgit
description: "Operate a walgit host (object-store-backed Git server) and do D1 collaboration bookkeeping (issues/PRs/reviews/board live in refs/collab/*). Use when the user mentions walgit, a local/self-hosted git host, pushing to/cloning from a walgit host, walgit issues/PRs/reviews/board, walgit service lifecycle, or listening to walgit events — commands walgit service start|stop|status|restart and walgit collab (--config <walgit.toml>)."
metadata:
  requires:
    bins: ["screen"]
  platform: "macOS / Linux / Windows"
---

# walgit — operate the host + D1 collaboration

walgit is a Git server whose only durable state is an object-store bucket (S3/R2/GCS/iobject): one process
serves smart HTTP, LFS, bundles and the web UI, local disk is a cache, there is no database and no
primary node. See the host's `/SKILL.md` for the *consumer* guide (clone recipes, HTTP API, collab lane);
this skill is the *operator* guide.

## 1. Service lifecycle (start with this)

```bash
walgit service start     # idempotent; prints "already running (pid …)" or "started"
walgit service status    # state + /healthz + port
walgit service stop
walgit service restart
```

The `walgit` CLI is a symlink to the installed binary (app bundle / install dir); user state lives in
`~/.walgit/` (`walgit.toml`, `cache/`, `keys/`, `server.log`). Never copy the binary into the state dir.

## 2. Host upgrades (tray)

The tray checks for updates **30 seconds after startup and every 30 minutes**; detection only
notifies, and installation always requires the user to click the tray menu.

- **macOS:** DMG channel — download the release DMG, verify SHA-256 and the signed/notarized app,
  replace the installed app, run the health check, and roll back on failure.
- **Windows:** installer channel — download `walgit-setup-<version>-x64.exe`, verify its exact
  SHA-256, then run the helper's silent install followed by `walgit.exe --version` and `/healthz`
  checks; failure rolls back. An installation created **before the first build containing this
  feature** must run the installer once manually before tray upgrades work.
- **Linux:** no packaged Release auto-update channel. If the tray runs with a configured source
  checkout (`WALGIT_REPO`, default `~/walgit-repo`), it offers **source upgrade** (ff-merge,
  build, health check, rollback); otherwise upgrade through the package/service workflow used to
  install the host.

## 3. MCP subscriptions (optional)

`walgit mcp` is a client-side stdio adapter. A minimal host-spawned configuration is:

```sh
walgit --config ~/.walgit/walgit.toml mcp --repo /path/to/checkout \
  --subscribe-interval-ms 5000 --max-subscriptions 32
```

Subscribe and read over JSON-RPC (newline-delimited on stdio):

```jsonc
{"jsonrpc":"2.0","id":1,"method":"resources/subscribe",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
{"jsonrpc":"2.0","id":2,"method":"resources/read",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
{"jsonrpc":"2.0","id":3,"method":"resources/unsubscribe",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
```

Subscriptions are client-side polling, **per-instance** and **best-effort** (D52), rather than a
server push channel. Version changes arrive as `notifications/resources/updated` with the resource
`uri`; disappearance arrives as `notifications/resources/list_changed`. `--subscribe-interval-ms`
defaults to `5000` and must be at least `1000`; `--max-subscriptions` defaults to `32` and excess
subscriptions are rejected. The four URI forms and the full `resources/list|read|subscribe|unsubscribe`
semantics are documented in the public host `/SKILL.md` under **MCP (optional, client-side)**; use
that as the single detailed reference.

## 4. What this host stores (read from config, never hardcode)

`walgit.toml` (`~/.walgit/walgit.toml` by default) is the single source of truth: `[server]` (listen,
auth mode, roles), `[store]` / `[store.<backend>]` (bucket, prefix, endpoint, credential env var
names), `[cache]`, `[maintenance]`, `[bundles]`, `[compaction]`. Check it with:

```bash
walgit config --config ~/.walgit/walgit.toml check   # and: dump
walgit repo list                                     # repos visible in the configured store
```

Credentials come from the env vars the config names (e.g. `R2_ACCESS_KEY` / `R2_SECRET_KEY`) or the
installer-managed credentials file; never print them.

## 5. D1 collaboration bookkeeping (`walgit collab …`)

Issues, PRs, reviews, status and the board are **append-only signed entries** in `refs/collab/*`; the
Web UI, `walgit collab` views and the board are deterministic projections. Work done without entries
leaves no collaboration record.

**Parallel team first.** For parallel work, register one principal + Ed25519 key per
agent *before* opening threads — a worker pool (`<proj>-worker-1..N`), a reviewer pool
(`<proj>-reviewer-1..N`), and a coordinator (`<proj>-coordinator`). N active agents = N
cards = N worktrees/branches; one card has one owner; the reviewer must sign with a
different principal than the author and reject a self-approve (the merge rule counts
verified approvals but does not infer authorship); the coordinator performs the merge. Never let
multiple agents sign under one shared key (`sqb` or otherwise): the board and audit can
then no longer distinguish implementer, reviewer, and merger. See `/SKILL.md` §0b for
the full topology and copyable checklist.

```bash
walgit collab principal-register --repo <checkout> --principal <principal> \
  --key ~/.walgit/keys/<principal>.ed25519 --push origin
```

`--key` is a file path: generate each principal's 32-byte Ed25519 seed with your own
key-generation flow, store it `0600`, and never paste its contents on the command line.
Reviewer principals must not start with `svc-`; `merge_rule_eval` excludes `svc-*`
actors from human approvals.

```bash
# A function, not `W="walgit …"; $W …` — zsh does not word-split an unquoted expansion.
W() { walgit --config ~/.walgit/walgit.toml "$@"; }
W collab ls                     # thread ids
W collab board                  # work-unit board (.walgit/board.toml; read-only projection)
W collab report                 # threads / PRs / verification / activity
W collab thread <id>            # parent-ordered, per-entry signature verification
W collab pr <id>                # aggregated PR view + merge-rule evaluation
```

Write entries (one signed entry per push; the printed second column is the entry oid that becomes the
next `--parent`):

```bash
W collab entry --kind <issue|comment|patch|review|merge_result|status> \
  --id <thread-id> --actor <principal> --parent <oid|""> \
  --body '<json>' --key ~/.walgit/keys/<principal>.ed25519 --push origin \
  [--base refs/heads/main --head refs/heads/<branch>]      # patch only
```

| kind | body (required) | use |
|---|---|---|
| `issue` | `{"title","body"}` | thread root |
| `status` | `{"status","owner","worktree?","branch?","work","note?"}` | claim / move the card |
| `patch` | `{"title","message"}` + `--base/--head` | implementation branch |
| `review` | `{"decision":"approve\|request_changes\|comment","agent","note"}` | independent review; actor must differ from the author (coordinator-enforced) |
| `merge_result` | `{"merged":true,"oid":…,"result":"merged","note":…}` | merge record, **one entry** (the board/PR state keys on `merged:true`) |
| `comment` | `{"note"}` | progress notes (does not move the card) |

**Board projection rules** (`.walgit/board.toml` defines the columns): `status` is the newest `status`
entry carrying a string `status` (`merge_result {"merged":true}` also lands on `merged`); `owner` /
`worktree` / `branch` / `work` each come from the newest `status` entry that names that field — later
entries inherit what they omit, an explicit empty string clears it, `work` falls back to that entry's
`note`. Projection does **not** filter by signature; verification shows in the `verified`/`unverified`
counts, in the merge rule (only verified approvals count) and in the `done` gate. An issue-only thread
therefore projects as an **unowned `open` card** — file the `issue` **and** a `status` entry naming
`owner`; parked work still names its owner (`blocked` / `needs-human`); an owner in a `comment` does
not count.

Standard flow: `issue` → `status: in-progress` (owner/worktree/branch) → work in a worktree →
`patch` → `status: needs-review` → independent `review` by another principal → coordinator merges
locally & pushes → `merge_result {"merged":true,"oid":…}` → `status: closed`.
Remove the worktree after closure. Keep the board and the thread as the single record; never edit
state files by hand.

## 6. Listening for events (pull, never push)

The event source is the **WAL** (`PUSH` / `REF_UPDATE` / `COMPACT` / `CHECKPOINT` / `SETTINGS`, strictly
increasing `seq`, replayable). The server does not push: the events bridge, `[events]` config,
`roles = ["events"]`, `events/cursor.json`, `POST /_events/notify` and `walgit ci run --listen` were
removed (D46). Pick the cheapest lane and keep your own cursor:

- **Ref tips only** — poll `git ls-remote <remote>` and diff snapshots; CI runners trigger this way
  (`walgit ci run --repo . --remote origin --actor <principal> --key <keyfile> [--once]`).
- **Collab entries** — `walgit collab watch --remote origin --interval 10 --exec <cmd>`: each new/changed
  entry's JSON goes to the handler's stdin; state file `<gitdir>/collab-watch.json`; `--once` for cron;
  after a D45 `collab gc` the folded snapshot is reported as `kind=snapshot`.
- **Full WAL stream** — `walgit wal ls <owner/repo> --from <seq> [--to <seq>]` enumerates retained
  entries (`--from` is **inclusive**: advance the cursor to `seq + 1`); `wal ls` prints summaries, use
  `walgit wal show <owner/repo> <seq>` for `created_at` and details; folded+GC'd history needs the
  checkpoint or `walgit wal materialize <owner/repo> --at-seq <seq> --out <dir>`.
- **Web** — JSON endpoints that cannot answer immediately stream the SSE envelope (`text/event-stream`);
  task packets are per instance (unknown id 404, no terminal packet = error) — live narration, not a
  durable subscription.

The pull lanes are **at-least-once**: be idempotent and dedupe by `seq` (WAL) or thread/ref + entry oid
(collab). Duplicates are possible; a fact you can still read is never lost. For push semantics
(webhook/IM/queue), add a sidecar that forwards from a pull lane — never from SSE.

## 7. Decentralized CI

The server holds no CI logic: `.walgit/ci.toml` in the tested commit declares tasks; a runner claims
them with signed `ci_claim` entries and publishes signed results into `ci-*` threads
(`walgit ci validate`, `walgit ci run --once`, `walgit ci status`).

## 8. Known pitfalls

- Keep the binary path free of non-UTF-8 characters (`env::args()` panics).
- Long-running watchers (mirror loops, `collab watch`) need `screen`/`nohup`: the service lifecycle does
  not own them, and a service restart can orphan them.
- A stale local `objects/pack/multi-pack-index` can make pushes fail with a misleading
  `connectivity: … object could not be found`; verify the object with `git cat-file`/`verify-pack`, then
  stop the service and rebuild (or move aside) the index before retrying.
- `[events]` (or `roles = ["events"]`) in `walgit.toml` is a hard parse error since the D46 removal.
