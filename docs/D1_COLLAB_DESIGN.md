# D1 — 去中心化协作层设计（Decentralized collaboration on walgit）

> 状态：**设计已实现**。当前规范（对象 schema、canonical 签名、验证、聚合、看板、D45 折叠、
> 限额与验收锚点）见 **`docs/D1_PROTOCOL.md`**——实现事实与规范冲突时，以规范与代码为准。
> 本文保留为设计背景与推进记录（§9–§11 的进度注记是历史原貌，不再逐条维护），不重复规范内容。
>
> 本文描述一个**构建在 walgit 之上**的外部协作层——它不是 walgit 进程内的功能。
> 与 walgit 的关系遵循 `AGENTS.md §3` 原则 X（keep walgit small）与 `GOAL.md §4`：
> code review / merge queues / CI / issues 不在 walgit 范围内，**build on this**。

## 1. 目标

1. **仓库在 S3，唯一事实源是桶**。git 数据由 walgit 托管（WAL + manifest CAS，实例无状态、可随时抹掉），
   本地盘不承载仓库；协作层同样不留权威状态在实例上。
2. **协作层去中心化（D1）**：没有"协作服务器"拥有 issue/PR/评审数据。协作状态是**签名、追加式的
   git 对象**，存放在仓库的 `refs/collab/*`，由 walgit 顺手托管；任何人都能克隆、验签、回放、离线计算视图。
3. **一个 token 无缝协作**：人类和 agent 各持一个凭据（token ↔ principal ↔ 签名公钥），即可完成
   clone/fetch/push、协作读写、dashboard 观察，无需在多个系统分别开户。
4. **agent 与人对等**：agent 是一等参与者，同一协议、同一身份模型、同一权威规则；默认"agent 提议、
   人拍板"，策略可配。
5. **dashboard 只读可观测**：视图是任何客户端都能确定性算出的纯函数；dashboard 只是其中一种渲染端，
   不持有状态、无写权限。

## 2. 非目标

- 不做 **D2**（跨实例/跨组织联邦）与 **D3**（纯 P2P、无共享桶）。本设计的模型是
  **共享事实源（桶）+ 无中心协作服务器**。
- 不重造 git 内部（对象格式、pack、传输协议）；只消费 walgit 已有的能力。
- 不把协作逻辑塞进 walgit 进程（保持 walgit 小、专注；协作层是外部协议 + 客户端 + 可选薄服务）。

## 3. 架构总览

```
                 S3 / GCS bucket（唯一事实源：WAL + packs + collab refs）
                                   ▲
        读写都经 walgit（serve / maintain 角色）
        ┌──────────────────────────┴───────────────────────────┐
        │                    walgit（smart HTTP）               │
        │   receive-pack（push 唯一写口） / upload-pack / API    │
        └───────┬───────────────────┬─────────────────┬────────┘
                │                   │                 │
        ┌───────▼──────┐   ┌───────▼──────┐   ┌───────▼───────┐
        │ 人类客户端     │   │ agent 运行时   │   │ dashboard     │
        │ web UI/CLI/IDE│   │ 评审/写码 agent │   │ 只读可观测     │
        └───────────────┘   └──────────────┘   └───────────────┘
        全部是对等参与者：读同一份 refs、本地验签、确定性聚合
```

关键数据流：

- **写协作**：客户端构造签名条目 → `git push` walgit 的 `refs/collab/inbox/<principal>/<uuid>`
  → walgit 校验 policy + manifest CAS 入 WAL → 全局可见。
- **读协作**：refs 级同步（O(1)，无 pack）→ 本地验签 → 确定性聚合出线程/PR 视图。
- **git 事件 → 协作**：push 进 WAL 成为 ref 事实（去重键 `(repo, seq, ref)`，可回放）
  → agent / CI / dashboard 用 refs 级轮询（`ls-remote` / `collab watch`）消费。
- **合并**：聚合视图判定规则满足（如 ≥1 个人类 approve）→ 合并方 `git push` 结果到 walgit
  → `policy.json` 兜底。

## 4. 协作数据模型

### 4.1 refs 命名空间（每仓库）

| ref | 内容 | 写权限 |
|---|---|---|
| `refs/collab/inbox/<principal>/<uuid>` | 某参与者的一条追加式条目链（签名字节） | 仅该 principal（policy 锁） |
| `refs/collab/meta/principals/<principal>` | 每 principal 一个注册 ref（内容 = 公钥 JSON）；吊销 = 删该 ref（tombstone） | 仅本人（首次注册）/ 本人 |
| `refs/collab/meta/rules` | 合并规则对象（协作层语义，见 §6） | 仅 admin |
| `refs/collab/meta/protocol` | 协议版本号 | 仅 admin |

设计要点：

- **收件箱模型**：每人只写自己的 ref → 无跨参与者写冲突；同一收件箱内并发由 walgit 的
  manifest CAS（412 重试）保证。这与 walgit"多写者正确性由 CAS 保证"是同一哲学。
- **收件箱一致性（聚合器不变量）**：条目 JSON 的 `actor` 必须与其收件箱 ref 路径上的
  principal 一致，聚合器必须校验（`EntryRef::is_verified`：不一致 = 未验证）。policy 是
  写入口的闸，聚合器是读侧的闸——两层缺一不可：无 policy 的仓库任何人可写任意收件箱，
  签名本身只证明"actor 签了它"，不证明"它属于这个收件箱"。
- **视图不存储**：issue 线程、PR 状态、评审统计都是**确定性纯函数**，输入 =
  (协议版本, 全部收件箱 refs 在某 manifest seq 的快照)。人人算出一致结果 → 不需要中心索引，
  没有中心服务可依赖/可单点故障。
- **跨 ref 一致性**：一次读取以单个 manifest CAS 为同步点（walgit 的 refs 在同一 manifest 下本就一致），
  避免读到中途的中间态。

### 4.2 条目（entry）schema

条目是内容寻址的 git 对象（blob），链式组织：

```json
{
  "version": 1,
  "kind": "issue | comment | patch | review | status | merge_result | agent_action",
  "id": "<uuid>",
  "actor": "alice@example.com",
  "ts": 1786500000,
  "parent": "<上一条目 oid 或根>",
  "refs": { "base": "refs/heads/main", "head": "refs/heads/topic" },
  "body": { "...": "..." },
  "sig": "ed25519:<base64>"
}
```

- **签名 canonical 形式（契约，跨语言验签者据此复现）**：对不含 `sig` 的条目做
  **递归键排序** JSON：对象键按字节序升序；数组保持顺序；字符串/数字/布尔按
  JSON 标准转义；值为 `undefined` 的键**丢弃**（与存储时 `JSON.stringify` 丢键一致）；
  无空白、无尾随逗号。`sig = "ed25519:" + base64(Ed25519 对 canonical 字节的签名)`。
  SDK 的 `collab.entry` 即按此实现；任何语言的验签端以本定义为准。
- **验签**：本地用 `refs/collab/meta/principals/<principal>` 里该 actor 的公钥验 `sig`；
  链式 `parent` 保证追加顺序不可篡改（防重排、防丢）。
- **条目类型语义**：
  - `issue`：开 issue。
  - `comment`：线程评论（可带 file/line 锚点 → 评审行内评论）。
  - `patch`：提 PR（含 base/head refs、title/body）。
  - `review`：评审结论 `approve | request_changes | comment`（可带锚点）。
  - `status`：状态流转（in-progress / needs-review / blocked / needs-human …）；
    可带 `owner` / `worktree` / `branch` / `work`（缺失的 `work` 回退 `note`），
    BoardCard 投影当前处理者与工作上下文供人类监督。
  - `merge_result`：合并结果（result oid、规则求值记录）——由合并方写入，审计可回放。
  - `agent_action`：agent 行为留痕（可选；模型、置信度、耗时等元数据，供 dashboard 与审计）。

### 4.3 确定性聚合（视图即纯函数）

- `thread(id)` = 按 (parent, ts) 排序的、引用该 id 的条目序列。
- `pr(id)` = {base, head, 状态机（open/merged/closed）, reviews[], approvals[], 规则求值}。
- `merge_rule_eval(rule, log)` → `{allowed, reason, satisfied_by[]}`。

聚合不落库、不以缓存为权威（可做只读缓存加速热路径，权威永远是 refs）。

## 5. 身份与凭据：一个 token 走天下

目标：人类和 agent 只需要一个凭据即可无缝协作（git + 协作 + dashboard）。

- **token = 认证**：walgit 的静态 token / `wgt_` / OIDC，认证 git smart HTTP、API、协作写。
- **principal = 身份**：token 解析为 principal（人 `alice@…` 或 agent `svc-reviewer-1`）。
  **principal 语法**：`[A-Za-z0-9][A-Za-z0-9._@-]*`（refname-safe，不含 `:`/`/`/`..`；SDK
  在 `collab.*` 里校验，非法即抛 `ReposError(400)`）。
- **公钥 = 验签**：Ed25519 密钥对与 principal 绑定，注册进 `refs/collab/meta/principals`。
  **落地（issue #10）**：首次使用自注册——`collab.principal({principal, publicKey})`
  构造注册条目，经 receive-pack push 到 `refs/collab/meta/principals/<principal>`；
  吊销 = 删该 ref（tombstone，`collab.revokePrincipal`）。"签发时自动注册"
  （扩展 `wgt_` 签发流程为"principal 注册"一步）留待服务端薄 API（§11）。
- **host-global registry（issue #76）**：同一主机的跨仓库身份——`GET|PUT|DELETE
  /api/v1/principals[/{principal}]`（web/API.md）。仓库本地的
  `refs/collab/meta/principals/*` 始终优先；本地没有的 principal 回落 host
  registry，一次注册对主机上所有仓库生效。读侧经 TTL 缓存（60 s，写即失效，
  issue #104）——collab report/thread/board 每请求只付一次命中成本。
  CLI 侧 `walgit collab principal-fetch` 把 host registry 拉成本地副本：
  当 approve reviewer 的 key 仅在 host registry 时，CLI 的 transition 门禁
  本地比对更严，须先 fetch 再判（否则合法流转被误拒）。
- **读可以直连桶（可选强化）**：bundle 走 presigned URL、静态对象走 S3 读——有凭据即可；
  **写永远走 walgit receive-pack**（manifest CAS 是唯一提交点，原则 II），
  所以"一个 S3 token"不意味着绕过 walgit 写。
- **agent 与人类完全同构**：agent 也是一份 token + 密钥对 + principal（`svc:` 前缀）；
  无特权路径，只有 policy 和合并规则赋予的权限。

## 6. 权威与合并规则

- **git 层**：`policy.json` 保护 `refs/collab/*`（收件箱只允许本人写；meta 只允许 admin），
  以及受保护分支（沿用现有 `match.refs` + `protect` + `bypass` 机制）。
- **协作层**：合并规则 = 确定性函数（`merge_rule_eval`），例如受保护分支要求
  `≥1 个人类 approve 签名`（agent 的 approve 默认不计入人类门禁，除非显式配置）。
  规则对象存于 `refs/collab/meta/rules`（协作层语义，不塞进 `policy.json`——
  与 `docs/POLICY.md` 的边界一致：required reviews / must go through a PR 属于合并规则，不属于 push 文件）。
- **人在回路默认**：受保护分支，agent 只能 propose + review；合并由人（或显式配置的
  merge-queue 服务）执行。非保护分支/个人仓库按配置放开。
- **审计**：一切动作（含合并）都是签名条目或 WAL 条目，可回放到任意点。

## 7. Agent 协议

1. **触发**：对 `refs/collab/*` 做 refs 级轮询（便宜，O(1)，无 pack；`(repo, seq, ref)` 去重、
   at-least-once、可回放）；`walgit collab watch` 把它变成事件循环。
2. **行动**：构造签名条目（review / patch / comment / status）→ push 自己的收件箱。
   幂等：条目内容寻址 + `(id, kind, actor, parent)` 去重；事件按 seq 去重。
3. **上下文**：walgit API（tree/blob/commits/resolve）+ blobless bundle（全量上下文、字节走桶）。
4. **求助**：`status: needs-human` 条目 + `review: request_changes` 把球踢回人；dashboard/订阅者展示。
5. **留痕**：`agent_action` 条目记录模型/置信度/耗时（可选），供 dashboard 与审计。

## 8. Dashboard / 可观测性

- **只读、无状态、无写权限**：它只是"确定性聚合 + walgit API/指标"的一个渲染端。
- **视图**：issue/PR 线程、评审状态、approve 覆盖、agent 活动流、合并统计、
  事件滞后（`head_seq − cursor`）。
- **工作单元看板**：列规则声明式定义在 `.walgit/board.toml`（随仓库版本化），
  投影 = `build_board(threads, principals, rules, def)` 纯函数（`walgit-wal::collab`
  一处实现，CLI / 端点 / SPA 三端同源、字节一致）；卡片在列间的移动不新增写语义
  ——就是一条既有签名的 `status` 条目。缺省（无定义文件）时内置 open/merged/
  closed/other 兜底板；定义非法 fail-closed（见 §9 进度与 web/API.md）。
- **指标来源**：walgit `/metrics/prometheus`、`/healthz`、`/readyz`、`/api/*`（SSE 实时流），
  加上聚合视图。
- **实时**：SSE 信封 / `collab watch` 在 ref 变化时立即通告，dashboard 与数据同步无轮询竞态。

## 9. walgit 缺口清单（需要补的能力）

| # | 缺口 | 说明 | 契合点 |
|---|---|---|---|
| 1 | **通用 refs 读取 API** | API 现只有 `refs/{branches|tags}`；补 `refs/collab/*` 的列出/读取（git 协议层已通告任意 ref） | 纯读、可缓存 |
| 2 | **评审原语端点** | `diff`、`merge-base`、`patch`、`blame`、`archive`（PR review 的 UI 和 agent 都要） | 纯函数、immutable、可缓存，符合 API 缓存规则 |
| 3 | **token↔公钥注册** | 签发 `wgt_` 时可选注册 Ed25519 公钥到 principals 注册表（或由协作层负责） | "一个 token 走天下"的前提 |
| 4 | ~~**events 桥对任意 ref**~~（2026-09-16 已删除） | events 桥随其 webhook、角色与配置一起移除；collab refs 的变更改由 refs 级轮询 / `collab watch` 观察（`tests/events.rs` 已删） | — |
| 5 | **SDK 扩展** | `repos.js` 增加 collab lane（读聚合、写条目） | dogfood 规则：不另起 SDK |

> 进度：缺口 1 由 issue #8（历史 GitHub issue；Issues 已随主仓迁移禁用） 批次落地——
> 已实现 `GET /{o}/{r}/api/refs/all`（全量 ref，全名、字节序分页、SSE）、
> `GET /{o}/{r}/api/refs/collab`（`refs/collab/*` 命名空间）与
> `GET /{o}/{r}/api/refs/name/{rest}`（精确单 ref 读取，SWR+ETag）。
> 缺口 2 进展：`GET /{o}/{r}/api/merge-base?from=&to=` 已落地（本地 `git merge-base`；
> remote 走有界双向 walk，含 SSE 叙述与预算保护）；
> `GET /{o}/{r}/api/diff?from=&to=&format=patch|stat|name-status` 已落地
> （remote 先 level-parallel fault 两树差异对象再跑同一 `git diff`，与本地字节一致）；
> `GET /{o}/{r}/api/blame/{rev}/{path}` 已落地（porcelain 解析为 JSON；remote 有界
> fault 祖先链后跑同一 `git blame`）；`patch`（格式 patch 语义）已由
> `diff?format=patch` 覆盖；
> `GET /{o}/{r}/api/archive/{rev}?format=tar.gz|zip` 已落地（二进制下载；
> remote 预算化整树 fault，超限 503 指向 bundle-uri）。
> 缺口 1、2 至此落地（第 8-10 项契约/测试/进度随批次收口）。**blame 已知限制**
> （评审期明确，见 web/API.md）：remote 上**不跟随 rename**（git blame 无开关可关，
> 且需读未 fault 的旧路径树 → 定义 404，本地正常），深祖先文件超预算 503——
> 两者均需本地 packs/bundle，后续批次再补 rename 跟随（issue #13）。
> merge-base 的"无关历史 → null"在深仓库可能先撞预算表现为 503（文案已注明）。
>
> **update (issue #13 已落地)**：remote blame 现在**跟随精确 rename**——游走单元是
> `(commit, path)` 对，边界提交 fault 两侧整树骨架（`BLAME_TREE_BUDGET` 上限，
> 超限 503），沿 blob oid 相等的内容前身（git blame 精确 rename 检测的首选候选）
> 以旧名继续；本地/remote 输出一致（`remote_blame_follows_exact_renames` 锁死）。
> 改内容型 rename（相似度检测需读候选 blob）仍不跟随，需本地 packs/bundle。
>
> 缺口 3-5 由 issue #10（历史 GitHub issue；Issues 已随主仓迁移禁用） 批次落地：
> 缺口 4（events 桥任意 ref）曾由 `tests/events.rs` 的 golden 测试覆盖，该桥与测试已于
> 2026-09-16 一并删除；collab refs 的变更现由 refs 级轮询 / `collab watch` 观察；
> 缺口 5（SDK）`repos.js` 已加 collab lane：`refsAll`/`refsCollab`/`refByName`、
> `mergeBase`/`diff`/`blame`/`archive`，以及 `collab.entry`/`collab.principal`/
> `collab.revokePrincipal`（构造+签名条目并产出 git push 指令，经 receive-pack
> 投递——SDK 跑不了 git，由 CLI/agent 执行）；
> 缺口 3（token↔公钥）落地为**首次使用自注册**：用 `collab.principal()` 把公钥
> push 到 `refs/collab/meta/principals/<principal>`，吊销 = 删该 ref（tombstone）。
> "签发时自动注册"（auth.rs 挂接）留待薄 API（见 §11）。
>
> 缺口 3、5 的**浏览器写路径 + Web UI** 由 issue #26（历史 GitHub issue；Issues 已随主仓迁移禁用）
> 批次落地（薄 API + 交互式 dashboard）：
> - **薄 API 写路径**：`POST /{o}/{r}/api/collab/entries`（浏览器直写收件箱，
>   服务端把条目对象打进 pack 走 WAL publish，强制 `actor == principal`）与
>   `POST /{o}/{r}/api/collab/principal`（首次使用自注册公钥到
>   `refs/collab/meta/principals/<principal>`）——缺口 3 的"签发时自动注册"
>   由此具备浏览器等价物（token ↔ principal ↔ 公钥在浏览器里一步绑定）。
> - **聚合读端点**：`GET /{o}/{r}/api/collab/report`（全量观测报告，D1 §8）
>   与 `GET /{o}/{r}/api/collab/threads/{id}`（单线程有序条目 + PR 视图 +
>   合并规则评估）；与 CLI 共用 `walgit-wal::collab` 纯聚合核心，服务端与本地
>   验证同一套（含收件箱归属校验）。
> - **SDK**：`collab.post`/`collab.registerPrincipal`/`collab.report`/
>   `collab.thread`/`collab.buildEntry`（canonical 签名后直接 POST）。
> - **Web UI（Collab 页）**：线程列表 + 总量健康 + PR 合并评估 + 单线程时间线
>   （条目流、验签徽标、PR/评审面板），浏览器 Ed25519 密钥（WebCrypto，私钥不出浏览器）
>   一键注册并发布 issue/评论/评审/状态/patch 条目。
>
> **工作单元看板**由 issue #30（历史 GitHub issue；Issues 已随主仓迁移禁用）
> 批次落地（§8 的看板视图 + §11 问题 3 的决定）：
> - **投影核心**：`BoardDef`（`.walgit/board.toml`，`version = 1`，`[[column]]`
>   谓词 kind / status / merge(allowed\|blocked) / unverified，声明序
>   first-match-wins，无列匹配的卡不上板）+ `build_board` 纯函数进
>   `walgit-wal::collab`；定义解析 fail-closed（坏版本 / 无列 / 空或重名列 /
>   未知 merge 判词 / 未知字段 = 错误，绝不静默兜底）；排序默认 last_ts 降序、
>   卡 id 作全序兜底——同一 refs 集合必然投影出逐字节一致的看板。
> - **端点**：`GET /{o}/{r}/api/collab/board`（定义读 HEAD 的
>   `.walgit/board.toml`，remote 仓经 remote reader fault；缺失 = 内置默认板
>   open/merged/closed/other，非法 = 400；SWR，永不 immutable）。
> - **CLI**：`walgit collab board --format text|markdown|json`（`--board`
>   预览未提交的定义；`json` 输出与端点字节一致，e2e 断言）。
> - **SPA**：`/{o}/{r}/collab/board` 只读页（列 + 卡 + SSE 实时刷新）；
>   移动卡片 = 薄 API 的一条签名 `status` 条目（parent = 卡 `last_oid`）。
> - **e2e**：双客户端字节一致断言（CLI 离线聚合 vs 服务端点，移动前后各一次）；
>   移动 = 第二个克隆经 receive-pack push 签名 status 条目，第三个克隆看到移动
>   且全部条目对注册表验签通过。

## 10. 一致性、并发与安全

- **写并发**：收件箱按 principal 分片 → 无跨参与者竞争；同收件箱内 CAS 重试。
- **读一致性**：以单次 manifest CAS 为同步点读取全部 collab refs。
- **防滥用**：条目大小上限、频率配额（policy/代理层）、公钥吊销（删
  `refs/collab/meta/principals/<principal>` tombstone，SDK `collab.revokePrincipal`）。
  薄 API（浏览器路径）与 receive-pack 同经 `policy.json` 评估——冻结/保护
  `refs/collab/*` 对两条写路径同样生效。浏览器签名的 Ed25519 私钥存于
  localStorage（可导出 JWK，持久化所迫）：同源 XSS 可整枚盗用签名身份，
  服务端只能靠人工 tombstone 吊销——原型期接受，升级路径是不可导出密钥 +
  服务端登记确认。
- **隐私**：collab refs 与仓库同权限域——匿名/私有仓库的协作数据随之同权限（walgit 认证已覆盖）；
  独立的可见性控制（如 issue 对组织外可见）留作后续，本设计不引入。

## 11. 开放问题 / 下一步

1. 协作写走"裸 `git push` 收件箱"还是"薄 API 包装 push"？**两者都落地了**：
   裸 push（issue #10，CLI/agent 路径，SDK `collab.entry`/`collab.principal` 产出
   receive-pack 指令）**和** 薄 API（issue #26，浏览器路径，`POST …/collab/entries`
   与 `POST …/collab/principal`：服务端把条目对象打进 pack 走 WAL publish；
   同一 WAL 发布路径，无第二套写语义）。
2. Web UI 先行还是 CLI 先行？（建议 CLI + 最小 Web 视图先跑通协议）——**已定（issue #14）**：
   CLI 先行，`walgit collab ls|thread|pr|entry|principal-register|principal-revoke`
   已实现（读走本地 git 的 collab refs + 验签聚合；写走构造/签名 + 本地写 ref +
   `--push` 经 receive-pack），并有真实 walgit 实例的 e2e（`crates/walgit-cli/tests/
   collab_e2e.rs`：注册 → 建 issue → 链式评论 → approve → 新克隆聚合验签）。

   ③ agent 运行时（issue #16）：`walgit collab watch` 已实现——常驻/`--once`
   拉取 `refs/collab/*`，状态文件（`<gitdir>/collab-watch.json`）对比增量，每条
   新条目通过 `--exec` 回调（stdin 传条目 JSON，env 给 kind/thread/actor/verified）；
   agent 的“大脑”是外部命令，walgit 只做 notify+sync。
    ④ dashboard（issue #19）：`walgit collab report --format text|markdown|html` 已实现——只读观测渲染端（无状态、无写权限），全局聚合线程/PR/验签健康/agent 活动，html 为自包含单文件。
   **交互式 Web UI（issue #26）**：`/{o}/{r}/collab` 页（线程列表 + 总量健康 +
   PR 合并评估 + by-actor/by-kind）与 `/{o}/{r}/collab/thread/{id}` 页（条目时间线、
   验签徽标、PR/评审面板、浏览器写框）已实现——薄 API 直写 + 浏览器 Ed25519 密钥。
   写路径治理与 receive-pack 对齐：`policy.json` 同一评估链（load→分类 force→evaluate，
   拒绝 = 403，理由记日志）、`wal.fsck_objects` 同源、principal 更新为真实 old 值 CAS；
   聚合读单请求预算 20k refs，超限 503 指向 CLI 离线聚合（条目对象一次
   `cat-file --batch` 读完，无逐条目子进程）。
    ⑤ CI 外挂协议（issue #31）：`docs/D1_CI_PROTOCOL.md`（规范）——触发 = ref 事实
   （refs 级轮询）、`ci_claim`/`ci_result` 签名条目、
   确定性竞争收敛 + TTL 重认领、产物引用 + 哈希、秘密只在客户端 env；落地为
   `walgit-wal/src/ci.rs`（聚合核心）与 `walgit ci validate|run|status`。服务端零 CI
   逻辑（原则 X）。
3. 聚合视图的只读缓存放哪（是否复用 walgit 的 render cache `cache/api/v1/*.json`）？
   **不复用（issue #30 决定）**。render cache 的契约是"答案按内容寻址、不可变"
   （键 = 答案自身的哈希：可无限重放、谁先算好谁受益）；而协作聚合的输入是活的
   collab refs，每次 push 都可能改变答案——不存在可缓存的不可变答案，硬套 render
   cache 等于把 TTL 伪装成内容寻址，违反原则 IV（每次读都重验证，没有"最终一致"）。
   因此聚合端点（report / threads / board）一律 SWR + ETag、永不 immutable；
   读路径的成本控制来自共享的 `collab_load`（单次 refs 级同步 + 条目一次
   `cat-file --batch` 读完，20k refs 预算），不来自跨请求缓存答案。
4. 条目 GC/压缩：**已落地（issue #160，walgit D45）**——聚合状态快照（读侧 = 快照 ∪
   增量尾），设计与语义见 §11.4。
5. 原型顺序建议：
   ① walgit 侧：通用 refs API + 评审原语端点；
   ② 协作条目协议 + CLI 验证（先于 UI）；
   ③ agent 接入（refs 级轮询 / `collab watch` + 签名条目）；
   ④ dashboard（只读渲染端）。

## 11.4 协作条目折叠：聚合状态快照（snapshot ∪ tail，issue #160 / walgit D45）

> **已实现；规范见 `docs/D1_PROTOCOL.md` §9。** 本节只保留设计动机与结论摘要；对象 schema、
> 读侧语义、gc 写侧算法（CAS lease、分批删除、纯剪枝）、限额与已知边界全部以规范为准。
> 本节此前的完整设计文本在 git 历史中可查，不再在此重复维护（避免同一事实两个家）。

要解决的问题：`refs/collab/inbox/*` 纯追加会让聚合撞上单请求 ref 预算，也让 clone/fetch 的
ref 通告体积随历史线性增长。解法与 WAL checkpoint 同形：把已折叠条目收进一个 CAS 移动的
快照 ref（`refs/collab/meta/snapshot`），读侧 = 快照 ∪ 未折叠尾部，按 oid 去重——折叠前后
聚合逐字节一致（这是验收等式）。条目信任始终来自每条自己的签名，不来自快照签名；折叠单元
是 CLI（`walgit collab gc`），写经 receive-pack，快照先落、收件箱后删，中途崩溃只产生重复、
不产生丢失。
