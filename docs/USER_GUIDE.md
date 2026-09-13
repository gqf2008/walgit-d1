# walgit 用户手册

> 面向人类用户的完整使用指南。walgit 是一个**"仓库在对象存储、主机只是缓存"**的 Git 托管服务，
> 并自带一套去中心化协作层（issue / PR / 看板 / CI / 事件 / 权限）。
> 本文从零讲清楚：怎么装、怎么托管仓库、怎么协作、怎么运维。

---

## 目录
1. [walgit 是什么](#1-walgit-是什么)
2. [快速开始](#2-快速开始)
3. [核心概念](#3-核心概念)
4. [仓库管理](#4-仓库管理)
5. [日常 Git 用法](#5-日常-git-用法)
6. [协作：issue / PR / 看板](#6-协作issue--pr--看板)
7. [身份与密钥](#7-身份与密钥)
8. [CI](#8-ci)
9. [事件通知](#9-事件通知)
10. [权限与保护](#10-权限与保护)
11. [镜像到 GitHub](#11-镜像到-github)
12. [多 agent 协作最佳实践](#12-多-agent-协作最佳实践)
13. [运维](#13-运维)
14. [排障与 FAQ](#14-排障与-faq)
15. [术语表](#15-术语表)

---

## 1. walgit 是什么

- **Git 智能 HTTP 服务器**：标准 `git clone / fetch / push` 就能用。
- **仓库存对象存储**：真正的数据在 S3/GCS（R2 等）里，主机只是无状态缓存，可以随时抹掉重建。
- **协作在仓库里**：issue/PR/评审/看板不是数据库，而是 `refs/collab/*` 下的**签名条目**；任何客户端都能离线重放、验签、算出同一视图。
- **无中心 CI**：任务声明写在代码里的 `.walgit/ci.toml`，由任意持有凭据的 runner 认领执行、签名回传结果。
- **事件桥**：把 WAL 里的 ref 变更推送到你的 webhook，驱动通知/自动化。

一句话：**Git 的事实源 + 可验证协作 + 去中心化 CI**。

---

## 2. 快速开始

### 2.1 启动服务（本机开发形态）
```bash
# 已有部署目录 ~/walgit
walgit service start     # 幂等启动(存活判断在二进制里)
walgit service status    # 状态 + /healthz
walgit service stop      # 停止
# `walgit-ensure` 仍可用,但它只是转发到 `walgit service`。
```
服务默认监听 `http://127.0.0.1:8081`，Web UI 在 `http://127.0.0.1:8081/`。

### 2.2 克隆一个仓库
```bash
git clone http://127.0.0.1:8081/<owner>/<repo>.git
```

### 2.3 推送/创建仓库
```bash
git init -b main myrepo && cd myrepo
git remote add origin http://127.0.0.1:8081/<owner>/myrepo.git
git add . && git commit -m "init"
git push -u origin main     # 目标仓库不存在时自动创建（auto_create_on_push）
```

### 2.4 看看主机上有哪些仓库
```bash
walgit repo list                 # bucket 直读（需配置）
# 或只读 HTTP（任何机器）：
walgit repo owners
walgit repo owners <owner>
```

---

## 3. 核心概念

| 概念 | 说明 |
|---|---|
| **Host（主机）** | 跑 `walgit serve` 的无状态进程，面向客户端做 smart HTTP / API / UI |
| **Store（对象存储）** | 唯一事实源：WAL 日志、pack 文件、manifest、policy 等都存桶里 |
| **WAL** | append-only 日志：每次 push/repack/checkpoint 是一条有 seq 的条目 |
| **Manifest CAS** | 多个主机并发的"提交点"：谁先 CAS 成功谁生效，其余重试 |
| **Principal** | 协作身份（人或 agent），对应一把 Ed25519 公钥 |
| **Thread** | 一个工作单元 = 一组 `(id)` 相同的签名条目链（issue→comment→review→status…） |
| **Board** | 线程在 `.walgit/board.toml` 列定义下的确定性投影，不是独立状态 |

---

## 4. 仓库管理

```bash
# 创建（bucket 直读，维护主机形态）
walgit repo create <owner>/<repo>

# 导入已有仓库（含历史）
walgit import --from /path/to/repo <owner>/<repo>

# 信息 / refs / tree / blob / commits / diff（HTTP 只读）
walgit repo overview <owner>/<repo>
walgit repo refs <owner>/<repo> branches
walgit repo tree <owner>/<repo> <rev> [path]
walgit repo blob <owner>/<repo> <rev>/<path> --raw
walgit repo diff <owner>/<repo> <from> <to>

# 归档下载
walgit repo archive <owner>/<repo> <rev> --out repo.tar.gz
```

---

## 5. 日常 Git 用法

与任何 Git 远程完全一致：
```bash
git clone http://host/<owner>/<repo>.git
git fetch origin
git checkout -b feature/x
git commit -am "feat: ..."
git push origin feature/x
git push origin --delete feature/x
```
大仓库可启用 bundle-uri 加速：
```bash
git clone -c transfer.bundleURI=true http://host/<owner>/<repo>.git
```
受保护分支（见 §10）会拒绝非白名单的 push。

---

## 6. 协作：issue / PR / 看板

### 6.1 人类视角（Web UI）
打开 `http://host/<owner>/<repo>` → **Collaboration** 标签：
- 创建 issue：标题 + 说明
- 追加 comment / 变更 status / 提交 review（approve / request_changes）
- 线程页显示完整时间线与正文；PR 页显示 base→head diff 与 merge 判定

### 6.2 CLI 视角
```bash
# 首次注册身份（每仓库一次）
walgit collab principal-register --repo <checkout> --principal <you> --key <keyfile> --push origin

# 开 issue（线程 id 自定义）
walgit collab entry --repo <checkout> --kind issue --id pr-1 --actor <you> \
  --body '{"title":"功能 A"}' --key <keyfile> --push origin

# 挂 PR（base/head 分支）
walgit collab entry --repo <checkout> --kind patch --id pr-1 --actor <you> \
  --base refs/heads/main --head refs/heads/feature/a \
  --body '{"title":"实现 A"}' --key <keyfile> --push origin

# 评审 / 状态 / 关闭
walgit collab entry --repo <checkout> --kind review --id pr-1 --actor <you> \
  --body '{"decision":"approve"}' --key <keyfile> --push origin
walgit collab entry --repo <checkout> --kind status --id pr-1 --actor <you> \
  --body '{"status":"done"}' --key <keyfile> --push origin
```

### 6.3 查看
```bash
walgit collab thread <id> --repo <checkout>     # 线程时间线（JSON）
walgit collab pr <id> --repo <checkout>          # PR 聚合 + merge 判定
walgit collab report --repo <checkout> --format markdown   # 全局观测
walgit collab board --repo <checkout> --format text        # 看板
```

### 6.4 收件箱折叠（GC，D45）
协作条目只增不删，长期会撑大 ref 广播与服务端聚合预算。任一持有身份的主体可定期折叠：
```bash
walgit collab gc --repo <checkout> --actor <you> --key <keyfile> --push origin
```
它把当前 `refs/collab/inbox/*` 全部条目原样收进签名快照 `refs/collab/meta/snapshot` 并删除已折叠的 ref。
读侧一律按「快照 ∪ 未折叠尾巴」聚合，折叠前后任何视图（thread/pr/report/board，CLI 与服务端）**字节一致**；
快照携带每条被删条目的原始签名字节与可重算的 oid，验签链不因删除而断。幂等，可随时重跑；中途崩溃只留重复不留丢失。

> 注意：`status=done` 有 transition 门禁——线程必须先到 `needs-review` 且存在 **verified approve** review。

---

## 7. 身份与密钥

- **repo 级身份**：`walgit collab principal-register` 写 `refs/collab/meta/principals/<p>`。
- **host 级身份（跨仓库）**：一次注册、各处可验：
  ```bash
  walgit principal register --url http://host:8081 --principal <you> --key <keyfile> --token <token>
  walgit principal list --url http://host:8081
  walgit principal rotate --url ... --principal <you> --key <newkey> --token ...
  walgit principal revoke --url ... --principal <you> --token ...
  # 在仓库 B 离线可验：把 host registry 拉进本地缓存
  walgit collab principal-fetch --repo <checkout>
  ```
- 密钥是 **32 字节 hex** 的 Ed25519 种子；妥善保管，谁持有谁就是该身份。

---

## 8. CI

在**被测提交**里放 `.walgit/ci.toml`：
```toml
version = 1
[[task]]
name = "fmt"
command = "cargo fmt --all -- --check"
timeout = "10m"

[[task]]
name = "nightly"
command = "cargo test --release"
schedule = "0 0 2 * * *"   # 可选：6/7 字段 UTC cron（秒 分 时 日 月 周 [年]）或 @daily

[[task]]
name = "dist"
command = "cargo build --release && cp target/release/walgit out.bin"
artifacts = ["out.bin"]    # 可选：任务结束后收集的产物（相对路径，≤ 32 项，单件 ≤ 16 MiB）
```
运行 runner（任意机器）：
```bash
walgit ci validate --repo <checkout>
walgit ci run --repo <checkout> --remote origin --actor ci-runner --key <keyfile> --once
walgit ci run --repo <checkout> --remote origin --actor ci-runner --key <keyfile> \
  --listen 127.0.0.1:8099          # 常驻模式：events webhook 可立即唤醒一次 pass
walgit ci status --repo <checkout>
walgit ci log --repo <checkout> [<run-id>]              # 打印该 run 所存日志（超限时告警）
walgit ci artifacts --repo <checkout> [<run-id>] --out out/  # 逐个 sha256 校验下载产物
```
- runner 认领任务、执行、把结果签成 `ci_result` 条目回传；
- 同一 run 的多次尝试收敛到唯一生效结果；秘密只进 runner 环境，不进仓库。
- `schedule` = 对不动的 ref 周期性评估：runner 每个 pass 顺带做 cron 扫描，错过的
  槽位合并为最新一个（不补跑）；`--once` 配外部调度器（crontab / systemd timer）
  即可当定时 CI 用。定时运行与 ref 触发运行是两个并行线程，各自收敛。
- `--listen` = events 桥 webhook 唤醒；若服务端配了 `events.webhook_secret`，runner
  同时设置同名环境变量 `WALGIT_CI_WEBHOOK_SECRET`（或传 `--webhook-secret`）。秘钥只
  留在 runner 进程，不进入仓库；唤醒只是提示，真正触发仍以 `ls-remote` 的 tip diff 为准。
- 日志与产物存放在仓库自身的 git 对象里（`refs/collab/ci-artifacts/<actor>/<sha256>`，
  按内容寻址）：普通 clone/fetch 不会带上它们，`ci log`/`ci artifacts` 先通过 HTTP
  大小预检（认证沿用 Git 对该 remote 的 credential helper），再按需拉取并验哈希；无 size 通道的 Git/SSH
  远程在下载前拒绝。日志超过 16 MiB 时只保留末尾并置 `log_truncated`。浏览器/SDK 走
  `GET /{o}/{r}/api/collab/ci-artifacts/<sha256>`
  （`repo.ci.artifact(sha256)`）。

---

## 9. 事件通知

配置服务端（walgit.toml）：
```toml
[events]
webhook_url = "http://127.0.0.1:8099/walgit"
webhook_secret = "***"
sweep_interval = "2s"
```
参考接收器（零依赖，校验签名 + 批级去重 + 落盘 JSONL）：
```bash
WALGIT_EVENTS_SECRET=*** python3 deploy/events/agent-receiver.py --port 8099
```
- 每笔 ref 变更会推送 `ref` 事件（repo / ref_name / old / new / seq）；
- 语义：at-least-once，去重键 `(repo, seq, ref_name)`；webhook 只是加速，正确性永远以 WAL 回放为准；
- 收事件后按需 `fetch` 再读条目正文。

---

## 10. 权限与保护

仓库级 `policy.json`（bucket 直读维护）：
```bash
walgit repo policy get <owner>/<repo>
walgit repo policy set <owner>/<repo> --file policy.json
walgit repo policy clear <owner>/<repo>
```
示例：锁 main + 锁 alice 收件箱
```json
{
  "version": 1,
  "groups": [],
  "rules": [
    { "name": "lock-main", "match": { "refs": ["refs/heads/main"] },
      "effect": { "protect": { "restricts": ["create","update","delete"], "bypass": ["alice"] } } },
    { "name": "lock-alice-inbox", "match": { "refs": ["refs/collab/inbox/alice/*"] },
      "effect": { "protect": { "restricts": ["create","update","delete"], "bypass": ["alice"] } } }
  ]
}
```
- **auth=none** 时所有人是 `anon`，只能做 ref 级门禁；
- **per-actor 隔离需要 auth=token**，服务端按 token→principal 判定 bypass（已实测）。

---

## 11. 镜像到 GitHub

walgit 是事实源，GitHub 只是镜像（可选）：
```bash
walgit mirror --from http://host/<owner>/<repo>.git \
              --to https://github.com/<owner>/<repo>.git \
              --dir /ssd/<repo>-mirror.git \
              --ref main --identity git --once
```
- `--identity token`：目标走 walgit Bearer token；`--identity git`：目标用本机 git 凭据（GitHub）；
- 常驻同步去掉 `--once`，配 `--interval 60s`。

---

## 12. 多 agent 协作最佳实践

- **一个线程 = 一个工作单元**；复杂任务拆子线程并用 oid 互引（`--related` / `--depends-on`）。
- **先读后写**：每次发言前 `collab thread` 取最后 oid 作 `--parent`，绝不凭记忆追加。
- **状态用 kind 不用口语**：`status` / `review` / `merge_result`。
- **自闭环**：issue → 认领(status in-progress) → 交付(status needs-review) → 评审(approve) → done。
- **触发**：低延迟用 events 桥唤醒，权威判定用线程读 + 验签；也可以 `collab watch --exec` 做本地自动化。
- **隔离**：每个 agent 一个 principal，只写自己收件箱；review 由独立 agent 完成，结论写全。

---

## 13. 运维

```bash
# 服务
walgit serve --config walgit.toml
# 压缩/打包（维护主机）
walgit compact --all
walgit bundle compose --repo <owner>/<repo>
# WAL 检查与回放
walgit wal ls <owner>/<repo>
walgit wal show <owner>/<repo> <seq>
```
- 主机可水平扩展：多个 `serve` 共享同一 bucket；写由 manifest CAS 收敛。
- 数据在 bucket，主机盘只是缓存；重装主机不丢仓库。
- 记得备份：bucket 即事实源，按需做 bucket 生命周期/版本策略。

---

## 14. 排障与 FAQ

| 现象 | 处理 |
|---|---|
| push 被拒 `rejected by rule 'x'` | 命中 policy，检查 bypass / 用允许的分支 |
| 协作条目 `unverified` | 该 principal 未在**本仓库**注册；注册后读时重算转绿 |
| clone 很慢 | 用 `-c transfer.bundleURI=true`；或 `--filter=blob:none` |
| 事件收不到 | 查 `[events]` 配置、webhook 可达、接收器日志、`server.log` 的 bridge sweep |
| 多个主机同时写 | 允许；CAS 收敛，失败方重试 |
| `config file not found` | 显式 `--config /dev/null` 用于纯 HTTP 读命令；其余需正确配置 |

---

## 15. 术语表

- **WAL**：write-ahead log（append-only 事实日志）
- **manifest**：仓库 refs/pack 集的一次 CAS 快照
- **principal**：协作身份名，绑一把 Ed25519 公钥
- **entry**：`refs/collab/inbox/<principal>/<uuid>` 指向的签名 JSON 对象
- **thread**：相同 `id` 的条目集合，按 `(parent, ts)` 排序
- **board**：线程集合按 `.walgit/board.toml` 列定义的投影
- **bridge**：读 WAL 推送事件的内部循环
- **runner**：`walgit ci run` 的客户端算力

---

> 更多协议级细节见仓库 docs：`D1_COLLAB_DESIGN.md`、`D1_CI_PROTOCOL.md`、`EVENTS.md`、`POLICY.md`、`BUNDLE_URI_DESIGN.md`。
