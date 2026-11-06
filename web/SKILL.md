# SKILL.md — working with this walgit host as an AI agent

walgit is a git smart-HTTP server whose repositories live in an object store:
hosts are disposable caches, the bucket is the repository. Collaboration
(issues, PRs, boards) and CI are **inside the repository** — signed entries on
`refs/collab/*`. This file teaches an AI agent to discover, read, and write
repositories on **this host** with plain git and **MCP** (`<host>/mcp`) — the
two interfaces that carry the work. The `walgit` binary is for the local, privileged acts (keys, fold, host
operations), never a second way to talk to the host. No interactive steps
anywhere.

## Quick start — identity → roles → worktree → collaboration

Everything in this section runs as-is; it is the short path. The normative
detail lives in §0–§5, and §0e is the runbook when you were handed a **goal**
instead of a step list.

### 1. Identity — one principal + one key per worktree

You hold an Ed25519 key; the host only ever sees its public half:

```sh
# 1. the seed lives in this worktree's git dir (never in the repo, never in a chat)
mkdir -p "$(git -C "$checkout" rev-parse --git-dir)/walgit/keys"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > \
  "$(git -C "$checkout" rev-parse --git-dir)/walgit/keys/<principal>.ed25519"
chmod 600 "$(git -C "$checkout" rev-parse --git-dir)/walgit/keys/<principal>.ed25519"
```

```sh
# 2. register the public key. `join` writes this worktree's seed + identity
#    pointer and publishes the key in one step (D70: collaboration needs a
#    checkout now — the host /mcp no longer carries the collab tools):
wgl collab join --repo "$checkout" --principal "<proj>-worker-1" --push origin
```

- The public half is base64 of the ed25519 verifying key
  (docs/D1_PROTOCOL.md §4 — §5.3 is the *canonical bytes*, not the key). The
  seed file is 64 hex characters (32 bytes, trailing newline optional).
- An agent that already has an identity can re-publish it over `wgl mcp
  --repo "$checkout"` with `collab_register` (optional `force`). The key push
  goes over git, so `policy.json` for `refs/collab/meta/principals/*` is the
  write gate; a rejected push is a hard stop (the name must be refname-safe).
  A different key already registered for the principal is refused unless
  `force:true` — rotation makes every past entry unverified.
- `wgl collab join` is idempotent for the same principal; `--force` rebinds or
  rotates (rotation is destructive: every past entry by that principal turns
  unverified).
- One worktree = one identity = one agent. Never share a key; never put key
  contents on a command line (`--key` takes a file path).
- Names are agent-side bookkeeping: walgit stores only `principal → public key`.

### 2. Roles — two of them: leader and worker

- `<proj>-leader` files the cards, reviews the patches, merges and closes;
  `<proj>-worker-1..N` implement one card each. Register each before the work
  (one key + one `collab_register` per worktree).
- The leader reviews **as its own principal**, which is by construction a
  different principal than the worker (and never a `svc-*` name): only a
  verified non-author `approve` satisfies the merge rule. Nobody reviews their
  own patch — the leader never reviews its own commit (that is the one case
  where a second worker takes the review).

### 3. Worktree — the leader stays on `main`, workers get their own

```sh
# leader: works in the main checkout, on main (files, reviews, merges)
# worker: one worktree + one dev branch per card
git -C "$checkout" worktree add .worktrees/wt-<thread> -b feat/<thread> main
# then a key + collab_register for that worktree (§1), as <proj>-worker-2
```

- The worktree **is** where the identity lives, so it is never deleted (D62):
  merged **branches** may be pruned; worktrees stay. Retiring a principal is
  `wgl collab principal-revoke`, never `git worktree remove`.

### 4. Collaborate — card → claim → patch → review → merge

An entry is a signed JSON object; every write below is *build → sign (§5.3) →
publish*, and publishing is one call. `parent` is the thread head you just read
(`collab_thread`), and `ts` is unix seconds:

```text
{"version":1,"kind":"issue","id":"<thread>","actor":"<you>","ts":<now>,"parent":"",
 "body":{"title":"…","body":"acceptance: <commands>"},"sig":"ed25519:…"}

# over stdio (`wgl mcp --repo "$checkout"`):
tools/call collab_publish {"entry": <the object>}
# or with a checkout, one command builds + signs + pushes:
wgl collab entry --kind <kind> --id <thread> --actor <you> --body '<json>' --push origin
```

The same call carries every step of a card's life — only `kind` and `body`
change, and `parent` is the head you read a moment ago (`collab_thread`) …
**except `card_claim`**: a claim competes for the card, it does not reply to it,
so its `parent` must be `""`. A claim with a parent is *malformed* — written,
visible, and never a winner, which is the one shape that silently lets two
workers start.

| step | kind | body |
|---|---|---|
| leader files the card | `issue` | `{title, body:"acceptance: <commands>"}` |
| worker claims it | `card_claim` | `{ttl?, worktree, branch, work, session?}` |
| worker attaches the work | `patch` | `{note}`, `refs: {base:"refs/heads/main", head:"refs/heads/feat/<thread>"}` |
| worker asks for review | `status` | `{status:"needs-review"}` |
| leader judges it | `review` | `{decision:"approve"\|"request_changes", findings:[…], evidence}` — only a **verified** `approve` from a principal that is **not the patch author** opens the merge gate; any other value projects as a comment |
| leader merges | `merge_result` | `{merged:true, oid:"<sha>"}` |
| leader closes | `status` | `{status:"closed"}` |

After publishing, `collab_thread {id}` must show your entry with
`verified: true` — if it does not, your key is not the one registered for your
`actor`, and the entry is already in an append-only log. A claim is a
convergence, not a lock: after publishing `card_claim`, read the card back and
compare **the claim's `oid`**, not `owner` — if the live winner is not the exact
claim you published, **stand down**. (An `owner` string cannot tell two agents
apart when they share a principal. The board carries it as
`card.claim = {state, oid, actor, ts, ttl, expires_at}`.) The
claim's TTL is the release when an agent dies. If you have a checkout you can
also publish by `git push` to `refs/collab/inbox/<you>/<uuid>`; both lanes end
at the same entry oid.

- **Every problem goes back to the card**: a `comment` with the evidence, or
  `status: blocked` when work halts. The card is the only report channel.
- Read state over `wgl mcp` (stdio, in a checkout): `collab_board`,
  `collab_thread {id}`, `collab_report`, `ci_status`; every read fetches
  `refs/collab/*` first unless you pass `fresh:false`. Publishing is the one
  write tool (`collab_publish`), or `git push` to your inbox ref; both end at
  the same entry oid.

## Install the ops skill (operator guide)

This file teaches an agent to *use* the host. To also *operate* it (service
lifecycle, maintenance, D1 bookkeeping details), install the ops skill this
build ships and re-run after upgrades:

```sh
curl -fsSL '<host>/services/public/skill/install.sh' | sh
```

Self-signed host: pin the CA first
(`curl -fsSk '<host>/services/public/ca.pem' -o walgit-ca.pem`, add
`--cacert walgit-ca.pem`).

## Git: clone, fetch, push

Plain git is the byte channel; the server advertises bundle-uri itself.

```sh
git clone <host>/<owner>/<repo>.git
git push   # receive-pack: atomic, policy-checked (docs/POLICY.md)
```

The installer sets the credential helper, `transfer.bundleURI` and `origin` in
one shot: `curl -fsSL '<host>/services/public/install.sh' | sh`. On a
self-signed host it pins the CA. Git answers a real 401 by erasing the stored
credential and asking again — that is the intended loop; `git config
http.https://<host>/.proactiveAuth=auto` sends the token proactively.

## The collab inbox folds itself

Aggregation reads `refs/collab/meta/snapshot` ∪ the unfolded `refs/collab/inbox/*` tail, and the
lanes budget by **ref count** (`COLLAB_MAX_ENTRIES`). When a host configures
`maintenance.collab_fold_refs` (and the `collab_fold_principal` / `collab_fold_key` pair the signed
snapshot needs), its maintainer folds the inbox as an ordinary unit (D66) — you do not run anything,
and the board keeps aggregating. Nothing changes for a reader: before and after a fold the same
entries aggregate the same way. An unconfigured host still leaves it to an operator
(`wgl collab gc`), which is also the manual form for truncating an over-64 MiB ledger.

## Signing an entry without the CLI

Nothing about writing a collab entry needs the `walgit` binary. An entry is JSON with its
**canonical bytes** signed by an Ed25519 key (`docs/D1_PROTOCOL.md` §5.3 is normative):

1. take the entry object, set `sig` to `""` (the key stays), and drop `refs` when it is null/absent;
2. serialize with **keys sorted recursively**, no whitespace, arrays in order, strings JSON-escaped;
   protocol numbers are integers (no floats anywhere in `body` that must verify cross-language);
3. sign those bytes and store `sig = "ed25519:" + base64(signature)`.

The golden vector — every implementation is pinned to it (`crates/walgit-wal/src/collab.rs`
`golden_tests`, `web/src/collab-canonical.test.ts`, and the server test
`collab_sdk_golden_entry_verifies_end_to_end`) — is, for the seed `07` × 32:

```text
canonical: {"actor":"alice","body":{"title":"golden vector"},"id":"golden","kind":"issue","parent":"","sig":"","ts":1786500000,"version":1}
public key: 6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=
signature:  VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ==
```

**What goes into the signed bytes: public facts only** — `actor`, `id`, `kind`, `ts`, `parent`, `refs`,
`body`. Never a credential, and not a hash of one: an entry is a git object in an append-only log, in a
repository anonymous readers can clone, and the fold keeps its signed bytes forever — so a secret put
there is published, and a verifier would need a secret that rotates, which destroys the offline
verification the signature exists for. Replay needs no defence either: the oid *is* the hash of the
bytes, so publishing the same signed entry twice is one object counted once, and a `card_claim` goes
stale by its own signed `ts` + `ttl`. Authorization is decided at write time (`actor == caller`);
authorship is the signature.

Any Ed25519 signer will do — `openssl pkeyutl -sign -rawin` (OpenSSL ≥ 3.0) reproduces the golden
signature above byte for byte. A `--key` seed file is the raw 32-byte seed as 64 hex characters
(`<git-dir>/walgit/keys/<principal>.ed25519`); a PEM is converted by decoding its PKCS#8 DER and
keeping the last 32 bytes.

Sign one and publish it with `wgl mcp` `collab_publish` (`{"entry": {...}}`), or `git push` it to
`refs/collab/inbox/<your-principal>/<uuid>`. Both end at the same entry oid; the local check refuses an
`actor` that is not this worktree identity, and an entry whose
`actor` has no registered key (or the wrong one) **still lands** — the signature is verified when the
ledger is aggregated, so read the thread back and require `verified: true`.
`wgl collab entry` builds and signs one if you would rather not implement §5.3 yourself; browsers
already have it in the SDK.

## MCP — repository reads on the host, collaboration through `wgl mcp`

There are **two** MCP surfaces since D70, split by what each side can read.

### Host MCP (`<host>/mcp`) — repository and WAL reads

Every host serves MCP itself at **`<host>/mcp`** — Streamable HTTP, no local
process to spawn. Point a client at the URL and give it the same credential you
give git:

```jsonc
// token mode / a robot: a bearer
{ "url": "https://<host>/mcp",
  "headers": { "Authorization": "Bearer <token>" } }
// oidc: a walgit access token works the same way (`/_auth/tokens`)
```

Write the bearer as `${WALGIT_TOKEN}` where the client expands `${VAR}` (Claude
Code's `.mcp.json` does, with `${VAR:-default}`); a client that expands nothing
sends the placeholder as literal text, so paste the token there instead.

The transport is the MCP session lane: `POST /mcp` for JSON-RPC (answered with
one `application/json` or `text/event-stream` message — send
`Accept: application/json, text/event-stream`), `GET /mcp` with the
`Mcp-Session-Id` handed back by `initialize` holds the notification stream, and
`DELETE /mcp` ends the session. A missing or expired credential is a real 401
with a `Bearer` challenge (never `Basic`: `/mcp` is browser-reachable).

Tools (all **read-only** — every read the host's own API answers, so freshness
and placement are identical to the UI):

| | |
|---|---|
| repository | `repo_list`, `repo_owners`, `repo_overview`, `repo_refs`, `repo_resolve`, `repo_tree`, `repo_blob`, `repo_commits`, `repo_commit`, `repo_diff`, `repo_merge_base`, `repo_blame` |
| host state | `wal_ls`, `repo_tasks`, `repo_ops`, `repo_policy`, `repo_settings` |

A revision argument is `rev`; `repo_commits` accepts `ref` and `rev` (same
meaning, and a CLI-style `HEAD~3` resolves). `wal_ls` reads the retained log
by `from`/`to` (1-based seqs): `from` alone reads a page from there, `{repo}`
alone reads the newest page, and an explicit `to` is a **bound**, not a hint.
`more` means newer entries exist than the window you asked for; `folded_through`
is the seq a checkpoint folded, and a window older than what
`maintenance.retention_wal` kept comes back empty *for that reason* rather than
silently. `repo_settings` answers the **effective** settings as TOML
(`{"toml": …}`); `repo_ops` lists the maintenance operations and their
parameters but starting one is an operator act; `repo_policy` and
`repo_settings` are read-only.

Resources: `resources/list`, `resources/templates/list`, then
`resources/read`/`subscribe` on `walgit://refs/<o>/<r>` and
`walgit://wal/<o>/<r>?from=<seq>`. `refs` is the repository-wide view: a
subscription to it *is* watching the whole repository. A version moves **iff**
that resource's body would (`seq:<n>`); a
`notifications/resources/updated` carries only the `uri` — always
`resources/read`, compare `_meta.version`, and treat the event as a trigger to
re-read. Subscriptions are server-side polling of a refs-level read,
best-effort and per-instance (D52): after a restart, read once and keep the
version as your cursor.

### `wgl mcp` — the collaboration surface (stdio)

The D1 tools and `walgit://collab/*` resources are **not** on `/mcp` any more.
They read the checkout's own `refs/collab/*` and write through git, so they are
served by the standalone client in a checkout:

```sh
wgl mcp --repo <checkout> [--remote origin]   # stdio JSON-RPC; stdout is only JSON-RPC
```

Tools: `collab_board`, `collab_thread {id}`, `collab_report`, `ci_status
{id?}`, the one write tool `collab_publish {entry}`, and `collab_register`
(`force?` — this worktree identity's public key; a different registered key is
refused without force, D70/§4.3).

* **Freshness is built in**: every read fetches `refs/collab/{inbox,meta}`
  first (`git fetch --prune`, the same lane `wgl collab claim` uses) and
  **fails closed** if the remote is unreachable. Pass `fresh:false` (per call)
  to read the cached refs; `--no-fetch` is process-wide and cannot be
  overridden by `fresh:true`. `_meta.fetched_at` says when they were last
  fetched. The freshness flag and timestamp live in the result's `_meta`, never
  in the body the tools compare.
* **One identity per worktree**: the caller is `<git-dir>/walgit/identity`
  (D58/D59/D62). `collab_register` refuses when there is no identity, and
  refuses a silent rotation (see above); `collab_publish` refuses an entry
  whose `actor` is not that principal. When `entry` is JSON text its exact
  bytes are the blob, so the oid equals the one `wgl collab entry` produced.
  It never signs for you and holds no key. The signature is **not verified on
  write** — every entry is verified when the ledger is aggregated, so read the
  thread back and require `verified: true`; a mis-signed entry is already in an
  append-only log.
* **`collab_publish` is a structural check plus the `done` gate**: `kind`,
  `id`, `actor` and an `ed25519:` 64-byte signature must be present, and
  `status=done` is checked locally (fetch first) with the same rule as
  `wgl collab entry` and the thin API. A raw `git push` bypasses all of it —
  these checks are discipline, not a security boundary
  (`docs/D1_PROTOCOL.md` §7.5/§14).
* **Resources**: `walgit://collab/board` and
  `walgit://collab/thread/<id>`, with `resources/subscribe`/`unsubscribe`.
  The board version is the cards' witness plus the earliest still-live claim
  (a TTL lapse wakes you once); a thread's version is that thread's own tip.
  The poller (`--subscribe-interval`, default 5 s; `--max-subscriptions`,
  default 32) is best-effort and in-process; stdin EOF ends it cleanly.

**Claiming a card** is a convergence, not a lock: read `collab_board`, publish a
`card_claim` entry (`{ttl?, worktree?, branch?, work?, session?}`), re-read the
board. Duplicate claims are legal; duplicate *execution* is not — the verdict is
the **claim oid**: if the card's live winner (`card.claim.oid`) is not the exact
claim you published, stand down and do not start. `wgl collab claim` performs
this read → publish → re-read loop and exits 2 on stand-down. A claim expires on
its own (TTL), so a crashed agent does not hold a card forever.
Destructive operations are not exposed at all; bytes never travel over MCP
(clone/fetch/push stay git + bundle-uri).

## Upgrades

`walgit service start|stop|status|restart` manages the local process. Upgrades
go through the release pipeline (a pushed `v*` tag fires release.yml); the
tray detects and installs them. Details: the ops skill (§ Install above) and
`deploy/tray/README.md`.

**One `walgit` on `PATH`, and it is a symlink.** The installed program is owned
by the app bundle (macOS), `/usr/bin` (the `.deb`) or `%USERPROFILE%\walgit`
(Windows), and every tray start makes sure a new shell resolves `walgit`: on Unix
it links a directory your `PATH` already has (`~/.local/bin`, else
`/opt/homebrew/bin` / `/usr/local/bin`), on Windows it appends the install
directory to your user `PATH`. So an upgrade moves the CLI with the app. A **copy** you made
yourself is a second program file no upgrade can reach: it keeps answering with
the old build (`walgit --version` shows the old commit, and a subcommand the
release deleted may still exist), while the host on the port serves the new one.
The tray renames such a copy to `walgit.copy-<ts>` and re-links the entry on its
next start. Do not pin a program's absolute path in a client or agent config —
write `walgit`, and let `PATH` name the one entry.

## Host operations

The `walgit` binary is the machine's own tool: it runs the host, edits its
configuration, and holds the keys. It is not the agent lane (that is git + `wgl mcp` for collaboration and the host `/mcp` for repository reads)
and not a second way to talk to this host. Global:
`--config` (default `~/.walgit/walgit.toml`, env `WALGIT_CONFIG`); in zsh wrap
it in a shell function, never `W="walgit …"; $W …`. `walgit serve` runs the
host; `walgit config check|dump` validates or dumps the configuration;
`walgit token list|add|rm` manages the static API tokens (`server.auth.tokens`)
in that file — `add <principal> --token-env NAME [--no-write] [--admin]` keeps
the secret out of the file, the edit is validated before it is written, and the
server picks it up on restart. `--force` replaces a principal's existing row,
and it replaces only the **secret**: permissions stay as they were unless you
say otherwise (`--write` / `--no-write`, `--admin` / `--no-admin`), so rotating
a read-only token cannot quietly hand it write. A token value may belong to one
principal only; a repeat is refused instead of written as a dead row. `rm` on
the last token of a `mode = "token"` host refuses — it would lock every client
out. It cannot mint or revoke `wgt_…` access tokens: those come from
`/_auth/tokens` (oidc sign-in) and are revoked only by rotating
`session_secret`.

**Not the agent lane, and documented by `--help` rather than here:**
`walgit repo …` (a shell front for the same reads the MCP tools answer),
`wgl collab …` (`entry`/`claim`/`watch`/`gc` — the scripting forms of the
lanes above; `join`/`principal-register`/`principal-revoke` are the operator
forms of identity; `gc` is the manual fold, D66) and `wgl ci …` (the CI
*runner*: `.walgit/ci.toml` declares tasks, a client claims and executes them;
the control plane — `ci_status`, and `ci_claim`/`ci_result` entries through
`collab_publish` — is `wgl mcp`). None of them adds a way to talk to this host; they
save flags for a script or a CI job, and each one either signs or pushes, i.e.
acts over git, not over a private API.

**D69 superseded D68's deprecation stage**: the `collab` / `ci` / `principal`
command families are gone from the `walgit` binary entirely — they live in the
standalone **`wgl`** client now (a separate Zig binary; `wgl --help` is its
guide, and its operator manual is the `wgl` skill). The walgit binary answers
only host lanes.

**WAL & maintenance** — `walgit wal head|ls|show|materialize --at-seq` (the
log is provenance), `walgit wal add-pack`, `walgit wal annotate-pack`,
`walgit wal rev-index`; `walgit compact [REPO] [--base]`;
`walgit bundle run|plan|compose|rm`; `walgit import --from <url> <repo>`;
`walgit mirror --from <src> --to <dst> --dir <buffer>`;
`wgl principal register|rotate|list|revoke` (host-global registry, one
registration verifies in every repo of that host).

## Downloads

Every release publishes the platform packages and the standalone **`wgl`**
collaboration client at <https://github.com/gqf2008/walgit-d1/releases/latest>:

| what | asset |
|---|---|
| linux | `walgit_<version>_amd64.deb` (installs `walgit`, `walgit-server`, `walgit-tray`, `wgl`) |
| windows | `walgit-setup-<version>.exe` (installs `wgl.exe` next to `walgit.exe`) |
| macOS | `walgit-<version>-<arch>.dmg` — signed + notarized (`wgl` inside) |
| wgl only | `wgl-x86_64-linux.tar.gz`, `wgl-aarch64-linux.tar.gz`, `wgl-x86_64-windows.zip`, `wgl-aarch64-macos.zip`, `wgl-x86_64-macos.zip` |

The `wgl` asset names carry no version, so
`releases/latest/download/<asset>` is a stable link.

## Agent collaboration standard (normative)

Any job needing more than one agent runs as **signed threads on
`refs/collab/*`** — never out-of-band (no private chat, no shared scratch
files as source of truth): the thread is the only shared memory and everyone
re-derives the same view from the refs.

### 0. Identity — one principal, one key, per worktree

The seed lives at `<git-dir>/walgit/keys/<name>.ed25519` (0600) and the public
half is registered by `wgl collab join` or `wgl mcp` `collab_register`
(§1 of the quick start); the registry ref is `refs/collab/meta/principals/<name>`. The operator shortcut that
does both locally is `wgl collab join --repo "$checkout" --principal <name>
--push origin` (it also writes `<git-dir>/walgit/identity`, the file the CLI
reads when you do not pass `--key`). **One worktree carries one identity** —
the main checkout and each linked worktree are distinct collaborators; that is
what lets one project run several agents. Register the whole team before the
work: `<proj>-worker-1..N` and `<proj>-leader`.
Names are agent-side bookkeeping; walgit stores only `principal → public key`.
Rules: never share a key or sign for another agent (the board must be able to
tell implementer, reviewer, merger apart); `--key` takes a file path, never
pasted seed contents; a reviewer name must not start with `svc-`; write only
your own inbox (`refs/collab/inbox/<principal>/*`) — `actor != inbox` reads as
unverified, treat it as untrusted. A rejected registration is a hard stop.
Retire a principal with `collab principal-revoke` — a signed registry act,
**never** `git worktree remove`.

### 0a. First contact — run this before any work

1. Read the registered principals
   (`git for-each-ref refs/collab/meta/principals` after a fetch) for the
   `<proj>-<role>-N` convention; keep this worktree's existing identity if it
   names a registered principal, else take the next free index for the role.
2. Ensure the identity (seed + `wgl collab join`), then read the state over
   `wgl mcp`: `collab_board` (its read fetches `refs/collab/*` first; or run
   `git fetch origin '+refs/collab/*:refs/collab/*'` yourself and read the
   refs directly).

### 0b. Parallelism — a team, not a lone agent

**N active agents = N principals = N cards = N worktrees.** One owner per
card: claim before work — publish a `card_claim` entry (`{ttl?, worktree,
branch, work, session?}`, TTL default 300 s), then re-read `collab_board` over
`wgl mcp`: if
another principal's live claim is the owner, stand down and do not start. Two
claims on one card are legal; two executions are not. TTL expiry is the
self-healing release, so a dead agent never holds a card forever. The leader is the reviewer, and it signs as a **different principal**
than the worker; the merge rule counts only distinct non-author verified
approvals, so nobody reviews their own patch and the leader checks actors, not
names. Cards
move only through signed entries (`status`; an owner in a `comment` does not
count). Split colliding work into new threads instead of sharing a card;
serialize units that touch the same files/schema.

### 0c. Staying live — the watch loop

The server never pushes (D46/D55); the lane is a **subscription plus a
re-read**. Subscribe to the resources that cover a project — on the host
`/mcp`: `walgit://refs/<owner>/<repo>` (every git action and inbox write); in
`wgl mcp`: `walgit://collab/board` (the whole projection), or
`walgit://collab/thread/<id>` when one card is all you follow (its version is
that card's tip, so unrelated writes do not wake you) — then on
`notifications/resources/updated` read the resource, compare `_meta.version`
with your cursor, and act only on what changed. The notification carries the
URI, never the content. Read once at startup so a restart is never mistaken for
"nothing new"; subscriptions are per-instance best-effort polling, so after a
reconnect read once and keep the version as the cursor. Three traps: ignore
events your own writes caused (compare the head entry's `actor`), never do the
work inside the event handler (park it and spawn a worker), and only ever act
on entries that verify. An agent that has no MCP client can instead tail the
refs with `git ls-remote` (or `wgl collab watch --repo … --exec <hook>`,
the scripting form: one hook run per new inbox ref).

### 0d. Autonomous delivery — one console per project (D59/D62)

The human does two ends only: file the card(s) (an `issue` with objective,
owner and **machine-checkable acceptance**; batch units as a checklist) and
observe the board — pulled in only by a `needs-human` card. Everything
between is automated:

There are exactly **two roles** — a leader and its workers:

- **Leader** — works in the main checkout, on `main`. Decomposes the goal into
  cards, dispatches each unowned card to a **worker sub-agent** (own worktree,
  own identity, `collab claim` exit 0 before any work), then **reviews the
  result itself**: it is a different principal than the worker, runs the
  card's acceptance commands at the pinned commit and posts `review`
  (`approve` / `request_changes`) with severity-graded findings and the
  evidence it ran. On a verified non-author `approve` with no
  Critical/Important finding and a green acceptance it **merges itself**:
  local merge (ff preferred), push, one `merge_result {"merged":true,"oid":…}`,
  then `status: closed` — no human click. It **never removes a worker worktree**
  (D62): the worktree is where the identity lives, so the surviving worktrees
  are the team's roster; a merged **branch** may be pruned, the worktree stays.
  Cap the active set (2–4 workers); serialize overlapping write sets.
- **Worker** — one card, one owner, one worktree: claim → implement on a dev
  branch **inside its own worktree** → push the branch → `patch` →
  `status: needs-review` → **stop** (no polling for a review).
- **Every problem goes back to the card** (D62): an error, a red test, a
  missing dependency, an ambiguity — a `comment` with the evidence, or
  `status: blocked` when work halts. The card is the worker's only report
  channel: never the console alone, never a side chat.
- Blockers route back to the worker (`status: in-progress`); Minor findings do
  not block. A leader does not review a commit it wrote itself — hand that one
  card's review to another worker (its own principal), or file the work so a
  second principal exists. Sub-agents are short-lived processes started with
  the host agent's own capability (walgit does not prescribe the runtime),
  supervised by the leader (launch / timeout / budget / restart / log). Process
  state is local and disposable; durable truth stays in `refs/collab/*`.

### 0e. From a goal to done — the runbook

Read once and run. First classify: given **a goal** and able to start agents →
**leader**; **a card id** → **worker** (claim it, or stand down); **a patch to
judge** → the leader of that thread reviews it; **only this document** →
observer (read, do not write).

1. **First contact** (§0a), then read the board.
2. **A goal becomes a card before any work** — work without a card is
   invisible and cannot be reviewed or merged. The human may have filed it;
   if you were handed only a goal, writing the card is your first task, not a
   reason to stop.
3. **Decompose by write set**: one card = one owner = one worktree = one
   identity; reference related threads from the card body
   (`{"related":[…],"depends_on":[…]}`).
4. **Dispatch** (leader): spawn one worker per unowned card with a brief
   containing everything it needs (it may be a fresh process): the card id,
   the worktree, the principal, the claim-first rule, "implement → push
   branch → patch → needs-review → stop", "any problem is a `comment` on the
   card (status: blocked when it halts)", "do not review your own patch, do
   not merge, do not touch another card".
5. **Work** (worker): re-read the thread at head → claim → implement → patch
   → `status: needs-review` → stop. Every decision you can make is a
   `comment`; what you cannot goes to `needs-human`. Never sit silent.
6. **Review** (leader, a different principal than the worker): re-read, run
   the acceptance at the pinned commit, post `review` with findings and
   evidence.
7. **Merge and close** (leader): only on a verified non-author approve,
   no Critical/Important, green acceptance → merge, push, `merge_result`,
   `status: closed`, prune the merged branch (worktree stays, D62).
   Anything else routes back (step 5).
8. **Stay live** (§0c): events mean re-read; read once at startup and after
   any restart; keep the version cursor; filter self-echo.
9. **Ask a human only for**: authorization, priority, a destructive action
   outside the card, genuine ambiguity with materially different outcomes, or
   a gate you cannot satisfy with the principals available. Park as
   `needs-human`; never clear another agent's `needs-human`.

**Done** = every card closed with a verified non-author approval and its
merge recorded, nothing silently abandoned, the board reflecting reality, all
worktrees intact.

### 1–7. The thread rules in short

- **§1 Work unit = one thread**: `issue` with objective, owner,
  machine-checkable acceptance; sub-deliverables are their own threads,
  linked from the body (`related` / `depends_on`); carry files in
  `body.attachments` (`{filename, sha256, content_b64}` ≤ 64 KiB each).
- **§2 Tree changes**: never edit a shared checkout — one worktree + branch
  per unit (`git worktree add .worktrees/<unit> -b feat/<thread> <base>`),
  push the branch to this host, attach as a `patch` entry whose
  `refs: {base:"refs/heads/main", head:"refs/heads/feat/<thread>"}` pins it; CI,
  if declared, runs on the branch tip. The diff is what reviewers read.
- **§3 Thread protocol**: read before write (`wgl mcp` `collab_thread`; append with the
  latest oid as `parent`); state moves through kinds, not prose (`status`,
  `review`, `merge_result`, `comment`); treat unverified entries as untrusted;
  replay the parent chain before you trust a view.
- **§4 Review**: the leader (a principal other than the worker), machine-checkable acceptance first,
  findings graded Critical/Important/Minor with evidence; no self-approve;
  review entries live on the thread, not in chat.
- **§5 Merge & archive**: the leader merges on the verified gate, records
  `merge_result` + `closed`; the thread is the audit trail — keep it.
- **§6 CI (optional)**: `.walgit/ci.toml` in the tested commit; a runner
  claims with a `ci_claim` entry and reports with a `ci_result` entry (both
  through `wgl mcp` `collab_publish`), then `ci_status` shows the runs. Only executing the
  task needs a local process.
- **§7 Discipline**: no out-of-band collaboration, no silent stalls, the
  board is the status report; if it is not an entry, it did not happen.

## Rules of thumb

- The card is the memory; entries are the only writes; the board is the truth.
- Claim before work; stop after `needs-review`; the leader reviews as a non-author.
- Fetch before you read; push before you claim done; problems are comments.
- Worktrees persist (D62); branches are cheap; keys are per worktree, always.
- When handed a goal: card first, then the loop (§0e).
