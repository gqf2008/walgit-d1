# SKILL.md — working with this walgit host as an AI agent

walgit is a git smart-HTTP server whose repositories live in an object store:
hosts are disposable caches, the bucket is the repository. Collaboration
(issues, PRs, boards) and CI are **inside the repository** — signed entries on
`refs/collab/*` — and the `walgit` CLI is their primary interface. This file
teaches an AI agent to discover, read, and write repositories on **this host**
with plain git, the CLI, and HTTP. No interactive steps anywhere.

## Install the walgit ops skill (operator guide)

This file teaches an agent to *use* the host. To also *operate* it — service
lifecycle, D1 collaboration bookkeeping, pulling events — install the ops skill
this build ships:

```sh
curl -fsSL '<host>/services/public/skill/install.sh' | sh
```

The installer downloads `<host>/services/public/skill/SKILL.md`, verifies it
against the sha256 baked into the script (the same value
`<host>/services/public/skill/manifest.json` reports) and writes
`${WALGIT_SKILL_DIR:-$HOME/.agents/skills/walgit}/SKILL.md`. Re-run it after a
host upgrade to refresh to that build; it is a no-op when the installed file
already matches.

On a host presenting a **self-signed** certificate, pin it first and add
`--cacert` — `curl -fsSk '<host>/services/public/ca.pem' -o walgit-ca.pem`, then
`curl -fsSL --cacert walgit-ca.pem '<host>/services/public/skill/install.sh' |
sh`. `curl -k` without pinning is trust-on-first-use: it keeps you going, but the
sha256 then protects only the `SKILL.md` download, not the installer itself.

## MCP (optional, client-side)

Some hosts would rather call tools than shell out. `walgit mcp` serves the Model Context Protocol
on stdio — host-spawned, so configure it as a command, not a URL:

```jsonc
{ "command": "walgit", "args": ["mcp"] }                       // read-only
{ "command": "walgit", "args": ["mcp", "--allow-write", "--key", "/path/to/key.ed25519"] }
```

Its tools **are** this CLI, so they cannot drift from it — but the surface is deliberately partial:
today `repo_list/owners/refs/resolve/tree/blob/commits/diff`, `collab_ls/thread/pr/board/report`,
`ci_status` and `wal_ls`; everything else (`repo info/blame/commit`, `ci log/artifacts`,
`collab watch`, …) still goes through the CLI directly. `collab_entry` is the only writing tool and
exists only with `--allow-write`. Destructive operations (GC, compaction, import, settings/policy
writes, ref deletion) are not exposed at all — and bytes never travel over MCP: clone/fetch/push
stay git + bundle-uri.

MCP resources expose read-only, `walgit://`-addressed views for the configured `--repo` checkout:
`walgit://refs/<owner>/<repo>`, `walgit://wal/<owner>/<repo>?from=<seq>`,
`walgit://collab/board/<owner>/<repo>`, and
`walgit://collab/thread/<owner>/<repo>/<thread-id>`. `resources/read` returns the content plus a
stable `_meta.version` (refs digest, WAL head seq, board hash, or thread head oid). Subscriptions
are **adapter-side polling, not server push** (D46): each is `per-instance` and **best-effort**, the
poller asks only for a cheap version probe, and the client decides whether to call `resources/read`
after an update. Refs are hashed as a stream and returned as a bounded summary; WAL probes read
only the refs-level manifest head; collab resources share one session-level `refs/collab/*` fetch
and only rebuild the board hash or thread head when that ref signal changes. Start with:

```sh
walgit --config ~/.walgit/walgit.toml mcp --repo /path/to/checkout \
  --subscribe-interval-ms 5000 --max-subscriptions 32
```

```jsonc
{"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}
{"jsonrpc":"2.0","id":2,"method":"resources/read",
 "params":{"uri":"walgit://refs/acme/repo"}}
{"jsonrpc":"2.0","id":3,"method":"resources/subscribe",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
{"jsonrpc":"2.0","id":4,"method":"resources/unsubscribe",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
```

An observed version change arrives as
`{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"…"}}`; a vanished
resource arrives as `notifications/resources/list_changed`. Notifications can be missed on process
restart or instance change, so a durable sidecar must keep its own cursor and use the pull lanes
(`git ls-remote`, `walgit wal ls`, `walgit collab watch`) as the source of truth.

## Host upgrades (tray)

The tray checks for updates **30 seconds after startup and every 30 minutes**. A Windows
installation made by the installer installs a detected release **by itself** — 90 s after
detection the helper stops the service, installs, health-checks the new version and relaunches
the tray — while macOS and source checkouts only notify and wait for the tray menu.
`WALGIT_AUTO_UPGRADE=0` pins a machine to manual, and a version whose unattended install rolled
back is never auto-retried (the menu still offers it).

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

## Host operations

`walgit serve` runs a standalone host; `walgit service start|stop|status|restart` manages the
installed service. Validate or print the effective configuration with `walgit config check` /
`walgit config dump`. Cross-repository principals are managed with `walgit principal …`. The
installed ops skill (`skills/walgit/SKILL.md`, published through the public installer) has the
full operator lifecycle and maintenance runbooks. On Windows the installer registers its install
directory on the **user** PATH (HKCU `Environment`, removed again at uninstall), so a shell started after
the install finds `walgit`; one that was already running (or a child of it) needs a restart.

## Discover

- `GET /api/v1` — the discovery document (lanes, endpoint list).
- `GET /api/v1/owners` → `["owner", …]`; `GET /api/v1/owners/{owner}/repos` →
  `["repo", …]` (or `walgit repo list`).
- `GET /repos.js` — the browser SDK (`window.repos`; `/repos.mjs` for ESM).
- Web UI: `/repos` (host-wide repository index), `/{owner}/{repo}` (browser).

## Git: clone / fetch / push

- Remote URL: `http(s)://<this-host>/<owner>/<repo>.git`.
- Auth modes: `none` (loopback only, everyone is anon), `token`, `oidc`. On a
  token/oidc host every request carries `Authorization: Bearer <token>`; the
  one-command client setup is in `/services/setup.json`, or run the installer:
  `sh -c "$(curl -fsSLk '<host>/services/public/install.sh')" -- <owner>/<repo>`.
- Big repositories advertise **bundle-uri**: clone with
  `-c transfer.bundleURI=true` (bytes come from static bundles, not
  upload-pack). Bounded fetches (CI checkouts with `--depth`/`--filter`) may
  pass `-c transfer.bundleURI=false`. Blobless: `--filter=blob:none`.

## The CLI: collaboration, CI, management (`walgit …`)

One binary, subcommands by role. Everything below works against a **checkout**
of a repository on this host (clone it first); entries are Ed25519-signed and
pushed as ordinary refs, so a local write becomes visible with one `--push`.

### D1 collaboration — issues, PRs, the board (`walgit collab …`)

- Read (JSON out — pipe through `jq`):
  - `walgit collab ls` — thread ids on `refs/collab/inbox/*`.
  - `walgit collab thread <id>` — one thread, parent-ordered, per-entry
    signature verification.
  - `walgit collab pr <id>` — aggregated PR view + merge-rule evaluation.
  - `walgit collab board` — the work-unit board: threads projected under
    `.walgit/board.toml`. Read-only — moving a card is a signed `status`
    entry, not an edit.
  - `walgit collab report` — global dashboard: threads, PR status,
    verification health, activity.
- Write (construct + sign + deliver):
  - Before a parallel workstream: register the whole team, one principal per
    agent (see §0b); `walgit collab principal-register` publishes a public key.
  - `walgit collab entry --kind <kind> --id <thread> --actor <principal>
    --body '<json>' --key <keyfile> [--base … --head …] --push <remote>
    [--auto-fold --fold-threshold <n>]`
    — appends a signed entry and pushes it. Nobody edits state; a change is
    a new signed entry whose parent chain anyone can replay and verify.
    `--auto-fold` (threshold default 10000) folds the inbox with the same
    actor/key once the unfolded refs pass the threshold — the server's
    aggregate read budget is 20000 refs.
- Automate: `walgit collab watch` — the resident loop that turns `refs/collab/*`
  changes into process invocations (`--exec <cmd>`, each new/changed entry's JSON
  on stdin). How an agent builds an unattended loop on it: **§0c**.
- Housekeeping (D45): `walgit collab gc --actor <principal> --key <keyfile>
  --push <remote> [--truncate]` folds the append-only inbox into the signed snapshot at
  `refs/collab/meta/snapshot` and prunes the folded refs — every aggregation
  (snapshot ∪ tail) is byte-identical across a fold, so run it whenever the
  inbox grows large; it is idempotent and safe to re-run. A fold over the
  64 MiB snapshot cap is refused unless `--truncate` drops the oldest records
  and marks the snapshot `complete:false`; `--truncate` also repairs an
  already over-cap snapshot when there is nothing new to fold.

### Decentralized CI (`walgit ci …`)

The server holds no CI logic. A runner is a client:

- `.walgit/ci.toml` in the tested commit declares the tasks;
  `walgit ci validate` checks it.
- `walgit ci run` — subscribe to ref tips, claim runs with signed `ci_claim`
  entries, execute, publish signed results. Simultaneous claims are a legal
  race: both may run, exactly one result is effective (deterministic winner
  rule), the others are kept for audit.
- `walgit ci status` — every run in the checkout's collab log, aggregated.

### Listening for events (pull, never push — D46)

The server does not push anything: the events bridge, the `[events]` config (and
`roles = ["events"]`), `events/cursor.json`, `POST /_events/notify` and
`walgit ci run --listen` were removed (D46 supersedes D32). The **events themselves
are the WAL entries** (`PUSH` / `REF_UPDATE` / `COMPACT` / `CHECKPOINT` / `SETTINGS`,
strictly increasing `seq`, replayable) — pick the cheapest lane that answers your
question, and keep your own cursor:

- **Ref tips only (cheapest, O(1), no pack)** — poll `git ls-remote <remote>` and
  diff against your previous snapshot. A CI runner does exactly this:
  `walgit ci run --repo . --remote origin --actor <principal> --key <keyfile> [--once]`
  (`--once` suits cron).
- **Collab entries (issues / PRs / reviews / `status` / CI)** —
  `walgit collab watch --remote origin --interval 10 --exec <cmd>`: each new or
  changed `refs/collab/*` entry's JSON arrives on your handler's stdin. State lives
  in `<gitdir>/collab-watch.json` (dedupe + resume); `--once` runs a single pass.
  After a D45 `collab gc` the watcher reports the folded snapshot
  (`kind=snapshot`) instead of the individual entries.
- **Full WAL stream (not just ref tips)** — `walgit wal ls <owner/repo>
  --from <seq> [--to <seq>]` enumerates the **retained** log entries. `--from` is
  inclusive, so advance your cursor to `seq + 1` after processing (or keep it as
  "next seq to read"). `wal ls` prints summaries (`seq`/`kind`/…); use
  `walgit wal show <owner/repo> <seq>` for `created_at` and a detailed entry view (note: `SETTINGS` payloads are not printed).
  Entries folded into a checkpoint and reclaimed by WAL GC are no longer listed —
  use the checkpoint or `walgit wal materialize <owner/repo> --at-seq <seq> --out <dir>` for that history.
- **Web narration (best effort)** — any JSON endpoint that cannot answer
  immediately streams the **SSE envelope** when the request says
  `Accept: text/event-stream` (read `progress` / `notice` / terminal
  `result`|`error`); a long task's live packets are at
  `GET /{owner}/{repo}/api/tasks/{id}`. Task packets are **per instance**: an
  unknown id returns 404 and a stream that ends without a terminal packet is an
  error — treat SSE as live narration, not a durable subscription.

Delivery: the pull lanes above (`git ls-remote`, `collab watch`, and `wal ls` for
entries still in the retention window) are **at-least-once** — be idempotent and
dedupe by `seq` (WAL) or thread/ref + entry oid (collab); duplicates are possible,
but a fact you can still read is never lost. SSE can drop packets (connection loss,
instance change), so a lossless sidecar must forward from a pull lane, never from
SSE. If you need push semantics — a webhook, IM or queue — add that sidecar: poll
or walk one of the pull lanes above and forward, keeping the cursor on your side.

### Repository reads over HTTP (no bucket credentials)

`walgit repo` reads a **running host** — any machine, no `walgit.toml`, no
bucket access; a host URL and (on token/oidc hosts) a bearer suffice:

- Host: `--url` > `$WALGIT_URL` > `http://127.0.0.1:8080`;
  token: `--token` > `$WALGIT_TOKEN`.
- Discovery: `walgit repo owners` (every owner) / `walgit repo owners <owner>`
  (that owner's repositories).
- `walgit repo refs <owner/name> [branches|tags|all|collab]` — head summary
  or one paged namespace; `walgit repo ref <owner/name> <full-ref-name>` —
  one ref by name.
- `walgit repo resolve <owner/name> <rev>` — revision → oid.
- `walgit repo tree <owner/name> <rev> [path]` / `blob … <path> [--raw]` —
  directory listing; blob as JSON envelope or raw bytes.
- `walgit repo commits <owner/name> [--ref --n --skip --path]` /
  `walgit repo commit <owner/name> <sha>` — history and one commit.
- `walgit repo merge-base <owner/name> <from> <to>` (`null` = unrelated) /
  `walgit repo diff <owner/name> <from> <to> [--format]`.
- `walgit repo blame <owner/name> <rev> <path>`.
- `walgit repo archive <owner/name> <rev> [--format] [--out FILE]` — the
  revision as an archive (default `tar.gz`; `--out` writes a file, default
  streams bytes to stdout).
- `walgit repo overview <owner/name>` — head seq, pack set, health.
- `walgit repo tasks <owner/name> [--follow <id>]` / `walgit repo ops
  <owner/name>` / `walgit repo op-start <owner/name> <op> [--arg k=v]` —
  task list, the available operations, and starting one with its live
  packet stream followed to the terminal result.
- Output is pretty JSON; HTTP ≥ 400 (including 401) is a diagnostic error.
  This covers the whole repository read + operations surface; what stays
  HTTP-only is the browser/SDK lane and the admin writes (`settings`/
  `policy` PUT/DELETE — the CLI's own `repo settings|policy` commands are
  the bucket-direct maintainer form).

### Repository management & ops

- `walgit repo create|list|info` — manage repositories (bucket-direct:
  needs the host config; the reads above do not).
- `walgit repo policy` — per-repo push rules (`policy.json`).
- `walgit repo settings` — per-repo TOML overrides (`[bundles]`,
  `[maintenance]`, `[compaction]`) published through the WAL.
- `walgit wal ls|show|materialize` — provenance: every push, repack,
  checkpoint is a log entry you can read and replay.
- `walgit mirror` — follow another git host's refs into walgit.
- `walgit import` — import an existing repository; `walgit compact`,
  `walgit bundle` — maintainer operations (compaction, static bundles).

## HTTP API

- Repository-scoped: `/{owner}/{repo}/api/…` — `refs`, `resolve`, `tree`,
  `blob`, `commits`, `commit`, `overview`, `tasks`, `settings`, `collab/*`.
  Credential: a bearer token or the same-origin session cookie.
- Cross-origin browser lane: `/{owner}/{repo}/api-browser/…`
  (`credentials: "include"`; CORS only for the host's configured origins).
- Any JSON endpoint that cannot answer immediately streams the **SSE
  envelope** when the request sends `Accept: text/event-stream` — read the
  stream (`progress` / `notice` / terminal `result`|`error`), never poll
  blindly.
- Long work is a **task**: `GET /{owner}/{repo}/api/tasks`, live packet
  stream at `…/tasks/{id}`.
- `503` + `Retry-After: n` means this host does not serve that repository's
  object work right now — retry after the advertised seconds; refs-level
  reads stay available on every host.

## Writing (push)

- Push with git over the same remote URL. Refs are linearized by a manifest
  compare-and-swap; a moved ref is rejected per-ref with its actual old value.
- A per-repo **push policy** may protect refs; an empty/missing policy means
  anyone with write may move any ref.

## LFS

- Batch + basic transfer under `/{owner}/{repo}.git/info/lfs`; objects are
  sha256-addressed, immutable.

## Agent collaboration standard (normative)

Any job that needs more than one agent — code, content, research, ops — runs on this host
as **signed threads on `refs/collab/*`**. Agents never collaborate out-of-band (no private
chat, no shared scratch files as the source of truth): the thread is the only shared memory,
everyone re-derives the same view from the refs.

### 0. Identity — register the team before the work

- One principal per collaborator, one Ed25519 key (`32` raw bytes as hex). The key is
  **project-local**: `walgit collab join --principal <name>` materializes it at
  `<git-common-dir>/walgit/keys/<name>.ed25519` (`0600`) next to the repository, records
  the identity at `<git-common-dir>/walgit/identity`, and registers
  `refs/collab/meta/principals/<name>`. Each collaborator owns its key: never pile several
  principals' private keys in one directory, and never share a key across agents or roles.
- The git **common dir** (never the work tree) is where this state lives: every worktree of
  the clone shares it, it is never tracked, and `git clean -fdx` cannot remove it. One
  clone therefore carries **one** project identity; run a separate clone per principal, or
  pass `--principal`/`--key` explicitly to act as another one.
- `--key`, when given, always takes a **file path** (never paste the seed contents into the
  command line or shell history). `collab join` generates a fresh seed when none exists and
  adopts an existing one when `--key` points at it.
- Before the first thread, register the whole team, not one lone identity: a worker
  pool, a reviewer pool, and a coordinator. A practical default is
  `<proj>-worker-1..N`, `<proj>-reviewer-1..N`, and `<proj>-coordinator`. Each
  collaborator runs only its own line, in its own clone (the key stays inside that
  clone's `.git/`, so no shared key directory exists to get wrong):

  ```sh
  checkout=/path/to/checkout
  proj=my-project
  walgit collab join --repo "$checkout" --principal "${proj}-worker-1" --push origin
  # one clone + one `join` per principal (worker-2, reviewer-1, coordinator, …)
  ```

- Register once per repository. `--push origin` publishes the public key so other agents
  can verify it. A rejected registration is a hard stop: do not start writing entries
  under an unregistered principal.
- **Names are agent-side bookkeeping; walgit only manages keys.** Each agent picks and
  registers its own principal name; the registration stores exactly one fact —
  `principal → public key` (`refs/collab/meta/principals/<principal>`, `docs/D1_PROTOCOL.md`
  §4.3) — and signature verification checks only that binding. walgit maintains no roster
  of who *should* be on the team and does not gate names: the team list (who is in, what
  they are called) is maintained by the agents themselves, one self-registration each.
  Roster changes are new registrations / revocations by the agents, never edits to a
  central list.
- Reviewer principals must not start with `svc-`: `merge_rule_eval` excludes `svc-*`
  actors from human approvals, so such an approve cannot satisfy a protected-base
  merge rule.
- Never sign an entry for another agent or make every agent use one shared key. If the
  author, reviewer, and merger all sign as `sqb` (or any other shared principal), the
  board sees one owner and verification cannot tell who implemented, reviewed, or
  merged. That destroys independent review and the audit trail.
- Write only your own inbox (`refs/collab/inbox/<principal>/*`). Never borrow another principal's
  key. Read-side verification marks `actor != inbox` entries unverified — treat them as untrusted.

### 0a. First contact — the automatic routine every agent runs

An agent that opens this guide for a repo it may work on performs these steps on its
own, in order, before reading the board, claiming a card, or touching a file. The
routine is what keeps collaboration style and naming consistent without a central
roster:

1. **Discover the naming convention.** List the registered principals
   (`git for-each-ref refs/collab/meta/principals` after a fetch) to read the project's
   `<proj>` prefix and role pattern (`<proj>-worker-N`, `<proj>-reviewer-N`,
   `<proj>-coordinator`).
2. **Adopt the existing identity, or take the next free name.** If this checkout's
   `<git-common-dir>/walgit/identity` already names a principal registered in this
   repo, it is this agent's — keep it. Otherwise pick `<proj>-<role>-N` with the next
   free index after the highest registered one for that role. Never reuse another
   collaborator's name or key.
3. **Ensure the identity exists.**
   `walgit collab join --repo "$checkout" --principal <me> --push origin` generates the
   32-byte Ed25519 seed at `<git-common-dir>/walgit/keys/<me>.ed25519` (`0600`) when none
   exists, writes `<git-common-dir>/walgit/identity`, and registers the public key. A
   rejected registration is a hard stop: do not start writing entries under an
   unregistered principal.
4. **Sync the collaboration view.** Fetch the collab refs
   (`git fetch origin '+refs/collab/inbox/*:refs/collab/inbox/*'
   '+refs/collab/meta/*:refs/collab/meta/*'`) and read `walgit collab board`
   before filing or claiming anything; sign every entry with this principal and key.

### 0b. Parallelism — register a team, not a lone agent

Parallel work is a topology, not simply "use more agents." Start from this default:
**N active agents = N principals/keys = N cards (threads) = N worktrees/branches**.

- **One owner per card at a time.** The latest `status` entry's `owner`, `worktree`,
  and `branch` are the claim ledger; ownership may hand off only through a new signed
  `status` entry. Do not let two agents work the same card; split the work into a new
  thread instead.
- **Reviewers are different principals.** The `review` entry must be signed by a
  principal other than the patch author (and, for the final review, other than the
  merger). Treat the author's own `approve` as invalid: do not merge on it. The
  `review.body.agent` field is descriptive; the verified `actor` and key are the
  identities the coordinator must compare. The merge rule counts **distinct
  non-author** verified approvals (a patch author's own approve and duplicate
  approves never count), so the automated gate agrees with this paragraph.
- **The coordinator merges and archives.** After approval, the coordinator merges the
  branch locally and pushes the result. Record the merge with **one** entry, which also
  moves the card:
  1. `merge_result` with `{"merged":true,"oid":"<sha>","result":"merged","note":"..."}`;
  2. `status` with `{"status":"closed","owner":"<proj>-coordinator","worktree":"...","branch":"main","work":"..."}`.
- **Move cards only through signed entries.** Use `status` for normal moves; the
  projector also treats `merge_result {"merged":true}` as the terminal `merged` move.
  Never edit the board, a state file, or another agent's inbox to move a card.
- **Clean up immediately after closure.** Remove the card's worktree and delete its
  local branch only after the merge is recorded, then verify `git worktree list` no
  longer contains it.
- **Split by write set, not by title.** Parallelize work whose files, interfaces, and
  review surface do not overlap. Serialize changes to the same file, schema, or
  migration; otherwise two workers will create a conflict that review cannot resolve
  cheaply.

A common shape is **2–4 workers, 1–2 reviewers, and 1 coordinator**. Reviewers can be
pulled in per review; every active card still needs its own owner and worktree. The
coordinator should keep the smallest possible write set so the merge path stays easy to
replay.

**Copyable start checklist** (set `checkout=/path/to/checkout` first, replace the
remaining `<...>` values, and use the returned oid as the next `--parent`). Run each
role in its **own clone** after `walgit collab join --principal <role> --push origin`;
with a project identity in place, `--actor` and `--key` resolve from
`<git-common-dir>/walgit/identity`, so the commands below omit `--key`:

```sh
# 1. File the card (coordinator). The issue names the objective, roles, owner,
#    and machine-checkable acceptance.
walgit collab entry --repo "$checkout" --kind issue --id <thread> \
  --actor <proj>-coordinator \
  --body '{"title":"<title>","body":"objective; roles; machine-checkable acceptance"}' \
  --push origin

# 2. Claim it (worker). status fields are the claim ledger; use the issue entry oid as parent.
walgit collab entry --repo "$checkout" --kind status --id <thread> \
  --actor <proj>-worker-1 --parent <issue-oid> \
  --body '{"status":"in-progress","owner":"<proj>-worker-1","worktree":"wt-<thread>","branch":"feat/<thread>","work":"<one-line plan>"}' \
  --push origin

# 3. Work on exactly that card.
git -C "$checkout" worktree add "$checkout/.worktrees/wt-<thread>" -b feat/<thread> <base>
git -C "$checkout" push origin feat/<thread>

# 4. Attach the implementation, then ask for review (use the status entry oid as parent).
walgit collab entry --repo "$checkout" --kind patch --id <thread> \
  --actor <proj>-worker-1 --parent <status-oid> \
  --base refs/heads/main --head refs/heads/feat/<thread> \
  --body '{"title":"<patch title>","message":"<what changed and why>"}' \
  --push origin
walgit collab entry --repo "$checkout" --kind status --id <thread> \
  --actor <proj>-worker-1 --parent <patch-oid> \
  --body '{"status":"needs-review","owner":"<proj>-worker-1","worktree":"wt-<thread>","branch":"feat/<thread>","work":"ready for independent review"}' \
  --push origin

# 5. Review with a different principal (its own clone). Full findings go in note.
walgit collab entry --repo "$checkout" --kind review --id <thread> \
  --actor <proj>-reviewer-1 --parent <review-request-oid> \
  --body '{"decision":"approve","agent":"<proj>-reviewer-1","note":"location; problem; suggestion; reproducible verification"}' \
  --push origin

# 6. Coordinator only: merge, push, then record the merge (one entry records the oid
#    and moves the card to `merged`).
git -C "$checkout" switch main
git -C "$checkout" merge --ff-only feat/<thread>
git -C "$checkout" push origin main
walgit collab entry --repo "$checkout" --kind merge_result --id <thread> \
  --actor <proj>-coordinator --parent <review-oid> \
  --body '{"merged":true,"oid":"<merged-oid>","result":"merged","note":"merged feat/<thread> into main"}' \
  --push origin
walgit collab entry --repo "$checkout" --kind status --id <thread> \
  --actor <proj>-coordinator --parent <merged-entry-oid> \
  --body '{"status":"closed","owner":"<proj>-coordinator","worktree":"wt-<thread>","branch":"main","work":"merged and verified"}' \
  --push origin
```

The anti-pattern is a single identity serially doing the whole job, or several agents
sharing one key/knowing each other's keys to "make the board green." Both make the board
look productive while its rows cannot answer the only questions it exists to answer:
*who did this, who reviewed it, and who merged it?*

### 0c. Staying live — the watch loop (unattended operation)

Everything above assumes an agent that is already awake, and a turn-based agent is not: nothing
rouses it when a card moves. That last mile is yours to build, and the primitive is
`walgit collab watch` — a resident loop that turns `refs/collab/*` changes into process
invocations. It is a trigger, not an agent: walgit notifies and syncs, the decision logic is
whatever you pass to `--exec`.

```sh
walgit collab watch --repo "$checkout" --remote origin --interval 10 \
  --exec /path/to/watch-hook.sh [--state /path/to/collab-watch.json]
```

**The `--exec` contract** (`crates/walgit-cli/src/collab_cmd.rs::run_watch`/`run_exec`):

- Every pass fetches `refs/collab/inbox/*` + `refs/collab/meta/*` (never `ci-artifacts`),
  diffs `refname → oid` against the state file, and invokes the command **once per new or
  changed ref, in refname order** — N new entries are N invocations.
- The command runs as `sh -c '<cmd>'` with **stdout/stderr inherited** from the watcher, so
  redirect your own logging.
- The **raw entry blob is on stdin** (the signed entry JSON; for `meta/snapshot` the folded
  snapshot, for `meta/principals/<p>` the registration doc). Signals come from the parsed
  entry, never from rendered text, so a body carrying a literal `kind=` line cannot forge them.
- Five variables: `WALGIT_COLLAB_REF`, `WALGIT_COLLAB_KIND` (the entry's `kind` — the six
  lifecycle kinds, `ci_claim`/`ci_result`, or any custom kind; plus the watcher's own three:
  `principal` for `meta/principals/<p>`, `snapshot` for a folded `meta/snapshot`, `unknown`
  for an unparseable ref), `WALGIT_COLLAB_THREAD`, `WALGIT_COLLAB_ACTOR`,
  `WALGIT_COLLAB_VERIFIED`. `VERIFIED=true` on a `principal` event only means "a registration
  document was touched" — registration documents are unsigned.
- **A non-zero exit ends the watcher process**, not merely the pass: the error propagates out
  of the loop and the state file is never advanced. The at-least-once guarantee is therefore
  conditional — the batch is re-delivered the next time the watcher *runs*, which in the
  resident form means **after your supervisor restarts it** (`screen`/`nohup` will not; see
  "Who owns the loop"). Park the event durably and return 0; reserve a failure exit for "I
  could not park it at all". The event carries no oid — hash the stdin body
  (`git -C "$checkout" hash-object`, which reproduces the oid the watcher prints) if you want
  the content address; the ref name is unique per entry under `inbox/*` but *not* under
  `meta/principals/<p>`, which stays put across key rotations.
- State lives in `<gitdir>/collab-watch.json` (`--state` overrides it); keep it as the cursor.
  Ref deletion produces no event — the ref set simply follows the remote.
- Without `--once` the loop sleeps `--interval` seconds (default 10) between passes; that
  interval is your worst-case wake latency. `--once` is one pass and exit — the shape cron and
  systemd timers want, and the friendlier one on Windows.

**Three traps that decide whether the loop works.**

1. **Self-trigger.** Your own writes are new refs, so your `patch`/`status`/`comment` comes back
   at your handler like anyone else's. Filter on `WALGIT_COLLAB_ACTOR` against your own
   principal, and never let a write be the unconditional answer to an event.
2. **Long work inside the hook.** The loop is serial and blocking, and one failure takes the
   whole watcher down — a hook that runs the job stalls every other event and risks the batch.
   Park the event and return; do the work in a worker — the same split as `ci run`'s claim vs
   execute.
3. **Unverified input.** `WALGIT_COLLAB_VERIFIED=false` means the signature did not check out
   against the registered key. Do not act on it, and do not treat it as a card you own.

**The loop shape.** An event is a trigger to *re-read*, never a snapshot of truth: fetch the
thread (`walgit collab thread <id>`), confirm at the head oid that the card is still unowned or
yours, and claim it with a signed `status` entry — the claim is the mutex, and two agents
racing a card converge only because both re-read first. Then the ordinary lifecycle (§1–§5):
work in the card's worktree → `patch` → `status: needs-review` → independent `review` →
coordinator `merge_result` → `status: closed`. The card stays the record; your queue is your
own business.

**Who owns the loop.** One loop per principal, holding that principal's key — two loops signing
as one principal are as indistinguishable as two agents sharing a key (§0b). The loop is a
process you own, and the service lifecycle does not supervise it: a service restart can orphan
it and a hook failure exits it. Keep it under something that **restarts on exit** — launchd
`KeepAlive`, systemd `Restart=always`, or Windows Task Scheduler (with a GUI-subsystem
launcher, never a console program, D53) — or drop the resident loop and run `--once` from a
timer, which replays the failed batch on the next tick.

**Where the loop stops.** It may claim cards, work them, and post — its `review` counts only
when it signs as a principal other than the patch author (§4). It must not merge without a
verified non-author `approve`, must not silently clear a `needs-human` card (that is a human
decision), and must not turn its own progress into messages to channels it was not given. The
board is where it reports; notifying anyone outside it is a separate, explicitly authorized
decision.

**Copyable hook** — park the event, return 0, let the worker decide:

```sh
#!/bin/sh
# watch-hook.sh — idempotent and fast; the decision logic lives in the worker.
set -eu
queue="${WALGIT_WATCH_QUEUE:-$HOME/.walgit/watch-queue}"
checkout="${WALGIT_CHECKOUT:-$PWD}"            # the hook inherits the watcher's cwd
mkdir -p "$queue"
tmp="$(mktemp "$queue/.incoming.XXXXXX")"
cat >"$tmp"                                   # the raw entry JSON from stdin
key="$(git -C "$checkout" hash-object "$tmp")"   # a content address for the entry
if [ -e "$queue/$key.meta" ]; then rm -f "$tmp"; exit 0; fi
{
  printf 'ref=%s\n'      "$WALGIT_COLLAB_REF"
  printf 'kind=%s\n'     "$WALGIT_COLLAB_KIND"
  printf 'thread=%s\n'   "$WALGIT_COLLAB_THREAD"
  printf 'actor=%s\n'    "$WALGIT_COLLAB_ACTOR"
  printf 'verified=%s\n' "$WALGIT_COLLAB_VERIFIED"
} >"$queue/$key.meta"
mv "$tmp" "$queue/$key.entry"
```

The worker is yours to write — that is where the model, the tools and the decision live. It
drains the queue and runs the loop shape above for each event it chooses to act on; re-drains
are idempotent because the same `key` is the same entry. `walgit ci run` is this shape applied
to CI tasks, and is the worked example to copy from.

### 1. Work unit = one thread

- Open an `issue` entry with: objective, roles, one owner, and **machine-checkable acceptance**.
- Split large jobs into sub-threads; each sub-deliverable is its own thread. Reference other
  threads by entry oid in the body — `walgit collab entry --related <oid>` /
  `--depends-on <oid>` (the thread view reports `broken_refs` for unresolvable oids).
  Carry files with `--attach <file>` (`{filename, sha256, content_b64}` in the body).
- Keep the main thread for assembly/review; record which sub-thread oids the result came from.

### 2. Tree changes (when the unit changes code/content)

- Never edit a shared checkout. Clone from **this host** (no GitHub needed) and use one
  worktree + branch per unit: `git worktree add .worktrees/<unit> -b <unit> <base>`.
- Commit locally, then push the branch to this host: `git push <remote> <unit>`.
- Attach it to the thread as a `patch` entry with `--base` / `--head`; carry files
  with `--attach <file>` (`{filename, sha256, content_b64}` in the body); CI (if declared) runs on
  the branch tip. The diff is what reviewers read — review entries happen on the thread, not in chat.

### 3. Thread protocol

- **Read before write.** Fetch the thread (and repo refs) first; append with the latest entry
  oid as `--parent`. Never answer from memory/cache.
- Express state with kinds, not prose: `status` (`in-progress` / `needs-review` /
  `blocked` / `needs-human` / `done` / `closed`), `review` (`approve` /
  `request_changes` + note), `merge_result` (`merged: true` + oid), `comment` for
  claims/progress/questions.
- **Decide before parking.** `needs-human` is for what genuinely needs the human —
  authorization, priority, external input. A technical or product judgment the owner
  can make must be made, recorded in a `comment`, and carried out; use a decision aid
  where the host offers one. Parking a decidable question is a stall, not a status.
- Every `in-progress` / `needs-review` / `blocked` / `needs-human` status carries the supervision
  context: `owner`, `worktree`, `branch`, `work` (or `note`). Fields inherit across status moves;
  an explicit empty string clears them. Example:
  `{"status":"in-progress","owner":"agent-mendel","worktree":"prod-release","branch":"feat/prod-release","work":"fix release preflight"}`.
- Every meaningful step is an entry: claim, progress, result, question.

### 4. Review

- An independent agent (or human) — a **different principal** from the patch author and,
  for the final review, from the merger — reviews the diff/artifacts and posts **full
  findings** in the `review` entry — location, problem, suggestion — not a one-line
  conclusion. The author's own `approve` is not independent review and must not satisfy
  the review gate. The merge rule itself drops `svc-*` actors and the authors of the
  **verified** patch(es) from its countable approvals; because an unverified patch does
  not populate that author set, the coordinator still checks the actor before merging.
- Run the review as an independent party — an agent process or a human, each with its
  own principal/key — whose own commands run against the pinned commit; a headless
  sub-agent started for the review is the normal shape, not the exception. Evidence
  means what the reviewer ran and observed; the author's own test run is not verification.
- `request_changes` → the implementer fixes on the branch and replies on the thread mapping each
  point to what changed → reviewer re-reviews → `approve` only when satisfied.
- Treat "approve with no evidence" as noise; verification claims must be reproducible.

### 5. Merge & archive

- walgit alone is a complete collaboration platform; **GitHub (or any other host) is optional** and
  is only a mirror when a project already keeps one.
- After approval: merge locally (fast-forward preferred), push the result to this host. Push the
  same refs to a GitHub mirror only for projects that are dual-homed (walgit = fact source,
  GitHub = backup/public mirror) — never as a requirement of walgit itself.
- The coordinator merges locally and pushes the result. Write **one** `merge_result`
  `{"merged": true, "oid": "<sha>", "result": "merged"}` (it records the oid and moves the
  card); then move the card to `closed` with a `status` entry.
- Archive human-facing artifacts as files in the repository (reports under `docs/`), not only in
  thread bodies.

### 6. CI (optional)

- Declare tasks in `.walgit/ci.toml` **in the tested commit**; a runner
  (`walgit ci run --once`) claims, executes, and signs results back. Green before merge.

### 7. Discipline

- Prefer machine-readable fields over prose; keep one logical change per patch.
- Don't rewrite pushed branch history without saying so; never write to another inbox; never
  trust unverified entries (wait for registration or verification).
- Honor per-repo policy when set: it gates *who may write*; signatures gate *who signed*.

## Rules of thumb

- `404` is a cheap probe answer — probe keys, don't list.
- Immutable objects answer with `Cache-Control: immutable`, a strong `ETag`,
  and `Range` support; ref-dependent answers carry SWR + `ETag`.
- A push acknowledged by git is already visible to the next read on any host.
- When in doubt, read the rules, not the platform: every collaboration screen
  is the same signed entries computed through public rules — the CLI prints
  the same bytes any server computes.
