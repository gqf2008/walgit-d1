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

## 2. What this host stores (read from config, never hardcode)

`walgit.toml` (`~/.walgit/walgit.toml` by default) is the single source of truth: `[server]` (listen,
auth mode, roles), `[store]` / `[store.<backend>]` (bucket, prefix, endpoint, credential env var
names), `[cache]`, `[maintenance]`, `[bundles]`, `[compaction]`. Check it with:

```bash
walgit config --config ~/.walgit/walgit.toml check   # and: dump
walgit repo list                                     # repos visible in the configured store
```

Credentials come from the env vars the config names (e.g. `R2_ACCESS_KEY` / `R2_SECRET_KEY`) or the
installer-managed credentials file; never print them.

## 3. D1 collaboration bookkeeping (`walgit collab …`)

Issues, PRs, reviews, status and the board are **append-only signed entries** in `refs/collab/*`; the
Web UI, `walgit collab` views and the board are deterministic projections. Work done without entries
leaves no collaboration record.

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
| `review` | `{"decision":"approve\|request_changes\|comment","agent","note"}` | independent review |
| `merge_result` | `{"oid","merged":true,"note"}` | merge record (the board keys on `merged:true`) |
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
`patch` → `status: needs-review` → independent `review` → merge locally & push → `merge_result` →
`status: closed`. Keep the board and the thread as the single record; never edit state files by hand.

## 4. Listening for events (pull, never push)

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

## 5. Decentralized CI

The server holds no CI logic: `.walgit/ci.toml` in the tested commit declares tasks; a runner claims
them with signed `ci_claim` entries and publishes signed results into `ci-*` threads
(`walgit ci validate`, `walgit ci run --once`, `walgit ci status`).

## 6. Known pitfalls

- Keep the binary path free of non-UTF-8 characters (`env::args()` panics).
- Long-running watchers (mirror loops, `collab watch`) need `screen`/`nohup`: the service lifecycle does
  not own them, and a service restart can orphan them.
- A stale local `objects/pack/multi-pack-index` can make pushes fail with a misleading
  `connectivity: … object could not be found`; verify the object with `git cat-file`/`verify-pack`, then
  stop the service and rebuild (or move aside) the index before retrying.
- `[events]` (or `roles = ["events"]`) in `walgit.toml` is a hard parse error since the D46 removal.
