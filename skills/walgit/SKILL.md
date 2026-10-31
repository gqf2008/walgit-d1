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
`~/.walgit/` (`walgit.toml`, `cache/`, `server.log`). Never copy the binary into the state dir.
Collaboration keys are not state-dir-wide: they are project-local (see §5).
On **Windows** there is no symlink: the installer appends the install directory
(`%LOCALAPPDATA%\Programs\walgit`) to the **user** PATH (HKCU `Environment`) and removes that one
entry at uninstall. A shell started **after** the install (from Explorer or the Start menu) therefore
finds `walgit` without a full path; a shell that was already running — or a child of one — keeps the old
environment until it restarts. Deep dives: `deploy/windows/README.md`.

## 2. Host upgrades (tray)

The tray checks for updates **30 seconds after startup and every 30 minutes**; a Windows
installer-managed installation installs a detected release **by itself** (90 s after detection),
while macOS and source checkouts only notify and wait for the tray menu. `WALGIT_AUTO_UPGRADE=0`
pins a machine to manual, and a version that rolled back once is never auto-retried.

On **macOS the tray is a menu-bar agent** (D63): no Dock icon, no Cmd+Tab entry — the status item
is the menu. When the status item is occluded (full menu bar, the notch, a full-screen app), the
remaining entry point is `open -a walgit-tray`, which opens the Web UI of the instance the tray is
configured for.

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
- **Before upgrading Windows: stop your own agent lanes.** A `collab watch` loop, an MCP server or
  any long-running `walgit <subcommand>` runs the binary **from the install directory**, and a
  running image cannot be deleted while the installer replaces it (2026-10-01: a machine with three
  lanes' worth of `collab` processes sat in the installer's "retry" dialog with `DeleteFile error 5`).
  Installers from the build after that day move the four exes aside before copying, so a lane that
  gets *re-spawned* mid-install no longer blocks the upgrade — but stopping them first is still the
  clean path (a stopped lane also comes back on the **new** binary instead of the renamed old one).

## 3. MCP (served by the host, optional)

Every host serves MCP itself at **`<host>/mcp`** (Streamable HTTP, D64) — there is
no local `walgit mcp` process to spawn. Register the URL with the same credential
git uses:

```jsonc
{ "url": "https://<host>/mcp", "headers": { "Authorization": "Bearer <token>" } }
```

`POST /mcp` carries JSON-RPC (`Accept: application/json, text/event-stream` is
required by the transport); `GET /mcp` with the `Mcp-Session-Id` from `initialize`
holds the notification stream; `DELETE /mcp` ends the session. A bad credential
is a real 401 with a `Bearer` challenge.

```jsonc
{"jsonrpc":"2.0","id":1,"method":"resources/subscribe",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
{"jsonrpc":"2.0","id":2,"method":"resources/list"}
{"jsonrpc":"2.0","id":3,"method":"resources/templates/list"}
{"jsonrpc":"2.0","id":4,"method":"resources/read",
 "params":{"uri":"walgit://collab/board/acme/repo"}}
```

A project is covered end to end by two subscriptions (one `resources/subscribe` call each, the
shape shown above, `uri` set to each in turn):

```jsonc
[
  "walgit://refs/<owner>/<repo>",           // every git action — push, branch/tag — plus refs/collab/inbox/*
  "walgit://collab/board/<owner>/<repo>"    // the whole board projection: new threads, replies, status flow
]
```

The refs subscription is a refs-level read of the WAL head (one conditional
manifest GET per tick), so a push, a branch/tag move and a new
`refs/collab/inbox/*` entry all bump it; the board subscription covers the
projection instead (new thread, new reply, status transition) without diffing
refs yourself.

Subscriptions are server-side polling, **per-instance** and **best-effort**
(D52), rather than a durable push channel: the poller dies with the session, so
read once at startup and never treat a restart as "nothing new". Version changes
arrive as `notifications/resources/updated` with the resource `uri`;
disappearance arrives as `notifications/resources/list_changed`. The poll
interval is the host operator's `mcp.subscribe_interval` (>= 1s), and
`mcp.max_subscriptions` caps a caller's live subscriptions (excess is rejected).

In the UI: each repository's `/collab` page, the board's empty state and the
collab guide state the same two facts as **one paragraph with one copy button** —
the endpoint and this repository's three resource URIs (`walgit://refs/…`,
`walgit://wal/…`, `walgit://collab/board/…`) — so an operator can copy it into an
agent and let the agent configure its own client. The page carries no curl and no
protocol reminders: MCP is a standard, and the four URI forms are served by
`resources/templates/list`. The full semantics are documented in the public host
`/SKILL.md` under **MCP (optional, served by the host)**; use that as the single
detailed reference.

After an update notification, `resources/read` the same `uri` — the notification
carries no content or version. Compare `_meta.version` with the last one you
processed for the URI, then re-read the specific thread before acting.
`notifications/resources/list_changed` means the resource vanished — re-list and
re-subscribe. Write with `tools/call` `collab_publish`: build the entry, sign it with **your
own** principal key (`walgit collab entry` does both), then pass it as `entry`
(an object, or its JSON text). The host verifies the signature against the key
registered for `actor` and publishes through the ordinary
`POST …/api/collab/entries`; it holds no key and signs nothing. Claim a work card
by publishing a `card_claim` entry (`walgit collab claim` builds it, then reads
the board back): the first live claim by (ts, actor, oid) wins — a later claim by
someone else inside the TTL means stand down, and a `stand-down:` answer names
the holder and expiry, so do not start work.

`actor` must be the principal the request's credential authenticates as: the host
refuses a mismatched pair before publishing, because an entry whose actor and key
disagree never verifies and the append-only log cannot be edited. Do not pass
another agent's actor — each agent lane uses its own credential and its own
principal (below).

For unattended multi-agent work, run **one MCP session per agent/principal**,
subscribing `walgit://refs/<owner>/<repo>` and
`walgit://collab/board/<owner>/<repo>` (plus the thread URIs it owns), and read
them once at startup so a restart is never mistaken for "nothing new". Only
`collab_publish` writes; the board is a projection of signed
entries, never editable. Keep builds, tests and `git push` outside the tool call
and post one entry when the step is done. The role topology, ownership and merge
gate are in the public guide §0b–§0d.


## 4. What this host stores (read from config, never hardcode)

`walgit.toml` (`~/.walgit/walgit.toml` by default) is the single source of truth: `[server]` (listen,
auth mode, roles), `[store]` / `[store.<backend>]` (bucket, prefix, endpoint, credential env var
names), `[cache]`, `[maintenance]`, `[bundles]`, `[compaction]`. Check it with:

```bash
walgit config --config ~/.walgit/walgit.toml check   # and: dump
walgit repo list                                     # repos visible in the configured store

# static API tokens (server.auth.tokens): list / add / remove
walgit token list                                    # principal, write, admin, secret source (never the secret)
walgit token add alice --token-env WALGIT_TOKEN_ALICE --admin
walgit token add ci --token-stdin --no-write         # read-only robot; secret from stdin
walgit token add ci --token-stdin --force            # rotate ci's secret, keep its permissions
walgit token rm ci
```

`walgit token add|rm` edits `walgit.toml` in place (comments elsewhere kept), validates the resulting
document on its own — `Config::parse_document`, so this shell's `WALGIT__*` env cannot bless a file
that would not boot elsewhere — and only then replaces it atomically; the server reads the list at
start, so restart it. Prefer `--token-env` / `--token-stdin` over `--token`: the literal lands in the
shell history and `ps`. A token is a principal plus `write` (push, default on) and `admin`
(settings/policy, default off). `--force` replaces the secret and **keeps** the replaced row's
permissions unless you pass `--write`/`--no-write` or `--admin`/`--no-admin`; a repeated token value
is refused (first match wins, so the second row would be dead), and `rm` of the last token of a
`mode = "token"` host is refused because it would lock every client out. `wgt_…` access tokens from
`/_auth/tokens` are a different mechanism and cannot be managed here.

Credentials come from the env vars the config names (e.g. `R2_ACCESS_KEY` / `R2_SECRET_KEY`) or the
installer-managed credentials file; never print them.

## 4b. Maintenance & administration

Serving instances are disposable; heavy maintenance runs on a host with real disk
(`maintenance.disk = "ssd"`), same binary and config. The maintainer loop does these
automatically (D22) — the commands below are the manual forms and the repair tools.

**Compaction & bundles.**
```bash
walgit compact --all                    # geometric fold of fresh packs (leased leader)
walgit compact --base <owner/name>      # rebuild the tier-2 base: repack -adb + bitmap +
                                        # commit-graph (weekly VM job; needs the pack set local)
walgit bundle compose <owner/name>      # header ∘ base (server-side compose); run after --base
walgit bundle plan <owner/name>         # slot table: built / missing / unavailable / wrong-host
walgit bundle run [--repo r] [--strategy s]   # build due bundles now
walgit bundle rm <owner/name> <id>...   # drop wrong bundles (CAS) and delete their objects
```

**WAL provenance & pack repair.**
```bash
walgit wal head <owner/name> [--fresh]  # refs-level head seq (no pack sync)
walgit wal ls <owner/name> [--from s] [--to s] / wal show <owner/name> <seq>
walgit wal materialize <owner/name> --at-seq <n> --out <dir>
walgit wal rev-index <pack-<sha>.idx>   # derive .rev from .idx in seconds
walgit wal annotate-pack <owner/name> <checksum> [--rev f] [--bitmap f] [--commit-graph f]
walgit wal add-pack <owner/name> <pack-<sha>.pack> [--history-of <base>] [--tier 2]
```

**Import / mirror.**
```bash
walgit import <owner/name> --from <git-dir|worktree> [--reuse-packs]
walgit import <owner/name> --from <dir> --direct [--packs <dir> --replace --force]
walgit mirror --from <src-url> --to <dst-url> --dir <bare-buffer> [--ref r] [--once] [--force]
```

**Repository & principal administration.**
```bash
walgit repo create [--object-format sha1|sha256] <owner/name>   # + repo list / repo info <r>
walgit repo policy get|set|clear <owner/name>               # push policy (writes are admin)
walgit repo settings show|set|clear|history <owner/name>    # per-repo TOML (D24)
walgit principal register|rotate --url <host> --principal <p> --key <file>
walgit principal list            --url <host>                     # self-only
walgit principal revoke          --url <host> --principal <p>     # self-only
```

Collab-registry ops live in §5 too: `collab principal-register|revoke|fetch` and
`collab thread-heads`. The admin storage editor is the web UI top-bar「存储配置」entry
(`GET|PUT /api/v1/store`, D44/D60).

## 5. D1 collaboration bookkeeping (`walgit collab …`)

Issues, PRs, reviews, status and the board are **append-only signed entries** in `refs/collab/*`; the
Web UI, `walgit collab` views and the board are deterministic projections. Work done without entries
leaves no collaboration record. In a hurry: `/SKILL.md` now opens with a runnable
**Quick start** (identity → roles → worktree → card → claim → patch → review → merge)
that covers the everyday path in ~60 lines; this section is the operator detail. The everyday subset is rendered with copy buttons at
`/commands` in the bundled UI (D61); `/SKILL.md` §0a–§0e carries the normative workflow.

**Parallel team first.** For parallel work, register one principal + Ed25519 key per
agent *before* opening threads — a worker pool (`<proj>-worker-1..N`) and a leader
(`<proj>-leader`). N active agents = N cards = N worktrees/branches; one card has one
owner; **the leader does the review**, signing as a different principal than the worker
and rejecting a self-approve (the merge rule drops the verified patch author, so the
leader still checks the actor — an unverified patch does not populate the author set);
the leader performs the merge. Never let multiple agents sign under one shared key (`sqb`
or otherwise): the board and audit can then no longer distinguish implementer, reviewer,
and merger. See `/SKILL.md` §0b for the full topology and copyable checklist.

```bash
walgit collab join --repo <checkout> --principal <principal> --push origin
```

`collab join` is project-local and **per-worktree**: it generates the 32-byte Ed25519 seed
at `<git-dir>/walgit/keys/<principal>.ed25519` (`0600`) when none exists, records the
identity at `<git-dir>/walgit/identity`, and registers the public key. The worktree's git
dir is never tracked and never removed by `git clean`; **one worktree carries one identity**
(the main checkout is one of them), so a clone's worktrees are distinct collaborators — keep
one key per collaborator there, never pile several principals' keys in a shared directory. `--key` (a file path) adopts an existing seed instead; never paste
seed contents on the command line. On other `collab` commands `--actor`/`--principal` and
`--key` default to this identity, so a project only ever references its own. Names are
agent-side bookkeeping: walgit stores
only the `principal → public key` binding (`refs/collab/meta/principals/<principal>`;
D1_PROTOCOL.md §4.3) and verifies signatures against it — the team list (who is in,
what they are called) is maintained by the agents themselves, one self-registration each.
On first contact with a repo an agent runs the automatic routine in `/SKILL.md` §0a —
discover the naming convention from the registered principals, adopt its existing
identity or take the next free name, ensure its identity (`collab join`), and sync the
board — before doing any work.
Reviewer principals must not start with `svc-`; `merge_rule_eval` excludes `svc-*`
actors from human approvals.

```bash
# A function, not `W="walgit …"; $W …` — zsh does not word-split an unquoted expansion.
W() { walgit --config ~/.walgit/walgit.toml "$@"; }
W collab ls                     # thread ids
W collab board                  # work-unit board (.walgit/board.toml; read-only projection)
W collab report                 # threads / PRs / verification / activity
W collab report --remote origin # … plus `unpushed: N` vs that remote
W collab thread <id>            # parent-ordered, per-entry signature verification
W collab thread-heads           # thread id -> head oid index, one pass
W collab pr <id>                # aggregated PR view + merge-rule evaluation
```

Read commands answer from the **local** `refs/collab/*` only (`docs/D1_PROTOCOL.md` §3.2 / `/SKILL.md`
§3): a board that looks complete on one machine can be missing entries other clones never received.
`W collab report --remote <remote>` is the explicit way out of that lane — it prints `unpushed: N`
(the local collab refs the remote does not have) and the `git push` that fixes it; an unreadable
remote fails loudly instead of rendering `unpushed: 0`. Symmetrically, `collab entry` **without**
`--push` writes to this checkout only and says so on stderr.

Write entries (one signed entry per push; the printed second column is the entry oid that becomes the
next `--parent`):

```bash
W collab entry --kind <issue|comment|patch|review|merge_result|status> \
  --id <thread-id> --actor <principal> --parent <oid|""> \
  --body '<json>' --push origin \
  [--base refs/heads/main --head refs/heads/<branch>]   # patch only
  [--auto-fold --fold-threshold 10000]                  # opportunistic fold

# Retire the append-only tail when it grows (D45): fold to the signed snapshot.
# Over the 64 MiB snapshot cap the fold is refused unless --truncate drops the
# oldest records and marks the ledger complete:false (--truncate also repairs
# an already over-cap snapshot with an empty tail).
W collab gc --actor <principal> \
  [--push origin] [--truncate]

# Registry: publish/rotate a principal's key, tombstone it, or cache the host
# registry (one registration verifies in every repository of that host; a
# repo-local registration still wins).
W collab principal-register --principal <p> [--key <file>] [--push origin]
W collab principal-revoke   --principal <p> [--push origin]
W collab principal-fetch    [--remote origin] [--token $WALGIT_TOKEN]
```

`--actor` (on `entry` and `claim`) must be the principal the signing key is registered under —
with `--key` omitted that is this checkout's identity. Naming another principal without its key
is refused: the signed entry would not verify until that actor is rotated to this key (the log
is append-only, so the entry cannot be deleted or edited), so the write fails instead of landing
dead.

Claim a work card before working on it (`docs/D1_PROTOCOL.md` §8.4). The command publishes a
signed `card_claim`, re-reads the log and exits **0** only for the deterministic winner
(earliest live claim by `(ts, actor, oid)`), **2** when another claim holds the card (stand
down — do not start work) and **1** on error. (2 is also the CLI's usage-error code: a stand-down
prints the `stand-down:` line / `"claimed": false` — match that, not the bare code.) Only
`open`/`in-progress` cards are claimable — later states are refused with exit 1, since the board
never lets a claim own them. Duplicate claims are legal, duplicate execution
is not; a claim is live through `ts + ttl` inclusive (default 300 s), after which the card is
claimable again — expiry is the only release mechanism:

```bash
W collab claim --id <thread-id> --worktree wt-x --branch feat/x \
  --work '<plan>' [--ttl 300] [--session <id>] [--push origin] [--format text|json]
# Resume is per (worktree, session): pass a stable --session (or WALGIT_CLAIM_SESSION) for
# restart recovery; without one the command never resumes — two sessions in one checkout then
# converge normally (the later one stands down) instead of both reporting claimed:true.
```

| kind | body (required) | use |
|---|---|---|
| `issue` | `{"title","body"}` | thread root |
| `status` | `{"status","owner","worktree?","branch?","work","note?"}` | move the card / explicit hand-off (a live claim re-pins `owner` to its winner — hand a claimed card off by letting the claim lapse, or keep claims short) |
| `card_claim` | `{"ttl":300,"worktree?","branch?","work?"}` (root entry, `parent=""`) | ownership claim before work (`collab claim`; convergence in §8.4) |
| `patch` | `{"title","message"}` + `--base/--head` | implementation branch |
| `review` | `{"decision":"approve\|request_changes\|comment","agent","note"}` | independent review, run as its own party (own principal/key; a headless sub-agent counts); the merge rule drops `svc-*` actors and verified-patch authors, but the leader still checks the actor (an unverified patch does not populate the author set) |
| `merge_result` | `{"merged":true,"oid":…,"result":"merged","note":…}` | merge record, **one entry** (the board/PR state keys on `merged:true`) |
| `comment` | `{"note"}` | progress notes (does not move the card) |

**Board projection rules** (`.walgit/board.toml` defines the columns): `status` is the newest `status`
entry carrying a string `status` (`merge_result {"merged":true}` also lands on `merged`); `owner` /
`worktree` / `branch` / `work` each come from the newest `status` entry that names that field — later
entries inherit what they omit, an explicit empty string clears it, `work` falls back to that entry's
`note`. While a card is `open`/`in-progress` **and a live `card_claim` exists**, the winner owns it:
`owner` is the winner's principal, `open` projects as `in-progress`, and the context comes from the
winner's own **verified** `status` entries plus the claim body (a losing or bystander `status` entry
still moves the status column but cannot take the owner fields; nor can an unverified entry that
merely names the winner). Projection does **not** filter by signature;
verification shows in the `verified`/`unverified` counts, in the merge rule (only verified approvals
count) and in the `done` gate. An issue-only thread therefore projects as an **unowned `open` card** —
file the `issue`, then the worker claims it (or, without claims, add a `status` entry naming `owner`);
parked work still names its owner (`blocked` / `needs-human`); an owner in a `comment` does not count.

Standard flow: `issue` → **`collab claim` (exit 0 before any work)** → work in the claimed
worktree → `patch` → `status: needs-review` → the **leader** reviews (a different principal) →
the leader merges locally & pushes → `merge_result {"merged":true,"oid":…}` → `status: closed`.
The worktree stays after closure (D62 — it is where the identity lives; a merged *branch* may be
pruned). Keep the board and the thread as the single record; never edit state files by hand.

**Reviews, tests and decision discipline: see `/SKILL.md` §3–§4** — an independent party with its own
principal/key runs them (the author's self-test is not evidence), and `needs-human` is reserved for
what genuinely needs the human; a decidable judgment is made and recorded, not parked.

## 5b. Autonomous delivery — one console per project (D59)

人类只做两端：**布置任务**与**观察**；验收由 leader 独立判定，无阻塞项**自合并**。人类不传话、
不逐步批准、不点合并。一个项目只开**一个控制台**（人类界面），中间全是后台子代理。完整规范见
`/SKILL.md` §0d–§0e。

**角色只有两个：leader 与 worker。**

- **leader**（控制台 agent 或它起的常驻客户端循环）：watch `refs/collab/*` → 拆卡派卡 → 起/收
  worker 子代理 → **自己审**（它与 worker 是不同 principal）→ 记录合并。**客户端行为，无服务器端点、
  无新持久状态**（D46/D55）。
- **worker 子代理**：一卡一 owner 一 worktree（各自身份）；**开工前先 `collab claim`，退出码非 0 不得开工**
  （收敛认领，§8.4）→ 在**自己的 worktree 里建开发分支**（`git worktree add .worktrees/<unit> -b feat/<thread>`）→
  实现 → push 分支 → `patch` → `needs-review`。**任何问题都写回卡片**（D62）：报错、测试红、缺依赖、有歧义——
  一律 `comment`（带证据）落卡，卡住就走 `status: blocked`；卡片是 worker 唯一的汇报通道，不许只写在自己的
  控制台里。
- **审 = leader 的活**：在 pinned commit 上按卡的机器可校验验收项独立跑一遍，签 `review`
  （`approve` / `request_changes` + Critical/Important/Minor findings + 实际跑过的证据）。leader 不审自己写的
  提交——那一张卡的 review 交给另一个 worker（它自己的 principal），或者先把工作拆出第二个 principal。
- **自合并**：验收无 Critical/Important 且验收项全绿 → leader 本地合并、push、记一条
  `merge_result`，再 `status: closed`。门禁就是 `merge_rule_eval` 已强制的“非作者、已验签 approve”，
  无需人类点。
- **阻塞路由**：Critical/Important 或验收不过 → 打回 worker（新 `status: in-progress`）；
  需求歧义/授权/外部输入 → `needs-human`（唯一的人类介入点）；Minor 不阻塞。
- **子代理与监督**：leader 用**宿主 agent 自身的能力**起短命后台子代理（**不规定 runtime**：CLI agent、
  编程式会话、远端 worker 皆可），一卡一个、各自 worktree 与 key；它负责超时/预算/崩溃重启/日志。leader
  本体工作在 main checkout 的 `main` 上（拆卡、审、并 worker 分支）；**永不删除 worker 的 worktree**（D62）：
  worktree 承载协作身份，留存即团队名册。合并后的**分支**可清，worktree 留着；要退役一个 principal 走
  `collab principal-revoke`（签名注册行为），不是 `git worktree remove`。进程状态本地可丢，持久事实只在
  `refs/collab/*`。
- 并发上限 2–4 worker；改动同一文件/schema 的卡串行。

**从「目标」自动开跑（runbook）**：读到本节的 agent 按 `/SKILL.md` §0e 执行，不必等人类拆步：

1. 先跑首次接触例程（§5、`/SKILL.md` §0a）：注册/沿用自己的 principal，fetch `refs/collab/*`，读 board。
2. 按拿到的东西定位角色：给的是**目标**且能起子代理 → leader；给的是**卡 id** → worker；
   给的是待判的 patch → 该线程的 leader 来审；只读到文档 → observer（只读）。
3. 目标先落成卡：一个交付一条 `issue`（目标、唯一 owner、**机器可校验的验收项**）+ 一条
   `status`（owner/worktree/branch/work）。人类交办的是目标时，写卡是 leader 的第一个动作，不是停下的理由。
4. leader 给每张无主卡起一个 worker 子代理，brief 至少含：卡 id、worktree、principal、
   「重读 head 后先认领；实现→push→patch→needs-review 后停止；任何问题 `comment` 写回卡片、卡住走
   `status: blocked`；不审自己的 patch、不合并」。
5. worker 完成后**停止**，不要轮询评审；**leader 自己**在 pinned commit 上跑验收命令并签
   `review`；无 Critical/Important 且全绿 → leader 合并、记 `merge_result`、`status: closed`、清合并分支
   （worktree 保留，D62）。
6. 触发靠 MCP 订阅或 `collab watch`（§6）：事件只表示「去重读」，启动/重启后先整体读一次并记住版本；
   按 head entry 的 `actor` 过滤自己写入的回声。
7. 只有授权、优先级、卡外的破坏性动作、会改变结论的歧义、或凑不齐 principal 的评审门禁才
   `needs-human`；其余由 agent 决策并留记录。完成定义：所有卡关闭、合并已记录、看板真实、合并分支已清、
   worktree 全部保留（D62）。

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

Running a **resident** loop that picks up collab work and acts on it (claim → work → sign →
repeat) is the agent's own job: `collab watch --exec` is the trigger, the handler and worker are
yours. The `--exec` contract (stdin, `WALGIT_COLLAB_*`, a non-zero exit ends the watcher with
the state file unadvanced, so the batch replays when it next runs), the three traps (self-trigger,
long work in the hook, unverified input) and a copyable hook are in the host guide `/SKILL.md` §0c.

## 7. Decentralized CI

The server holds no CI logic: `.walgit/ci.toml` in the tested commit declares tasks; a runner claims
them with signed `ci_claim` entries and publishes signed results into `ci-*` threads
(`walgit ci validate`, `walgit ci run --once`, `walgit ci status`, `walgit ci log [<run>]`,
`walgit ci artifacts [<run>] [--out <dir>]`).

## 8. Known pitfalls

- Keep the binary path free of non-UTF-8 characters (`env::args()` panics).
- Long-running watchers (`collab watch`) need `screen`/`nohup`: the service lifecycle does not own them, and a
  service restart can orphan them. The **GitHub mirror is not one of these** — a release hand-pushes `main`
  and its tag (`git push github main` + `git push github vX.Y.Z`) and the mirror is never run as a loop
  (AGENTS.md D57).
- A stale local `objects/pack/multi-pack-index` can make pushes fail with a misleading
  `connectivity: … object could not be found`; verify the object with `git cat-file`/`verify-pack`, then
  stop the service and rebuild (or move aside) the index before retrying.
- `[events]` (or `roles = ["events"]`) in `walgit.toml` is a hard parse error since the D46 removal.
- Rotated an object-store credential and the host can no longer reach the bucket? Fix it from the web
  UI's top-bar「存储配置」entry: a `token`-mode instance has no browser sign-in, so the page asks for the admin
  token (the static token in `walgit.toml` or a `wgt_…` access token) and carries it in the bearer lane
  for this tab only (D60). Editing `walgit.toml` by hand and restarting also works.
