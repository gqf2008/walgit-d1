# SKILL.md — working with this walgit host as an AI agent

walgit is a git smart-HTTP server whose repositories live in an object store:
hosts are disposable caches, the bucket is the repository. Collaboration
(issues, PRs, boards) and CI are **inside the repository** — signed entries on
`refs/collab/*` — and the `walgit` CLI is their primary interface. This file
teaches an AI agent to discover, read, and write repositories on **this host**
with plain git, the CLI, and HTTP. No interactive steps anywhere.

## Quick start — identity → roles → worktree → collaboration

Everything in this section runs as-is; it is the short path. The normative
detail lives in §0–§5, and §0e is the runbook when you were handed a **goal**
instead of a step list.

### 1. Identity — one principal + one key per worktree

```sh
walgit collab join --repo "$checkout" --principal <proj>-worker-1 --push origin
```

- Generates the Ed25519 key inside that worktree's git dir
  (`<git-dir>/walgit/keys/<principal>.ed25519`, 0600), writes the identity
  pointer, and publishes the public key. A rejected registration is a hard stop.
- One worktree = one identity = one agent. Never share a key; never put key
  contents on a command line (`--key` takes a file path).
- Names are agent-side bookkeeping: walgit stores only `principal → public key`.

### 2. Roles — name the team by convention

- `<proj>-worker-1..N` implement; `<proj>-reviewer-1..N` review;
  `<proj>-coordinator` files cards and merges. Register each before the work
  (repeat `collab join`, one principal per worktree).
- The reviewer must be a **different principal** than the patch author (and
  never a `svc-*` name): only a verified non-author `approve` satisfies the
  merge rule. Nobody reviews their own patch.

### 3. Worktree — the leader stays on `main`, workers get their own

```sh
# coordinator/leader: works in the main checkout, on main (files, reviews, merges)
# worker: one worktree + one dev branch per card
git -C "$checkout" worktree add .worktrees/wt-<thread> -b feat/<thread> main
walgit collab join --repo "$checkout/.worktrees/wt-<thread>" --principal <proj>-worker-2 --push origin
```

- The worktree **is** where the identity lives, so it is never deleted (D62):
  merged **branches** may be pruned; worktrees stay. Retiring a principal is
  `walgit collab principal-revoke`, never `git worktree remove`.

### 4. Collaborate — card → claim → patch → review → merge

```sh
# coordinator files the card (objective + machine-checkable acceptance)
walgit collab entry --kind issue --id <thread> --body '{"title":"…","body":"acceptance: <commands>"}' --push origin
# worker claims it (exit 0 = go; exit 2 = stand down, do not start)
walgit collab claim --id <thread> --worktree wt-<thread> --branch feat/<thread> --push origin
# … implement on the branch, push it, then attach and ask for review …
walgit collab entry --kind patch --id <thread> --parent <oid> --base refs/heads/main --head refs/heads/feat/<thread> --body '{"note":"…"}' --push origin
walgit collab entry --kind status --id <thread> --parent <oid> --body '{"status":"needs-review"}' --push origin
# reviewer (different principal) runs the acceptance on the pinned commit and posts review approve / request_changes
# coordinator merges only on a verified non-author approve, then records:
walgit collab entry --kind merge_result --id <thread> --parent <oid> --body '{"merged":true,"oid":"<sha>"}' --push origin
walgit collab entry --kind status --id <thread> --parent <oid> --body '{"status":"closed"}' --push origin
```

- **Every problem goes back to the card**: a `comment` with the evidence, or
  `status: blocked` when work halts. The card is the only report channel.
- Read state with `walgit collab board`, `walgit collab thread <id>`,
  `walgit collab report --remote origin`. Entries without `--push` stay local.

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

## MCP (optional, client-side)

Host-spawned over stdio — configure a command, not a URL:

```jsonc
{ "command": "walgit", "args": ["mcp"] }                     // read-only
{ "command": "walgit", "args": ["mcp", "--repo", "/path/to/checkout",
                                "--subscribe-interval-ms", "5000",
                                "--max-subscriptions", "32"] }
{ "command": "walgit", "args": ["mcp", "--allow-write", "--key", "/path/key.ed25519"] }
```

Its tools **are** this CLI (one implementation, no second contract). Read-only
surface: `repo_list/owners/refs/resolve/tree/blob/commits/diff`,
`collab_ls/thread/pr/board/report`, `ci_status`, `wal_ls`. The writing tools —
`collab_entry` and `collab_claim` — exist only under `--allow-write --key`, and an entry's
`actor` must be the principal that key is registered under (a mismatched pair can only write a
permanently unverified entry, so the call is refused). Destructive operations are not exposed at
all; bytes never travel over MCP.

Resources (`resources/list|read|subscribe`): `walgit://refs/<o>/<r>`,
`walgit://wal/<o>/<r>?from=<seq>`, `walgit://collab/board/<o>/<r>`,
`walgit://collab/thread/<o>/<r>/<id>`. Two subscriptions cover a project —
refs (every git action + inbox) and board (the whole projection). A
`notifications/resources/updated` carries only the `uri`: always
`resources/read`, compare `_meta.version`, ignore unchanged, and treat an
event as a trigger to re-read — never act on the notification alone.
Subscriptions are adapter-side polling, best-effort, process-local (D52): on
start/restart read once and keep the version as your cursor. One `walgit mcp`
process per agent, in its own worktree, with its own principal and key; filter
your own writes by the head entry's `actor`.

## Host operations & upgrades

`walgit service start|stop|status|restart` manages the local process. Upgrades
go through the release pipeline (a pushed `v*` tag fires release.yml); the
tray detects and installs them. Details: the ops skill (§ Install above) and
`deploy/tray/README.md`.

## The CLI

Everything works against a **checkout** of a repository on this host. Global:
`--config` (default `~/.walgit/walgit.toml`, env `WALGIT_CONFIG`); in zsh wrap
it in a shell function, never `W="walgit …"; $W …`. `walgit serve` runs the
host; `walgit config check|dump` validates or dumps the configuration.

**Collaborate (`walgit collab …`)** — read: `ls`, `thread <id>`,
`thread-heads`, `pr <id>`, `board` (the `.walgit/board.toml` projection),
`report [--remote origin]` (prints `unpushed: N`; reads answer from **local**
refs, so fetch first). Write: `entry --kind <kind> --id <thread> --parent
<oid> --body '<json>' [--base … --head …] --push <remote>` (without `--push`
the entry stays local); `claim` (§0b); `gc` (fold the inbox, D45, idempotent;
`--truncate` past the 64 MiB cap); `watch` (§0c); `join`,
`principal-register`, `principal-revoke`, `principal-fetch` (§0).

**Decentralized CI (`walgit ci …`)** — the server holds no CI logic: the
tested commit's `.walgit/ci.toml` declares tasks (`walgit ci validate`), a
client runner claims and executes (`walgit ci run`; simultaneous claims are a
legal race, exactly one result is effective), and `walgit ci status | log
[<run>] | artifacts [<run>] --out <dir>` read results (sha256-verified).

**Repository reads (`walgit repo …`)** — HTTP, no bucket credentials; host:
`--url`/`$WALGIT_URL`, token: `--token`/`$WALGIT_TOKEN`. `owners [<owner>]`,
`refs <o/r> [branches|tags|all|collab]`, `ref <o/r> <full-ref>`,
`resolve <o/r> <rev>`, `tree <o/r> <rev> [path]`, `blob <o/r> <rev> <path>
[--raw]`, `commits <o/r> [--ref --n --skip --path]`, `commit <o/r> <sha>`,
`diff <o/r> <from> <to> [--format]`, `merge-base <o/r> <from> <to>`,
`blame <o/r> <rev> <path>`, `archive <o/r> <rev> [--out FILE]`,
`overview <o/r>`, `tasks <o/r> [--follow <id>]`, `ops <o/r>` +
`op-start <o/r> <op> [--arg k=v]`. Management (bucket-direct, maintainer
form): `repo create|list|info`, `repo policy get|set|clear`,
`repo settings show|set|clear|history`.

**WAL & maintenance** — `walgit wal head|ls|show|materialize --at-seq` (the
log is provenance), `walgit wal add-pack`, `walgit wal annotate-pack`,
`walgit wal rev-index`; `walgit compact [REPO] [--base]`;
`walgit bundle run|plan|compose|rm`; `walgit import --from <url> <repo>`;
`walgit mirror --from <src> --to <dst> --dir <buffer>`;
`walgit principal register|rotate|list|revoke` (host-global registry, one
registration verifies in every repo of that host).

## Agent collaboration standard (normative)

Any job needing more than one agent runs as **signed threads on
`refs/collab/*`** — never out-of-band (no private chat, no shared scratch
files as source of truth): the thread is the only shared memory and everyone
re-derives the same view from the refs.

### 0. Identity — one principal, one key, per worktree

`walgit collab join --repo "$checkout" --principal <name> --push origin`
generates the Ed25519 seed at `<git-dir>/walgit/keys/<name>.ed25519` (0600),
writes `<git-dir>/walgit/identity`, and registers the public key at
`refs/collab/meta/principals/<name>`. **One worktree carries one identity** —
the main checkout and each linked worktree are distinct collaborators; that is
what lets one project run several agents. Register the whole team before the
work: `<proj>-worker-1..N`, `<proj>-reviewer-1..N`, `<proj>-coordinator`.
Names are agent-side bookkeeping; walgit stores only `principal → public key`.
Rules: never share a key or sign for another agent (the board must be able to
tell implementer, reviewer, merger apart); `--key` takes a file path, never
pasted seed contents; reviewer names must not start with `svc-`; write only
your own inbox (`refs/collab/inbox/<principal>/*`) — `actor != inbox` reads as
unverified, treat it as untrusted. A rejected registration is a hard stop.
Retire a principal with `collab principal-revoke` — a signed registry act,
**never** `git worktree remove`.

### 0a. First contact — run this before any work

1. Read the registered principals
   (`git for-each-ref refs/collab/meta/principals` after a fetch) for the
   `<proj>-<role>-N` convention; keep this worktree's existing identity if it
   names a registered principal, else take the next free index for the role.
2. Ensure the identity (`collab join`), then fetch
   `+refs/collab/inbox/*:refs/collab/inbox/*` and
   `+refs/collab/meta/*:refs/collab/meta/*` and read `walgit collab board`.

### 0b. Parallelism — a team, not a lone agent

**N active agents = N principals = N cards = N worktrees.** One owner per
card: claim before work (`walgit collab claim --id <card> --worktree …
--branch … --push origin`; exit 0 = you won, 2 = stand down — do not start).
The claim is the mutex; TTL expiry (default 300 s) is the self-healing
release. Reviewers sign as a **different principal** than the author; the
merge rule counts only distinct non-author verified approvals, so nobody
reviews their own patch and the coordinator checks actors, not names. Cards
move only through signed entries (`status`; an owner in a `comment` does not
count). Split colliding work into new threads instead of sharing a card;
serialize units that touch the same files/schema.

### 0c. Staying live — the watch loop

The server never pushes (D46). The primitive is `walgit collab watch --repo
"$checkout" --remote origin --interval 10 --exec <hook>` (or `--once` from a
timer): every new/changed `refs/collab/inbox/*` ref runs the hook once, the
signed entry JSON on stdin, plus `WALGIT_COLLAB_REF/KIND/THREAD/ACTOR/VERIFIED`.
It is a trigger, not an agent: **park the event and return 0** (long work
belongs in a worker); a non-zero exit ends the watcher and the batch replays
after your supervisor restarts it — keep it under something that restarts on
exit (launchd/systemd/Task Scheduler with a GUI-subsystem launcher, D53), or
run `--once` from a timer. Three traps: filter your own `ACTOR` (self-echo),
never do the work inside the hook, and never act on `VERIFIED=false`. One
loop per principal, holding that principal's key.

### 0d. Autonomous delivery — one console per project (D59/D62)

The human does two ends only: file the card(s) (an `issue` with objective,
owner and **machine-checkable acceptance**; batch units as a checklist) and
observe the board — pulled in only by a `needs-human` card. Everything
between is automated:

- **Coordinator/leader** — works in the main checkout, on `main`. Decomposes
  the goal into cards, dispatches each unowned card to a **worker sub-agent**
  (own worktree, own identity, `collab claim` exit 0 before any work), has the
  result reviewed by an **acceptance agent with a different principal**, and
  on a verified non-author `approve` with no Critical/Important finding and a
  green acceptance **merges itself**: local merge (ff preferred), push, one
  `merge_result {"merged":true,"oid":…}`, then `status: closed` — no human
  click. It **never removes a worker worktree** (D62): the worktree is where
  the identity lives, so the surviving worktrees are the team's roster; a
  merged **branch** may be pruned, the worktree stays. Cap the active set
  (2–4 workers + 1–2 acceptance); serialize overlapping write sets.
- **Worker** — one card, one owner, one worktree: claim → implement on a dev
  branch **inside its own worktree** → push the branch → `patch` →
  `status: needs-review` → **stop** (no polling for a reviewer).
- **Acceptance agent** — different principal, runs the card's acceptance
  commands itself at the pinned commit, posts `review` (`approve` /
  `request_changes`) with severity-graded findings and the evidence it ran.
- **Every problem goes back to the card** (D62): an error, a red test, a
  missing dependency, an ambiguity — a `comment` with the evidence, or
  `status: blocked` when work halts. The card is the worker's only report
  channel: never the console alone, never a side chat.
- Blockers route back to the worker (`status: in-progress`) or a fixer;
  Minor findings do not block. Sub-agents are short-lived processes started
  with the host agent's own capability (walgit does not prescribe the
  runtime), supervised by the orchestrator (launch / timeout / budget /
  restart / log). Process state is local and disposable; durable truth stays
  in `refs/collab/*`.

### 0e. From a goal to done — the runbook

Read once and run. First classify: given **a goal** and able to start agents →
coordinator; **a card id** → worker (claim it, or stand down); **a patch to
judge** → acceptance; **only this document** → observer (read, do not write).

1. **First contact** (§0a), then read the board.
2. **A goal becomes a card before any work** — work without a card is
   invisible and cannot be reviewed or merged. The human may have filed it;
   if you were handed only a goal, writing the card is your first task, not a
   reason to stop.
3. **Decompose by write set**: one card = one owner = one worktree = one
   identity; reference related threads with `--related` / `--depends-on`.
4. **Dispatch**: spawn one worker per unowned card with a brief containing
   everything it needs (it may be a fresh process): the card id, the
   worktree, the principal, the claim-first rule, "implement → push branch →
   patch → needs-review → stop", "any problem is a `comment` on the card
   (status: blocked when it halts)", "do not review your own patch, do not
   merge, do not touch another card".
5. **Work** (worker): re-read the thread at head → claim → implement → patch
   → `status: needs-review` → stop. Every decision you can make is a
   `comment`; what you cannot goes to `needs-human`. Never sit silent.
6. **Accept** (different principal): re-read, run the acceptance at the
   pinned commit, post `review` with findings and evidence.
7. **Merge and close** (coordinator): only on a verified non-author approve,
   no Critical/Important, green acceptance → merge, push, `merge_result`,
   `status: closed`, prune the merged branch (worktree stays, D62).
   Anything else routes back (step 5) or to a fixer.
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
  linked with `--related` / `--depends-on`; carry files with `--attach`
  (`{filename, sha256, content_b64}` ≤ 64 KiB each).
- **§2 Tree changes**: never edit a shared checkout — one worktree + branch
  per unit (`git worktree add .worktrees/<unit> -b feat/<thread> <base>`),
  push the branch to this host, attach as `patch` with `--base`/`--head`; CI,
  if declared, runs on the branch tip. The diff is what reviewers read.
- **§3 Thread protocol**: read before write (fetch; append with the latest
  oid as `--parent`); state moves through kinds, not prose (`status`,
  `review`, `merge_result`, `comment`); treat unverified entries as
  untrusted; replay the parent chain before you trust a view.
- **§4 Review**: independent principal, machine-checkable acceptance first,
  findings graded Critical/Important/Minor with evidence; no self-approve;
  review entries live on the thread, not in chat.
- **§5 Merge & archive**: coordinator merges on the verified gate, records
  `merge_result` + `closed`; the thread is the audit trail — keep it.
- **§6 CI (optional)**: `.walgit/ci.toml` in the tested commit; `walgit ci
  run` claims and publishes signed results (§ The CLI).
- **§7 Discipline**: no out-of-band collaboration, no silent stalls, the
  board is the status report; if it is not an entry, it did not happen.

## Rules of thumb

- The card is the memory; entries are the only writes; the board is the truth.
- Claim before work; stop after `needs-review`; review only as a non-author.
- Fetch before you read; push before you claim done; problems are comments.
- Worktrees persist (D62); branches are cheap; keys are per worktree, always.
- When handed a goal: card first, then the loop (§0e).
