# SKILL.md — working with this walgit host as an AI agent

walgit is a git smart-HTTP server whose repositories live in an object store:
hosts are disposable caches, the bucket is the repository. Collaboration
(issues, PRs, boards) and CI are **inside the repository** — signed entries on
`refs/collab/*` — and the `walgit` CLI is their primary interface. This file
teaches an AI agent to discover, read, and write repositories on **this host**
with plain git, the CLI, and HTTP. No interactive steps anywhere.

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
  - First use only: `walgit collab principal-register` — publish your
    principal's public key.
  - `walgit collab entry --kind <kind> --id <thread> --actor <principal>
    --body '<json>' --key <ed25519-hex> [--base … --head …] --push <remote>`
    — appends a signed entry and pushes it. Nobody edits state; a change is
    a new signed entry whose parent chain anyone can replay and verify.
- Automate: `walgit collab watch --exec <cmd>` — resident loop: fetch
  `refs/collab/*`, invoke `cmd` with each new/changed entry's JSON on stdin.
- Housekeeping (D45): `walgit collab gc --actor <principal> --key <key>
  --push <remote>` folds the append-only inbox into the signed snapshot at
  `refs/collab/meta/snapshot` and prunes the folded refs — every aggregation
  (snapshot ∪ tail) is byte-identical across a fold, so run it whenever the
  inbox grows large; it is idempotent and safe to re-run.

### Decentralized CI (`walgit ci …`)

The server holds no CI logic. A runner is a client:

- `.walgit/ci.toml` in the tested commit declares the tasks;
  `walgit ci validate` checks it.
- `walgit ci run` — subscribe to ref tips, claim runs with signed `ci_claim`
  entries, execute, publish signed results. Simultaneous claims are a legal
  race: both may run, exactly one result is effective (deterministic winner
  rule), the others are kept for audit.
- `walgit ci status` — every run in the checkout's collab log, aggregated.

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

### 0. Identity

- One principal per agent, one Ed25519 key (`32` raw bytes as hex, keep at
  `~/.walgit/keys/<name>.ed25519`). Register once per repository:
  `walgit collab principal-register --repo <checkout> --principal <you> --key <keyfile> --push origin`.
- Write only your own inbox (`refs/collab/inbox/<you>/*`). Never borrow another principal's
  key. Read-side verification marks `actor != inbox` entries unverified — treat them as untrusted.

### 1. Work unit = one thread

- Open an `issue` entry with: objective, roles, and **machine-checkable acceptance**.
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
- Express state with kinds, not prose: `status` (`in-progress` / `needs-review` / `done`),
  `review` (`approve` / `request_changes` + note), `merge_result` (`merged: true` + oid),
  `comment` for claims/progress/questions.
- Every `in-progress` / `needs-review` / `blocked` / `needs-human` status carries the supervision
  context: `owner`, `worktree`, `branch`, `work` (or `note`). Fields inherit across status moves;
  an explicit empty string clears them. Example:
  `{"status":"in-progress","owner":"agent-mendel","worktree":"prod-release","branch":"feat/prod-release","work":"fix release preflight"}`.
- Every meaningful step is an entry: claim, progress, result, question.

### 4. Review

- An independent agent (or human) reviews the diff/artifacts and posts **full findings** in the
  `review` entry — location, problem, suggestion — not a one-line conclusion.
- `request_changes` → the implementer fixes on the branch and replies on the thread mapping each
  point to what changed → reviewer re-reviews → `approve` only when satisfied.
- Treat "approve with no evidence" as noise; verification claims must be reproducible.

### 5. Merge & archive

- walgit alone is a complete collaboration platform; **GitHub (or any other host) is optional** and
  is only a mirror when a project already keeps one.
- After approval: merge locally (fast-forward preferred), push the result to this host. Push the
  same refs to a GitHub mirror only for projects that are dual-homed (walgit = fact source,
  GitHub = backup/public mirror) — never as a requirement of walgit itself.
- Write `merge_result` `{"merged": true, "oid": "<sha>"}` and a `status` `done` on the thread.
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
