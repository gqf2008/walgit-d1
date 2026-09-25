# walgit — a git server that is one binary in front of an object store

[![CI](https://github.com/gqf2008/walgit-d1/actions/workflows/ci.yml/badge.svg)](https://github.com/gqf2008/walgit-d1/actions/workflows/ci.yml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows%20%7C%20macOS-blue)](README.md#platforms)
[![Agent-native](https://img.shields.io/badge/agent--native-work%20units%20%26%20protocol-purple)](AGENTS.md#6-agent-collaboration-protocol)

walgit hosts git repositories with **no database, no leader and no local state that matters**. You run a
single binary, point it at an S3 or GCS bucket, and you have: smart HTTP (v0/v2) fetch and push, `bundle-uri`
clones served as static files, Git LFS, a browsing web UI, a JSON API with an SDK, per-repository push policy
and a server that scales to repositories **larger than the machine it runs on**. Every machine that runs
walgit is a disposable cache; the bucket is the repository.

```sh
# 1. a bucket (any S3-compatible store or GCS) and a config
cat > walgit.toml <<'EOF'
[server]
listen = "0.0.0.0:8080"
public_url = "https://git.example.com"
auto_create_on_push = true
[server.auth]
mode = "token"
anonymous_read = false
tokens = [{ principal = "me", token_env = "WALGIT_TOKEN_ME", write = true }]
[store]
backend = "s3"
bucket = "my-walgit"
[store.s3]
endpoint = "https://s3.us-east-1.amazonaws.com"
region = "us-east-1"
EOF

# 2. run it
WALGIT_TOKEN_ME=$(openssl rand -hex 24) walgit serve --config walgit.toml

# 3. use it — a push to a new name creates the repository
git -c http.extraHeader="Authorization: Bearer $WALGIT_TOKEN_ME" push https://git.example.com/acme/app.git main
```

That is the whole deployment. Add more machines pointed at the same bucket and they serve the same repositories,
consistently, with nothing to coordinate. Kill them all and you lose warmth, nothing else.

It is a Rust implementation of the architecture Cursor described in
[*Git at any scale*](https://cursor.com/blog/git-at-any-scale) (the system they call Continuity), with the changes
needed to run it on machines that are smaller than the repository. The post is worth reading first; it is kept
verbatim in `docs/reference/cursor-git-at-any-scale.md`.

---

## 关于这个仓库 — walgit-d1（分叉说明）

**这是 `tobi/walgit` 的一个独立分叉**（仓库名 `gqf2008/walgit-d1`，命令与二进制仍叫 `walgit`）。

上游 walgit 的目标是**把服务端做小**：git 托管、bundle-uri、LFS、Web UI，仅此而已；
代码评审 / CI / issue 按上游 `GOAL.md §4` 明确**不在范围内**（principle X “keep walgit small”）。

本分叉在上游那套对象存储 Git 之上，加了**去中心化协作层（代号 D1）**——它不引入独立的
协作状态或聚合服务，只在 walgit 现有服务上增加一层薄 API，并把整套东西做成了可分发的桌面产品。

因此它与上游在定位上已经分道扬镳，**不打算向上游回并**；取名 `walgit-d1` 就是为了和上游区分开。

### 上游基线

分叉点在 `6d8fa54`（2026-08-26），此后本分叉领先上游 132 个提交
（`git rev-list --count upstream/main..main`）。

内核的**语义与不变式**不变（桶仍是唯一事实源、manifest CAS 仍是提交点、实例仍是可丢弃缓存），
但**改动并不止于新增一层**：相对分叉点共 239 个文件、+46.7k/−4.7k；即便只粗扣
`collab.rs`/`ci.rs`/`collab_cmd.rs`/`ci_cmd.rs` 四个核心文件，剩余仍有约 37k 行
（其中还含协作层的 Web/SDK/测试），包括 Windows 原生支持
（`crates/walgit-wal/src/platform.rs`）、publish/sync/handle 的可靠性修复（#148 重启后
refs 回退、#36 幻影 refs、#144 跨卷原子 rename）、对象存储健壮性（#129/#130 socket
超时与配置写硬化）以及站点与部署面扩展。

准确的说法是 **D1 是最大的增量，不是唯一增量**；"内核没变"应读作"内核契约没推翻"，
而不是"内核代码没动过"。

### 新增功能

**D1 协作层** — 没有协作服务器：协作状态是仓库 `refs/collab/*` 里的**签名、追加式 git 对象**，
权威永远是这些 ref，服务端**不持有**协作状态或聚合结果。协作 ref 不在默认 refspec 里，
拿到它们需要显式拉取：

```sh
git clone <repo> && cd <repo>
git fetch origin '+refs/collab/*:refs/collab/*'
```

之后即可离线验签、重算出与别人一致的视图。若公钥只注册在 host 级 registry
（`refs/walgit/principals/*`，不在上面的 refspec 里），验签前先跑一次
`walgit collab principal-fetch` 把它拉到本地。

服务端为 Web UI 提供的 `/{owner}/{repo}/api/collab/report`、`.../collab/board`、
`.../collab/threads/{id}` 是**无状态聚合读端点**，与 CLI 共用 `walgit-wal::collab`
同一份纯函数，不落库、不作为权威：

- **issue / PR / 评审 / 线程**：`walgit collab` 下的 `thread`、`pr`、`entry`、
  `board`、`report`、`gc`（折叠出 `refs/collab/meta/snapshot` 快照）、`watch` 等命令。
- **看板**：`.walgit/board.toml` 里的声明式列定义，将线程集合折叠成确定性投影
  （见 `docs/BOARD.md`）——板不是状态，是纯函数。
- **身份**：host 级 principal 注册表（`walgit principal`），一个 token 同时覆盖
  git 读写与协作读写；支持签名公钥注册/吊销。
- **D1-CI（去中心化 CI）**：**服务端零 CI 执行**（没有 runner / 调度 / 秘密；聚合随 walgit 编译，
  只在 report runs 投影里跑）。认领与结果都是 `refs/collab/inbox/*`
  里的签名条目，由客户端 runner（`walgit ci`）认领、执行被测提交里的 `.walgit/ci.toml`、
  签名回传；收敛靠对条目日志的确定性规则，不靠互斥。规范见 `docs/D1_CI_PROTOCOL.md`。
- **Web UI**：协作页、线程/PR 页、看板页、以及面向人类的「了解 D1 协作」讲解页
  （`/{owner}/{repo}/collab/guide`）。

**首次运行的部署向导** — `walgit-server` 的 setup wizard：新部署不再要求手写完整
`walgit.toml` 才能起服务，走 `/setup` 向导配置 store 与认证。

**分发与桌面** — 上游有二进制、Containerfile 与 Nix，但没有最终用户安装器；本分叉补齐了这条路：

- **跨平台托盘**：macOS（Swift）、Windows/Linux（Rust）三平台系统托盘，
  启停服务与版本检测（`deploy/tray/`）。**点击升级在 Windows/Linux 依赖本地源码仓库**，
  经安装器部署、没有源码树的机器只能重跑安装器；Release 感知的自动下载升级目前仅 macOS。
- **macOS Release 感知升级**：托盘同时比对 GitHub Release 与源码仓库，
  下载 → 严格校验（sha256 / 版本 / 签名 / 公证）→ 原子换装 → 健康检查，失败回滚。
- **安装包**：Linux `.deb` 与 Windows Inno Setup 安装器由 `release.yml` 在打 tag 时构建；
  macOS 签名+公证 DMG 由 `deploy/tray/macos/build-dmg.sh` 在 CI 之外构建后上传 Release。

**工程与治理** — 上游没有这些；本分叉按 agent 协作的方式补上：
issue/PR 模板与批次化流程、`AGENTS.md` 协作协议、CI 分级（fast tier / e2e /
windows fast tier）、CodeQL、Dependabot、`code review` 与发布规范、Windows 开发
runbook（`docs/WINDOWS.md`）等。

### 文档

- `docs/USER_GUIDE.md` — 面向人类的完整使用手册（协作层怎么用）。
- `docs/D1_PROTOCOL.md` — D1 协作层规范；`docs/D1_COLLAB_DESIGN.md`（设计背景）、`docs/D1_CI_PROTOCOL.md`（CI 子协议）。
- `docs/BOARD.md`、`docs/POLICY.md` — 看板 / 推送策略。
- `AGENTS.md` — 架构、所有设计决策、以及 agent 协作协议。
- `GOAL.md` — 上游的验收目标（本分叉保持其内核语义不变）。

### 与上游的关系

`upstream` remote 指向 `tobi/walgit`，仅用于查阅与偶尔同步内核修复；
**不接受也不发起回并**。若你想用上游那份“只有 git 托管”的版本，请直接用上游仓库。

---

## Why this shape

Git is distributed, and that makes hosting it miserable for one reason: **packfiles**. Everything in a repository
is compressed into large binary packs laid out to be small, not to be read in order; every git operation is a
random walk over gigabytes. That is fine on a laptop with the file in page cache and catastrophic over a network
filesystem, which is why "just put the repositories on NFS" failed at every large host that tried it. The design
that survived (GitHub's Spokes) keeps real repositories on local NVMe so upstream `git` does the work, and
replicates at the packfile level with strict consistency — paid for with three-phase commit across a fixed replica
set, a database that maps every repository to its machines, and a fleet of pets.

Continuity's insight changes the economics: **make a write-ahead log in object storage the source of truth, and
make every on-disk repository a cache.** A push is stored as an immutable object in the bucket and becomes visible
only when a tiny manifest is rewritten with a compare-and-swap. That CAS *is* the consensus — no election, no
quorum, no primary. Any instance may accept a push; two racing instances cannot both win. A replica that has never
seen a repository reads the log and has it. Reads are consistent without coordination because every read first
asks the store whether anything changed (a conditional GET, usually a 304). Compaction is done once by whoever holds
a lease and published *into the log*, so replicas download compacted packs instead of repacking. And because the
WAL is the truth, there is complete provenance: every push and every repack, replayable to any point.

walgit takes that as-is, and adds what a *monorepo on small machines* needs: serving refs and web pages for a
repository whose packs will never fit on the instance (a **remote reader** over HTTP range requests), keeping
commits and trees local while blobs stay in the bucket (the **history pack**), and moving clone bytes out of the
server entirely (**bundle-uri**: fresh clones and catch-ups are static files the bucket or a CDN hands out).

## What it does

| | |
|---|---|
| **git** | smart HTTP v0/v2: `ls-refs` with prefixes, fetch with filter/shallow/deepen/sideband-all, receive-pack (atomic, deletes, tags, push options, report-status-v2), `<owner>/<repo>` namespaces, sha1 and sha256 repositories. Upstream `git` does upload-pack/repack/bundle; walgit does receive-pack, the WAL and the plumbing. |
| **bundle-uri** | Bundles cut on calendar slots (weekly full, chained dailies, hourlies) as a pure function of the WAL: a fresh clone downloads the newest full plus the chain above it from the bucket and asks the server only for the remainder; a catch-up downloads exactly the slots it missed. Two lists per repo: `bundles/list` for clones, `bundles/catchup` for fetches. Blobless families for `--filter=blob:none`. |
| **LFS** | Batch API + basic transfer, objects in the bucket, optional read-through from an upstream LFS server for imported repositories. |
| **web UI + API** | A React UI (tree, blob, commits, diffs, the WAL's own health page) on a read-mostly JSON API under `/{owner}/{repo}/api/*`; sha-addressed answers are immutable and cached everywhere; long answers stream progress as SSE. `repos.js` is a dependency-free SDK for pages, agents and scripts. |
| **collab** | A decentralized collaboration layer on `refs/collab/*`: signed issue/comment/review/status/patch entries, per-principal Ed25519 keys self-registered via the thin API, and a deterministic aggregation (threads, PR merge rules, verification health, and a work-unit board projected from declarative column rules in `.walgit/board.toml` — moving a card is just a signed `status` entry) shared by the `walgit collab` CLI, the JSON API and the web UI's Collab tab — one S3 token per participant, no server-side collaboration state. `docs/D1_PROTOCOL.md`. **CI** rides the same refs: `.walgit/ci.toml` in the tested commit declares tasks; `walgit ci run` clients subscribe to ref tips, claim runs with signed `ci_claim` entries (deterministic earliest-claimant convergence, TTL re-claim), execute the command under an env allowlist and publish signed `ci_result` entries — a scheduler-free CI with zero server-side logic. `docs/D1_CI_PROTOCOL.md`. |
| **policy** | Per-repository push rules (`policy.json`): protected refs, groups, fast-forward only, bypass lists. `docs/POLICY.md`. |
| **settings** | Per-repository config (bundle schedules, compaction, upstream follow) published into the WAL with history. |
| **maintenance** | Checkpoints, bundle builds, geometric compaction, base rebuilds, connectivity audits and repairs — one loop that computes the desired state from (config, WAL) every pass and does one bounded unit of the most important missing work. Self-healing by construction: an outage leaves no holes; a deleted artefact is "missing" and rebuilt identically. |
| **auth** | `none` (loopback), `token` (static tokens), `oidc` (any OpenID Connect issuer: browser sign-in, ID tokens, and walgit-issued access tokens for git). `/services/public/install.sh` sets a developer's machine up in one idempotent command. |
| **stores** | S3 and S3-compatible (AWS, MinIO, rustfs, R2, Ceph, …) and GCS, first class; an in-memory store for tests. |

## How it works, briefly

**The repository is a WAL in the bucket.** Under `repos/<owner>/<repo>/`: `manifest.pb` (tiny, CAS-rewritten:
head sequence, the live pack set, checkpoint pointer, settings — *the linearization point*), `log/<seq>.pb`
(immutable entries: PUSH, COMPACT, CHECKPOINT, SETTINGS), `wal/<checksum>.pack|.idx|.rev|.bitmap|.commit-graph`
(immutable, content-addressed packs with their side-files), `checkpoints/<seq>/` (folded ref snapshot + pack
inventory so a cold start is snapshot + tail), `bundles/`, `leases/` (CAS with TTL — the only cross-instance
mutex), `policy.json`, `lfs/objects/`.

**A push**: our receive-pack indexes the pack (`git index-pack --fix-thin --rev-index` in a scratch dir), checks
connectivity and policy, uploads `pack ∥ idx ∥ log entry`, then CASes the manifest. On a 412 it re-reads,
re-validates every ref's old value and retries. Concurrent pushes to one repository on one instance are group
committed into one CAS. The client sees `ok` only after the bucket does.

**A read**: one conditional GET of the manifest; 304 → serve from the local copy, 200 → apply the new entries.
What "apply" means depends on what the request needs: **refs** (snapshot + log → `packed-refs`, no packs:
advertisements, the API, bundle lists), **serve** (the pack set *as this machine can hold it*: small packs and the
history pack local, a too-large base read by range), **full** (everything local, for repacks), **objects** (the
remote reader, for the UI on a repository that does not fit). Pack downloads run on their own runtime and never
block a refs request.

**Placement is configuration.** `[placement] serve / maintain` globs say which repositories a host does object
work for; refs-level reads work everywhere. One box: leave the defaults. Several: put the monorepo on the host with
the SSD (`cache.mode = "disk"`), everything else on the small ones, and route by `/<owner>/<repo>` in front.

**Nothing waits silently.** Anything slow is a *task* with an id, a log and a progress stream — narrated to git on
sideband 2 (`remote: * …`) and to the browser as SSE.

`AGENTS.md` is the full architecture and operating manual: constraints, the WAL strategies, every design decision
with its reasoning, the invariants, and the cost model (round trips to the bucket are the budget).

## Running it

```sh
# build (needs a recent stable Rust toolchain, protoc, node 24 + pnpm for the web UI)
just web-build && cargo build --release -p walgit-cli
# or: nix build .#walgit        or: podman build -t walgit -f Containerfile .

# one box, TLS by walgit itself, a local S3 store (rustfs in a container)
just dev-store
./target/release/walgit-server --config walgit.standalone.toml
open https://walgit.localhost:8080/
```

* `walgit.standalone.toml` — the one-machine shape (self-signed TLS, rustfs, every role). Start here.
* `walgit.example.toml` — every key with its default and a comment.
* `Containerfile`, `flake.nix` — an OCI image and a Nix package/devshell.
* `deploy/nginx.conf.example` — an optional nginx in front: public TLS, one `auth_request` per credential, and
  **byte offload**: walgit answers bundle/LFS downloads with `X-Accel-Redirect` and nginx streams + caches the
  object from the bucket itself (S3 presigned or GCS with walgit's bearer). The file documents the contract.

## Platforms

Production targets Linux (containers, Nix). The code builds and passes the full test
surface natively on Windows (`x86_64-pc-windows-msvc`): the fast tier, e2e and the sim
suite run on CI's windows leg. Symlink-dependent store-mount tests need an NTFS volume
(current Windows 10/11 allows non-admin symlink creation; on older builds enable
Developer Mode or run elevated — exFAT drives silently cannot host links). Pass
`--config NUL` where docs say `/dev/null`. The developer `just dev-store` rig assumes
podman on POSIX; on Windows see `docs/WINDOWS.md` for the rustfs equivalent.

macOS: this fork's local one-box shape runs the full server **on macOS** — the app bundle
contains the Mach-O `walgit` and starts it through `walgit service`; user state stays under
`~/.walgit`. The Swift tray plus the signed/notarized DMG are built from
`deploy/tray/macos/build-dmg.sh`, both locally and by the macOS release job. The macOS CI leg
runs the tray Release/package guards. What is Linux-targeted is **production /
multi-instance deployment** (containers, the Nix OCI image, tmpfs hosts, object-store-backed
fleets), not the binary's ability to run on a Mac.

Roles (`server.roles`): `serve` (git, API, UI, bundles, LFS), `maintain` (checkpoints, bundles, compaction,
fsck/repair). Empty = all. Any number of `serve` hosts may point at one bucket; give
each repository one maintainer (placement globs) and you are done.

### Authentication

| mode | who gets in | how git authenticates |
|---|---|---|
| `none` | everyone is `anon` with write — loopback experiments | nothing |
| `token` | static `tokens` in the config (`token_env` reads the secret from the environment) | `Authorization: Bearer <token>`, or the token as an HTTP Basic password |
| `oidc` | any OpenID Connect issuer (`issuer`, `oauth_client_id/secret`, `allowed_domains`/`allowed_emails`): Google, Entra, Okta, Auth0, Keycloak, Dex, GitLab… | a **walgit access token**: sign in once in the browser, create one at `/_auth/tokens`, paste it into the installer. Stateless (HMAC with `session_secret`, `access_token_ttl`); rotating the secret revokes all. ID tokens from the issuer (`audiences`) and static `tokens` work too. |

Developer setup is one idempotent command — `sh -c "$(curl -fsSL 'https://git.example.com/services/public/install.sh')"` —
which stores the token in a file only the user can read, installs a tiny git credential helper (git ≥ 2.46: it
answers `get` with `authtype=Bearer`, and on a real 401 `erase`s the token and says where a new one comes from),
and turns on `transfer.bundleURI`. `?repo=owner/name` clones right after.

### Developing

```sh
just test          # fast hermetic tier (< 1 min): unit + quick integration, in-memory store, real git
just test-cli      # walgit-cli integration suites (ci_e2e/collab_e2e against a real server)
just e2e           # real git against the server (~20 s)
just warnings      # zero rustc warnings across all targets
just ci            # all of the above
cargo test -p walgit-server --test sim     # fault-injection simulation (crashes, partitions, stale reads)
just test-s3       # store contract against local rustfs
```

Code map:

```
crates/
  walgit-proto    protobuf schema (wal.proto), log framing, store keys
  walgit-store    ObjectStore trait (CAS versions, conditional GET, range, compose); backends s3, gcs, memory; leases
  walgit-git      bare repos on disk, receive-pack, pack ingest, refs ↔ packed-refs, advertisements, upload-pack drivers
  walgit-wal      RepoHandle: sync levels, publish (group commit + CAS), checkpoints, log reader, remote reader, tasks
  walgit-bundle   bundle-uri: slots and chains, building, header ∘ pack composition, lists, retention
  walgit-server   axum: smart HTTP, LFS, bundles, auth (none/token/oidc), the maintainer loop, upstream follow,
                  web/ (API, UI, SDK routes, SSE), setup.rs (installer + recipes)
  walgit-config   walgit.toml (+ WALGIT__ env overrides), per-repo settings merge, fail-closed validation
  walgit-cli      `walgit serve|import|compact|bundle|wal|mirror|synth|config|repo|collab|ci`; `walgit-server` = `walgit serve`
web/              React SPA (Vite) + sdk/repos.ts, built into the binary; the wire contract is web/API.md
docs/             USER_GUIDE (面向人类的使用手册), BUNDLE_URI_DESIGN, ROUNDTRIPS (the cost model), POLICY, LFS, INTEGRITY, D1_PROTOCOL, D1_COLLAB_DESIGN, D1_CI_PROTOCOL, CONTRACT, WINDOWS (dev runbook), patches/
```

## Invariants worth memorising

* The manifest CAS is the only commit point; everything before it is invisible, everything after it is
  idempotent and replayable.
* Immutable objects are content-addressed; nothing is overwritten except the manifest, the bundle list and leases.
* Every read revalidates against the bucket first; there is no "eventually".
* Local disk is a cache. Memory is a cache. The bucket is the repository.
* Placement is configured, never inferred; refs-level reads work everywhere, object work only where placed.
* The maintainer's output is a pure function of (config, WAL); missing is just "not built yet".
* Cost must not scale with ref count on any hot path, nor with pack size on a machine too small for the pack.
* Long work is a task: discoverable, attachable, narrated.
* Correct is not sufficient: every protocol change is judged on round trips to the bucket (`docs/ROUNDTRIPS.md`).

## License

MIT — see `LICENSE`.
