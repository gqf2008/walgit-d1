# D1 — 去中心化协作协议（规范）

> 状态：**规范（normative）**。本文是 D1 协作层的单一规范：身份、refs 布局、条目 schema 与
> canonical 签名、验证、聚合（thread / PR / merge / report / board）、折叠（D45）、CI 接入面与
> 观察通道，全部写成可实现的规则；与代码冲突时以代码为准
> （`docs/CONTRACT.md` 纪律：code wins where they differ）。
>
> 文档关系（一个事实只有一个家，避免重复维护）：
>
> | 文档 | 角色 |
> |---|---|
> | 本文 `docs/D1_PROTOCOL.md` | **规范**：所有 D1 对象与算法的规则语言 |
> | `docs/D1_COLLAB_DESIGN.md` | 设计背景与历史进展记录（提案起家；冲突以本文 + 代码为准） |
> | `docs/D1_CI_PROTOCOL.md` | CI 子协议（`ci_claim` / `ci_result` / 日志与产物），normative |
> | `docs/BOARD.md` | 看板定义文件的**作者指南**与示例（语义规范在本文 §8） |
> | `docs/POLICY.md` | `policy.json` 推送策略语言（写入口的闸） |
> | `web/API.md` | HTTP 线协议（D1 lane 一节；本文 §12 只给规则面） |
> | `skills/walgit/SKILL.md` | 运维/agent 操作手册（随二进制分发，D47） |
>
> 可执行形式（读协议时对照实现）：
>
> | 实现面 | 位置 |
> |---|---|
> | 聚合核心（entry/canonical/verify/thread/pr/merge/report/board/snapshot） | `crates/walgit-wal/src/collab.rs` |
> | CLI（读、写、watch、gc） | `crates/walgit-cli/src/collab_cmd.rs` |
> | host 注册表客户端 | `crates/walgit-cli/src/principal_cmd.rs` |
> | 薄 API 写路径 + 服务端聚合 | `crates/walgit-server/src/web/api.rs`（`collab_*`、`collab_load`） |
> | host 注册表服务端 | `crates/walgit-server/src/web/v1.rs`（`/api/v1/principals`） |
> | SDK（canonical / 签名 / post） | `web/sdk/repos.ts` |
>
> 一致性验收锚点（§16 列全）：`crates/walgit-wal/src/collab.rs` 的内嵌测试、
> `crates/walgit-cli/tests/collab_e2e.rs`、`crates/walgit-server/tests/web_api.rs` 的
> `collab_*` 用例。**改动本协议涉及的行为必须同时改这些测试**（规范与黄金用例同批）。

## 1. 定位与不变式

1. **桶是唯一事实源，协作没有中心服务器。** 协作状态是 `refs/collab/*` 里的签名 git 对象；
   walgit 只负责托管（receive-pack + WAL）与在服务端顺手聚合。任何克隆都能离线验签、回放、
   重算出同一个答案。
2. **收件箱模型。** 每个 principal 只写自己的收件箱 ref；写权限由 `policy.json` 分片。
   读侧另有独立闸：`entry.actor` 必须等于收件箱属主（§7.1）。
3. **签名覆盖 canonical 形，与存储格式无关。** 条目可以 pretty JSON（CLI）或紧凑 JSON
   （薄 API）落盘；验签只依赖 canonical 字节（§5.3）。
4. **聚合是纯函数。** 输入 = 条目**集合**（同一 oid 去重）+ 注册表 + 规则；输出与 refs 的读取
   顺序、分页边界、是否正处于折叠中途无关——同一输入必须字节一致。
5. **追加式、不可变。** 条目一经发布不改不删（注册表 tombstone 是唯一删除语义）；纠正 =
   追加一条新的条目。折叠只移动引用与删除 ref，被折叠条目的字节由快照逐字携带（§9）。
6. **写永远经 receive-pack（或共享同一 WAL 发布路径的薄 API）。** manifest CAS 是唯一提交点；
   没有第二套写语义。
7. **服务端不验签，验签在聚合。** 写路径只做身份（actor == 认证 principal）与 `policy.json`
   门禁；「这条签名属于谁、算不算数」永远由读侧按 §4.5 判定。

## 2. 术语

| 术语 | 定义 |
|---|---|
| principal | 协作身份（人/agent/CI runner），refname-safe 段；`actor` 字段与注册表键用它 |
| entry | 一条签名 JSON 条目（§5），作为 git blob 存在 `refs/collab/inbox/<principal>/<seg>` |
| thread | 同一 `id` 的全部条目在 `parent` 链上的确定序（§6） |
| inbox | `refs/collab/inbox/*` 命名空间；一条目一 ref |
| verified | `is_verified` 为真：签名对该 actor 注册 key 通过 **且** 收件箱属主 == actor（§4.5） |
| canonical 形 | 签名所覆盖的、递归键排序、无空白的 JSON 字节串（§5.3） |
| fold / gc | `refs/collab/meta/snapshot` 收纳已折叠条目、随后删除收件箱 ref 的压缩过程（§9） |
| board | 线程集合在 `.walgit/board.toml` 声明式列定义下的确定性投影（§8） |
| merge rules | 合并门禁规则文档（`refs/collab/meta/rules`），确定性求值（§7.3） |
| host 注册表 | 同机跨仓库的 principal→公钥映射（`/api/v1/principals`，§4.4） |

## 3. refs 布局

每仓库（引用名规则见 §3.1）：

| ref | 内容 | 写入者 | 折叠 |
|---|---|---|---|
| `refs/collab/inbox/<principal>/<entry-seg>` | 一条 entry 的 blob | 仅该 principal（policy 分片） | 折叠进 snapshot 后删除 |
| `refs/collab/meta/principals/<principal>` | 注册文档（JSON，无自签名，§4.3） | 仅本人（首次）/ 本人（轮换） | 不折叠（每 principal 单例） |
| `refs/collab/meta/rules` | `MergeRules` JSON（§7.3） | admin | 不折叠 |
| `refs/collab/meta/snapshot` | 折叠快照 blob（§9.1） | 任何持有已注册 key 的折叠者（CAS） | 不折叠 |
| `refs/collab/ci-artifacts/<actor>/<sha256>` | CI 日志/产物 blob（D1-CI §8.2） | CI runner | 不折叠 |

不存在的 ref 不作为协议的一部分：设计文档提过的 `refs/collab/meta/protocol` **未实现**，读方
不应把它当版本协商点（版本在对象内，§15）。`refs/walgit/principals/*` 是**客户端本地缓存**
（`collab principal-fetch` 写入，不进仓库协议、不推送），只在验签时作为 host 注册表的离线副本。

### 3.1 段名规则（refname-safe segment，normative）

principal 与 entry-seg 必须满足（CLI `ref_segment` 与薄 API `ref_segment_ok` 同一规则）：

1. 非空，UTF-8 字节长 ≤ 255；
2. 首字符为 ASCII 字母或数字；
3. 其余字符 ∈ `[A-Za-z0-9._@-]`；
4. 不含 `..`（git 禁止 `..` 作为路径分量）；不等于 `.` / `..`；
5. 不以 `.lock` 结尾（大小写不敏感）。

等价的 ASCII 正则：`^[A-Za-z0-9][A-Za-z0-9._@-]*$` 再叠加第 4/5 条。非法段在写路径被拒
（CLI 报错退出；薄 API `400`），因为 WAL 发布路径不复查 refname，非法名会在副本物化
packed-refs 时才爆炸。SDK 的客户端校验（`refSegment`）只做正则与 4/5 条、未限 255 字节，
超长段会在服务端被拒。

`<entry-seg>` 只需 refname-safe 且**在该收件箱内唯一**；协议不规定生成方式。现状：
CLI 用 16 随机字节 hex，SDK 用 `crypto.randomUUID()`，薄 API 用 UUIDv4。

### 3.2 读取面

- CLI 读命令只读**本地** git 的 `refs/collab/*`，不发 API 请求。默认 `git clone` 只取
  `refs/heads/*`，因此新克隆要先显式取协作 refs（或跑一次 `walgit collab watch --once`）：
  `git fetch origin '+refs/collab/inbox/*:refs/collab/inbox/*' '+refs/collab/meta/*:refs/collab/meta/*'`。
- 服务端从 WAL 索引（`Need::Objects`）读取同一命名空间；条目 blob 够小则本地读，否则经 remote
  reader 一次批量 fault + 一次 `git cat-file --batch`（绝不每条目一进程）。见 §12 预算。

## 4. 身份、密钥与信任

### 4.1 密钥格式

- **私钥文件**：32 字节 Ed25519 种子的 hex 文本（64 个 hex 字符，可带尾随换行）；CLI 的
  `--key` 参数是**文件路径**，不是内容（传内容会在报错里把私钥回显到终端）。
- **公钥**：32 字节原始 Ed25519 公钥的 **base64（标准字母表）**，示例
  `6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=`。
- 算法：Ed25519（RFC 8032），验签用严格模式（拒绝弱公钥/可延展签名）。

### 4.2 principal 语法

同 §3.1 段名规则（refname-safe）。惯例前缀：agent `svc-`、CI runner `ci-`；**`svc-` 前缀在
合并门禁里有语义**（§7.3，agent 的 approve 默认不算人类批准）。

### 4.3 仓库级注册文档

`refs/collab/meta/principals/<principal>` 的 blob：

```json
{ "version": 1, "principal": "alice", "public_key": "<b64>", "registered_at": 1786500000 }
```

- **文档本身没有签名**。绑定信任来自写入口：`policy.json` 必须只允许该 principal 写自己的
  registry ref（见 §14 威胁模型）。没有 policy 的仓库（allow-all）= 任何人可注册任何人。
- `version` 当前恒为 1；`registered_at` 为 unix 秒（信息性）。
- **轮换** = 用新 key 重新注册（覆盖该 ref）；**吊销** = 删除该 ref（tombstone）。
- 写路径：CLI `walgit collab principal-register|principal-revoke [--push <remote>]`；薄 API
  `POST …/api/collab/principal`（CAS 旧值更新，§12）。receive-pack 路径按 Git 对 non-commit
  ref 的更新规则（覆盖既存 ref 需要显式 force 时由调用方处理）。

### 4.4 host 注册表（跨仓库身份）

同一主机上一个 principal 只注册一次，主机上所有仓库可验：

- `GET /api/v1/principals` → `{ "<principal>": "<public_key_b64>", … }`（需读权限）。
- `PUT /api/v1/principals/{principal}`、`DELETE /api/v1/principals/{principal}`：需写权限，
  **self-only**（认证 principal == 路径 principal；auth `none` 的 loopback 例外）。PUT 为覆盖写
  （注册/轮换同形），DELETE 即吊销。响应缓存 60s，写后立即失效（issue #104）。
- 存储键 `host/principals/<principal>`；内容与 §4.3 同形。
- CLI：`walgit principal register|rotate|list|revoke --url <host> [--token]`（HTTP-only）。
- `walgit collab principal-fetch` 把 host 注册表拉成本地 `refs/walgit/principals/*` 缓存，
  并**清除 host 已不再列出的缓存 principal**（吊销在副本上立即结束）。
- **优先级：仓库本地 `refs/collab/meta/principals/*` 永远覆盖 host 注册表**（CLI 与 server 同
  规则：先本地 `or_insert`，host 只填空缺）。repo 本地缺 key 时才回落 host。

### 4.5 验证定义（normative）

对一条 `EntryRef {oid, principal, entry}` 与注册表 `principals`：

```text
is_verified(er) =
      er.principal == er.entry.actor          // 收件箱归属（本协议 §4.5 的读侧闸）
   && principals 含 er.entry.actor
   && verify_entry(er.entry, principals[actor])   // canonical 字节上的 Ed25519 严格验签
```

- 收件箱归属与签名缺一不可：policy 是写入口的闸，聚合器是读侧的闸；无 policy 的仓库里
  任何人都能往任何收件箱 push，签名只证明「actor 签过它」，不证明「它属于这个收件箱」。
- **验证是「此刻注册表」的函数**，不是历史时点的快照：删除某 principal 的注册 ref 后，其全部
  历史条目在未来聚合中变为 unverified（计数、approve、merge 判定随之变化）。这是吊销的
  预定语义：不可信 key 的签名不可信。
- 读侧对「验签失败 / 收件箱不符 / body 超大」的条目一律**计入展示、不参与收敛**：
  report 的 unverified 计数、看板 unverified 谓词、PR 的 unverified 列表都能看到；
  绝不被静默吞掉。

## 5. 条目（entry）schema 与签名

### 5.1 字段

```json
{
  "version": 1,
  "kind": "issue | comment | patch | review | status | merge_result | ci_claim | ci_result | <custom>",
  "id": "<线程 id>",
  "actor": "<principal>",
  "ts": 1786500000,
  "parent": "<上一条目 oid，或根条目为 \"\">",
  "refs": { "base": "refs/heads/main", "head": "refs/heads/topic" },
  "body": { "…": "…" },
  "sig": "ed25519:<base64>"
}
```

| 字段 | 类型 | 规则 |
|---|---|---|
| `version` | u32 | 写出恒为 `1`；聚合解析目前不校验该字段（结构性解析） |
| `kind` | string | 见 §5.2；未知 kind 合法：只计数/展示，无投影副作用 |
| `id` | string | 线程键，**不要求 refname-safe**（不进 ref 名）；同线程全部条目共用 |
| `actor` | principal（§4.2） | 必须等于所在收件箱属主才可能 verified |
| `ts` | i64 | unix 秒；写入端取当前时间。**不被认证**：只影响排序与展示（及 CI 子协议的 TTL 活性判断），不参与签名/身份等安全判定 |
| `parent` | string | 上一条目 blob oid；根为 `""`。工具**不校验**指向是否存在（§6.3） |
| `refs` | object? | 可省略；`{base?, head?}`，patch 用 |
| `body` | JSON | 协议约定为对象；字段抽取值仅对对象生效 |
| `sig` | string | `ed25519:<base64>`，覆盖 canonical 形（§5.3/§5.4）；缺失/空 = 验签必然失败 |

**存储**：一条目一个 git blob（ref `refs/collab/inbox/<actor>/<entry-seg>`）。CLI 写 pretty
JSON，薄 API 写紧凑 JSON——**格式不是协议**，签名不覆盖存储字节，验签只看 canonical 形。
但 blob oid 是存储字节的内容寻址，因此去重键是 oid/字节而非语义相等：**重试必须复用同一份
字节**（同一次构造的 blob），重新序列化后的「同一条目」是另一个 oid，会被聚合视为两条（§7.1）。

### 5.2 kind 与 body 约定

基础协议对 body **不做强制校验**（读取端做宽容抽取）；下表是各 kind 的规范约定，
唯一被强制的两项是 §7.5（`status=done` 写侧门禁）与 merge 计数（§7.3）。

| kind | 语义 | body 约定字段 | 投影副作用 |
|---|---|---|---|
| `issue` | 工作单元（线程根） | `title`；正文 `body`/`text` | 成为 report thread / 看板卡片 |
| `comment` | 评论、留痕 | `note`/`text` | 无状态副作用 |
| `patch` | 挂实现分支 → 线程成 PR | `title`、`message`；分支在 **entry.refs** | report.prs / merge 判定 |
| `review` | 评审结论 | `decision`（`approve` 计入；其余值原样展示，缺省当 `comment`）、`agent`、`note` | approvals |
| `status` | 工作单元状态/上下文 | `status`、`owner`、`worktree`、`branch`、`work`、`note` | card_status / 看板上下文 / `done` 门禁 |
| `merge_result` | 合并落账 | `merged: true`；`oid`、`result`、`note` | card_status 与 PR status → `merged` |
| `ci_claim` / `ci_result` | CI 认领/结果 | **见 `docs/D1_CI_PROTOCOL.md`**（严格 schema） | CI 聚合（report.runs） |
| `<custom>` | 自定义/未来 | 自由 | 仅出现在 kinds / by_kind |

结构扩展（写侧注入，读侧规范见 §5.5）：`related: [oid]`、`depends_on: [oid]`、
`attachments: [{filename, sha256, content_b64}]`。

### 5.3 canonical 形（签名字节，normative）

签名覆盖的字节 = 对条目 JSON 做如下变换后的字符串：

1. 取条目对象：把 **`sig` 清为空字符串 `""`**（键保留）；`refs` 为 null/缺失时**省略该键**
   （Rust `Option::None` 的 `skip_serializing_if` 与 SDK 的 `if (input.refs)` 一致）；
2. 递归序列化：
   - 对象：键按升序排序，`{"k":v,…}`，无空白；
   - 数组：保持顺序，`[…]`，无空白；
   - 字符串：标准 JSON 转义；数字：规范化十进制（整数即十进制字面量）；布尔/`null` 直出；
3. 不允许空白、尾随逗号、注释。

数字与键序的跨语言约束：**协议数值字段全部是整数**（`ts`/`version` 等），跨语言验签的条目
不得在 `body` 使用浮点（各语言的浮点序列化不同）。键序按实现语言字节序（Rust `String` Ord
= UTF-8 字节序；JS `Object.keys().toSorted()` = UTF-16 码元序，需 ES2023 运行时）——**协议字段名全为 ASCII，两者一致**；
跨语言条目的 `body` 键也应为 ASCII（非 ASCII 键在两种序下可能不同）。JS 实现还必须在
canonicalize 前丢弃值为 `undefined` 的键（SDK 已如此），其它语言无此概念。

一个具体的规范样例（键序 = 字母序）：

```text
{"actor":"alice","body":{"title":"x"},"id":"t","kind":"issue","parent":"","sig":"","ts":1,"version":1}
```

#### 5.3.1 已知跨语言偏差（待修，勿再复制）

**现状**：Rust `entry_canonical`（`walgit-wal/src/collab.rs`）把 `sig` 清空后**保留键**，
canonical 形含 `"sig":""`；而 SDK `web/sdk/repos.ts` 的 `signedEntry` 在加入 `sig` **之前**
对对象调 `canonicalize(entry)`，字节串**不含** `sig` 键。两者签名输入不同：

- 用 SDK/浏览器 WebCrypto 签的条目，Rust 聚合（`verify_entry`）验不过 → 恒为 unverified；
- 用 CLI/agent（Rust `sign_entry`）签的条目是历史现网行为，全部覆盖「含 `"sig":""`」的字节。

**证据**（可复跑）：同版本 serde_json 1.0.151 的探针输出
`{"actor":"alice",…,"parent":"","sig":"","ts":1,"version":1}`；Node 交叉验签中
「SDK 签名字节 vs Rust canonical」= FAIL、「Rust 签名字节 vs Rust canonical」= PASS。

**修复方向（需项目决策并同批测试）**：为不使既有全部签名失效（协作 refs 是桶内追加式数据），
应改 **SDK 侧**：签名 `canonicalize({...entry, sig: ""})`，使浏览器/CLI 覆盖同一字节串；
反向修改 Rust（真正「不含 sig」）会作废全部历史签名，只能作为显式迁移处理，不能顺手改。
在该偏差修复前，本文 §5.3 的规则（含 `"sig":""`）以**验证端行为**为准。

### 5.4 签名与验签

```text
sig = "ed25519:" + base64( Ed25519_sign( canonical(entry with sig="") ) )   // 64 字节签名
verify = Ed25519_verify_strict( pubkey(actor), canonical(entry with sig="") , sig )
```

验签步骤（normative，与实现一致）：`sig` 必须以 `ed25519:` 开头 → base64 解码必须得 64 字节
签名 → 注册表公钥 base64 解码必须恰为 32 字节 → Ed25519 严格验签 canonical 字节。任一步失败
即 `unverified`（原因不对外分级，读侧只报真/假；CLI 报错文本可含原因）。

### 5.5 引用与附件约定

- `related` / `depends_on`：oid 字符串数组（写 CLI：`--related` / `--depends-on`，可多次）。
  读侧不做存在性写门禁；`GET …/api/collab/threads/{id}` 对每条条目返回
  `broken_refs`（引用但不在当前 collab 全集里的 oid），机器可读（仅服务端线程端点计算；
  CLI `thread` 不返回该字段）。
- `attachments`：数组项 `{filename, sha256, content_b64}`；**单文件 ≤ 64 KiB**（写侧
  `--attach` 强制）；`sha256` 为内容十六进制摘要，读取方应复算校验；`filename` 只取基名。

## 6. 线程与顺序

### 6.1 id 与 parent

- 线程 = 所有 `id` 相同的条目。`id` 由创建者选定（惯例：人类可读短名 `cc-ai-<topic>`；
  CI 用例 `ci-<hex16>`，见 CI 协议）。
- `parent` 约定 = **上一条目发布输出的 oid 原样复制**；根条目 `""`。链式追加是审计可回放性
  的来源（防重排/防丢的证据面），但见 §6.3。

### 6.2 thread() 排序算法（normative）

输入：同一 `id` 的条目集合（任意顺序）。输出：确定的条目序。

1. 建 `oid → entry` 索引。
2. 待处理列表按 `(ts, actor, oid)` 升序排序（字节序比较）。
3. 循环（**守卫条件按每轮开始时的当前待处理长度求值**）：
   ```text
   guard = 0
   while pending 非空 and guard < pending.len() * 2 + 1:
       guard += 1
       顺序扫描 pending；一条条目「就绪」= parent == ""
         或 parent 不在集合里（悬空父视作根）
         或 parent 已发射（本轮前序或更早的轮次）
       就绪者（含本轮前序条目刚解锁的级联）按扫描顺序发射；未就绪者进下一轮
       pending = 未就绪者
   ```
4. 循环退出后，**剩余待处理条目**（无论是否还有就绪者）按第 2 步的相对顺序直接追加在尾部。

性质与边界（实现事实）：

- **确定序**：算法是集合的纯函数——同一集合、任意输入顺序，输出一致（轮内 `(ts, actor, oid)`）。
- **链序**：当 `ts` 沿链**非递减**（常规写入：后写的 `ts` 不早于前写）时，单链在第一轮按
  `ts` 顺序级联发射，输出即完整链序。
- **守卫会提前截断（实现缺陷，非设计特性）**：第 3 步的界用的是**当前** pending 长度（随发射
  收缩），触发点很低——沿链严格递减的 `ts` 下，7 条链即截断（n=7 输出 `o0…o4, o6, o5`；n=10 输出 `o0…o6, o9, o8, o7`）。一旦截断不会恢复：后续追加条目不再
  链序，`last_oid` 也不再是链头（后续以它为 `parent` 的条目全部挂错）。同一集合仍字节确定
  （确定序成立），但**拓扑序不成立**。修复方向（后续 issue）：界相对**初始**集合大小求值
  （或去掉全局上限），并加沿链递减 `ts`、n≥7 的回归测试。读方在修复前不应假定输出是拓扑序；
  需要严格链序时以 `parent` 自行重排。
- **排序不由签名保真**：`ts` 与 `parent` 都是条目作者可控的字段（签名只证「作者这么写了」，
  不证「时间真实 / parent 属实」），顺序是协作约定、不是可依赖的安全边界；`done` 门禁与看板
  都读这个顺序（§7.5/§8.2），威胁模型见 §14。
- **链头**（tip）= 输出末条，`last_oid`；后续条目以它为 `parent`。

### 6.3 链完整性（运维须知）

- 写入工具**不校验** `parent` 是否存在、是否属于同一线程；手敲错 oid 会产生**孤儿条目**
  （`collab thread` 把它列为额外根），工具不会拦。
- 批量建线程时先建 issue、再把其 oid 接住作为下一条的 `--parent`（不要先批量建 issue 再
  统一切状态，否则线程会出现两个根）。收尾前用 `collab thread <id>` 复核 `oid/parent` 链。
- 条目不可变：写错的 parent 不能改，只能追加一条语义相同、parent 正确的条目并注明
  「上一条 parent 写错，以本条为准」。

## 7. 聚合（normative）

聚合核心 = `walgit-wal::collab`，CLI 与 server 共用；同一输入集合，双方输出字节一致。

### 7.1 输入集合：`EntrySet`（oid 去重）

聚合输入是**集合语义**：每个 oid 只算一次（乱序、重放、折叠中途的快照∪尾部重复都收敛到同一
集合）。同一 oid 出现在多处（如被恶意植入他人收件箱）时，保留「属主 == actor」的那份；
若两份都不是属主自身或都是，保留先读到的。这样一次植入无法顶掉合法副本。

读取面构造集合的来源：`refs/collab/meta/snapshot` 的有效记录 ∪ 全部 inbox ref（§9.2）；
解析失败 / oid 对不上字节的副本跳过（退化可见，绝不静默取信）。

### 7.2 PR 视图 `pr_view`（线程含 `patch` 时）

按 §6.2 排序后逐条重放：

- `base`/`head`：遇到带 `refs` 的 `patch` 条目时，`refs.base`/`refs.head` 非空则覆盖（后者为准）；
- `status` 状态：默认 `open`；`status` 条目的 `body.status` ∈ {`merged`,`closed`} 时置为该值
  （其它值不影响 PR 状态）；`merge_result` 的 `body.merged == true` 置 `merged`。更晚的
  `status` 条目只在取值为 merged/closed 时才覆盖——即 PR 状态机是 **open → merged/closed**，
  不会从 merged 退回 open；
- `reviews`：全部 `review` 条目（无论是否 verified），`decision` 缺省为 `comment`；
- `human_approvals`：其中 **verified 且 decision == `approve`** 的评审（注意：这里包含
  `svc-` actor；是否算「人类批准」由 merge 规则判定，§7.3）；
- `unverified`：所有非 verified 条目，格式 `<actor>@<oid>`。

### 7.3 合并规则 `MergeRules` 与求值

规则文档（`refs/collab/meta/rules` 或 CLI `--rules <file>`）schema：

```json
{ "protect": ["refs/heads/main"], "require_human_approvals": 1 }
```

- `protect`：受保护 base 的前缀列表；`require_human_approvals` 默认 `1`；未知键容忍（忽略）。
- 求值 `merge_rule_eval(rules, pr)`：
  1. `protected = pr.base 存在且 ∃p ∈ protect: base == p || base.starts_with(p)`。
     **注意前缀语义**：`refs/heads/main` 也会匹配 `refs/heads/mainline`（想要精确保护就写全名，
     且不要用会成为别的 ref 前缀的字符串）。
  2. 非保护 base → `allowed=true`，理由 `base is not protected`。
  3. 批准数 = `human_approvals` 中 `actor` **不以 `svc-` 开头**的条目数。
  4. `allowed = 批准数 ≥ require_human_approvals`；理由与 `satisfied_by`（批准者列表）随附。
- **按条目计数，不去重批准者**：同一 principal 的多条 verified approve 会重复计入
  （`satisfied_by` 也逐条列出）；patch 作者可以给自己 approve。这是当前实现的语义（不是笔误）；
  是否要求 distinct approvers、是否禁止自审是待定的设计决策（后续 issue 跟踪）。

**规则来源按客户端不同**（现状，非缺陷）：server 的 report/threads/board 自动读
`refs/collab/meta/rules`（解析失败 = 500，fail closed）；CLI 的 `pr`/`report`/`board` 只认
`--rules <file>`，不传 = 默认（不保护任何 base）。要本地复现服务端判定，CLI 需显式给规则文件。

### 7.4 观测报告 `report`

字段（`walgit-wal::collab::Report`，CLI 与服务端同源）：

- `threads[]`：每个非纯 CI 线程的 `{id, title, entries, verified, last_ts, kinds}`；
  `title` = 根条目 `body.title`（缺失为 `""`）；`kinds` = 去重升序。
  **排序是投影的一部分**：`last_ts` 降序，`id` 升序破平；客户端照序渲染、不重排。
- `prs[]`：含 `patch` 的线程的 `{id, title, base, head, status, approvals, merge_allowed, merge_reason}`；
  排序：`open` → `merged` → 其余（closed 等），组内 `last_ts` 降序、`id` 升序。
- `runs[]`：CI 运行段（纯 CI 线程不进 threads/board，由此段投影；schema 见 CI 协议 §8.3）。
- 计数：`total_entries` / `verified_entries` / `unverified_entries` / `missing_principals`
  （unverified 且 actor 无注册 key）/ `by_actor` / `by_kind`（键升序）。

unverified 永远可见：读侧对验签失败/收件箱不符的条目只降级为计数（红），绝不静默吞掉；
空结果也如实呈现为空，不得被渲染成「全部通过」。报告 HTML 渲染对文本做转义（`&<>`）。

### 7.5 `status = done` 写侧门禁（normative）

`done` 是工作单元的终态（与 `closed` 区分；`closed` 不限前置）。经 **CLI 写路径**或
**薄 API 写路径**追加 `kind=status && body.status == "done"` 的条目时，必须满足：

1. 线程按 §6.2 **先排序后**重放 `card_status`（§8.2），当前状态 == `needs-review`；
2. 线程中存在 **verified** 的 `review` 条目且 `decision == "approve"`。

否则写入被拒（CLI 非零退出并给机器可读理由；薄 API `400`）。CLI 在缺 verified approve 时
提示先 `walgit collab principal-fetch`（approve 者的 key 可能只在 host 注册表）。

**边界（必须知道）**：门禁在**写路径**（CLI + 薄 API），不在聚合。裸
`git push`/其它客户端追加的 `done` 不会被读侧拒绝——读侧只按签名与投影规则处理；门禁是协作
纪律的执行器，不是共识规则。且第 1/2 步都读由 `ts`/`parent` 决定的线程顺序（§6.2），approve
也按条目计数——门禁挡流程失误，不挡恶意（参与者可回拨 `ts` 或自签 approve）。真正的安全边界
是写权限（`policy.json`）与签名身份，见 §14。

## 8. 看板投影（`build_board`，normative）

看板不是状态：它是 `(entries, principals, rules, BoardDef)` 的纯函数。列定义随仓库版本化在
`.walgit/board.toml`（HEAD；用 `--board <file>` 可预览未提交稿）；**移动卡片 = 追加一条签名
`status` 条目**，投影重新派生列。

### 8.1 定义 schema（`version = 1`）

```toml
version = 1

[sort]
by = "ts"          # ts（默认，last_ts）| id
direction = "desc" # desc（默认，新在前）| asc

[[column]]
name = "needs-review"   # 必填、非空、文件内唯一
kind = ""               # 可选：线程必须含该 kind
status = ""             # 可选：卡片有效状态（card_status）必须等于它
merge = ""              # 可选："allowed" | "blocked"（仅含 patch 的线程；无 verdict 不匹配）
unverified = false      # 可选：true = 只收至少含 1 条 unverified 条目的卡片
```

**fail-closed 校验**（违反即错误；服务端 `400`，CLI 报错退出——绝不允许静默折错列）：
`version != 1`；无列；列名为空或重复；`merge` 取非 `""`/`allowed`/`blocked`；未知字段
（`deny_unknown_fields`，拼错的键直接红）。

缺省（仓库无定义文件）= 内置默认板：`open`（status）→ `merged` → `closed` → `other`（catch-all）。

### 8.2 卡片与状态

卡片字段（序列化字段序即线协议序，CLI `--format json` 与服务端端点字节一致）：

| 字段 | 来源/规则 |
|---|---|
| `id` | 线程 id |
| `title` | 根条目 `body.title`，缺失 `""` |
| `prose` | 根条目 `text`→`body`→`note`→`message`→`summary` 第一个非空并 trim，缺失 `""` |
| `actor` | 根条目 `actor` |
| `status` | `card_status`：按线程序重放，`status` 条目的字符串 `body.status` 覆盖；`merge_result {"merged":true}` 置 `merged`；更晚的 `status` 可再覆盖；默认 `open`。**刻意宽于 PR 状态机**：跟踪工作单元（in-progress/needs-review/blocked/needs-human…） |
| `owner`/`worktree`/`branch`/`work` | 工作上下文：按线程序遍历 `status` 条目，字段为**字符串**时覆盖（含显式 `""` 清空）；字段缺失 = 继承上一份；`work` 缺失时回退该条目的 `note`；这条 `note` 回退也覆盖继承值 |
| `created_ts` | 根条目 `ts` |
| `last_ts` | 线程内最大 `ts` |
| `entries`/`verified`/`unverified` | 集合大小与验证计数 |
| `kinds` | 去重升序 |
| `last_oid` | §6.2 输出末条（后续条目的 `parent` 接它） |
| `merge` | 线程含 patch 时 `{allowed, reason}`，否则 `null` |

### 8.3 匹配、排序与排除

- **首列命中**：卡片进入**声明顺序第一个**满足全部已声明谓词的列；不匹配任何列的线程**不在
  看板上**（「说你想看到的，而不是拿到你没要求的」）。
- 谓词语义：`kind` 非空 → 线程含该 kind；`status` 非空 → `card.status == 该值`；
  `merge` 非空 → 含 patch 且 verdict 命中（`allowed` → `merge.allowed==true`；`blocked` → false）；
  `unverified=true` → `card.unverified > 0`。空字段 = 任意。
- 排序：`by=ts` 按 `last_ts`，`by=id` 按 id；方向按 `direction`；**破平恒为 id 升序**，
  因此输出是全序、字节确定。
- **纯 CI 线程（全部条目为 `ci_claim`/`ci_result`）不是看板卡片**，跳过；其展示面是
  report.runs / `walgit ci status` / SPA 线程徽标（CI 协议 §8.3）。

## 9. 折叠：快照 ∪ 尾部（D45）

追加式收件箱撞两堵墙：单请求聚合预算（20k refs）与 clone/fetch 的 ref 通告体积。解法与 WAL
checkpoint 同形：读侧 = 最新快照 + 其后增量尾。

### 9.1 快照文档（`refs/collab/meta/snapshot` → 一个 blob）

```json
{
  "version": 1,
  "kind": "collab_snapshot",
  "actor": "<执行折叠的 principal>",
  "ts": 1786500000,
  "entries": [
    { "oid": "<条目 blob 的 git oid>", "principal": "<收件箱属主>", "json": "<条目原始字节，逐字>" }
  ],
  "sig": "ed25519:<base64，覆盖把 sig 清空（键保留）后的本文档 canonical 形，同 §5.3>"
}
```

- `entries` 按 `oid` 升序、按 oid 去重：折叠是输入集合的纯函数，同一集合必然产出同一快照字节。
- 每条记录 = **digest 清单**：oid + 收件箱属主 + **原始签名字节逐字携带**（绝不重序列化）。
- 快照自身的签名只证明「谁在何时折叠」（审计与 watch 通告）；**条目信任永不来自快照签名**，
  每条折叠条目仍按 §4.5 独立验签。

### 9.2 读侧语义

```text
聚合输入 = parse_snapshot(快照 blob).entries 中「oid 可由 json 重算且能解析为 Entry」的记录
          ∪ refs/collab/inbox/* 中可解析的条目
          → EntrySet（按 oid 去重，§7.1）
```

- 记录 oid 校验：`git blob oid("blob <len>\0" + json)` 按**仓库对象格式**（sha1/sha256）重算，
  与声明不符 → 跳过（与损坏收件箱 blob 同语义：退化可见、绝不取信）；`json` 解析失败同样跳过。
- 快照文档本身**fail-closed**：JSON 非法 / `version != 1` / `kind != "collab_snapshot"` →
  读取整体报错（静默跳过等于改写历史）。服务端在物化 body 前先查 size，超 64 MiB → `503`。
- 折叠前后聚合逐字节一致（快照 ∪ 尾部 == 原收件箱），这是本设计的验收等式：空尾部全折叠、
  部分折叠、折叠中途（快照已更新、删除未完成 → 重复输入）三种状态答案相同。

### 9.3 `collab gc` 写侧算法（normative）

前置：折叠者必须已完成注册（§4.3）且 `--key` 的私钥与注册公钥匹配——否则会把「actor 无法
验证的」快照发出去，让所有读者以为折叠者作恶。折叠单元是 CLI（不是 maintainer 守护进程），
写经 receive-pack。

1. **读基线**：快照 ref 当前值（无 = `None`，记录为「必须不存在」基线）；现有记录逐字装进
   结果集。
2. **扫本地收件箱**：逐 ref 读 blob；无法 UTF-8/无法解析为 Entry/名字不合法的 → 计数
   `unparseable` 留在原地（读侧同样跳过；自动剔除会把可修复的暂时损坏变成永久丢失）。
   可解析者按 oid 合并：若新副本属主 == actor 而既有记录不是，替换（与 §7.1 同规则）。
3. **判断**：有新记录或更好的副本 → 构造并签名新快照 blob（`hash-object -w`）；否则
   **纯剪枝折叠**：不重建快照（重建只差 `ts`，是纯 ref churn）。
4. **先落快照（CAS）**：`git push <remote> <snapshot-oid>:refs/collab/meta/snapshot`
   携带 `--force-with-lease=refs/collab/meta/snapshot:<基线 oid>`；无快照时基线是**空
   `<expect>`**（`refs/collab/meta/snapshot:`）——不是零 oid；**绝不加 `+`**（`+` 会静默短路
   lease，让过期折叠覆盖并发折叠者的新快照）。lease 失败 = 有并发折叠者，重取重折。
   纯剪枝折叠也先推基线快照（no-op 或补齐），**任何情况下先快照后删除**；若检查点没有任何
   快照可推（远端尚无快照且本次无新记录），命令报错并提示先 fetch，绝不用空基线直接删除。
5. **再删收件箱**：一次全量 `git ls-remote` 取远端通告，只删仍在通告里的 ref（并发 gc 可能
   已剪掉一些；stock git 对通告里没有的 ref 删除会报错，把这种拒绝当错误会让「重跑收敛」
   承诺失效）；每批 ≤ 500 条 refspec（规避 ARG_MAX），每条带**刚读到的 OID lease**（防通告
   与删除之间同名 ref 被 force-update 后误删新条目）。非原子、幂等，重跑收敛。
6. **本地镜像**：按服务器结果 reconcile 本地命名空间（快照 ref 更新、已删收件箱 ref 删除）。

崩溃语义：快照先落 ⇒ 中途崩溃/中途读者只看到「重复」，去重后答案不变；重跑 idempotent。
多折叠者并发：lease 保证恰有一方落地，另一方重试。

### 9.4 限额与成本

- 服务端单请求 `COLLAB_MAX_ENTRIES = 20000`：计数 = **未折叠**的 inbox ref + principals ref
  （快照与 rules 是单例不计）；超限 `503`，提示 `walgit collab gc` 或离线 CLI 聚合。
- 快照 blob 上限 `COLLAB_SNAPSHOT_MAX_BYTES = 64 MiB`：remote 与 local/mounted 路径都在
  物化 body 前拒绝，超限 `503`。
- 折叠把 N 条 ref 收成 1 条 ref + 1 个有界 blob；fetch 该命名空间的客户端会拉快照字节
  （≈ 折叠历史体积），不关心协作层的克隆不取该命名空间（这也是正确的：快照是对该命名空间
  的压缩，不是仓库数据的压缩）。
- **写侧无上限（当前实现缺陷）**：64 MiB 只在服务端读路径检查，CLI 的 `gc` 与本地 load 都
  不设限；折叠后的快照一旦超过 64 MiB，服务端从此对所有聚合返回 `503`（读路径 fail-closed）。
  恢复：先把快照记录/条目字节取出（保留窗口内可 `walgit wal materialize --at-seq` 回放，或从仍持有快照的克隆导出），再移动/删除 `refs/collab/meta/snapshot` 并重折——直接删 ref 会把快照 blob 变成不可达对象，有丢历史的风险；修复方向是 `gc` 推送前拒绝并给出提示（后续 issue 跟踪）。

## 10. CI 协作（子协议）

CI 完全构建在本协议之上，**协调层零服务端逻辑**（触发/认领/执行/结论都在持有凭据的客户端
runner）；walgit 只提供事实源（桶 + `refs/collab/*`）与通用对象读（含产物 HTTP 读面）。
runner 是普通 principal，触发 = ref 事实，认领/结果是
同一收件箱里的 `ci_claim` / `ci_result` 条目（`id` = run id）。**schema、状态机、竞争收敛、
TTL、秘密边界、日志/产物存放（`refs/collab/ci-artifacts/<actor>/<sha256>`）全部以
`docs/D1_CI_PROTOCOL.md` 为准**，本文不复制。与 D1 的接口只有三条：

1. 条目仍走 §5 的 schema/canonical/签名与 §7.1 的集合语义；
2. 纯 CI 线程不进 report.threads / 看板，只出现在 report.runs 与 `walgit ci status`；
3. 产物命名空间**不在** `collab watch` 的 fetch 范围内（按需拉取，避免每个 watcher 背 16 MiB blob）。

## 11. 观察通道（pull-only）

D46：服务端不推送事件；事实源是 ref 变化与 WAL。至少一次语义只在 pull 车道成立。

### 11.1 `walgit collab watch`

每轮（默认 `--interval 10` 秒，`--once` 单轮）：

1. `git fetch -q <remote> '+refs/collab/inbox/*:refs/collab/inbox/*' '+refs/collab/meta/*:refs/collab/meta/*'`
   ——**刻意不取 `refs/collab/ci-artifacts/*`**。
2. 以 `for-each-ref refs/collab` 取当前 `refname → oid`，与状态文件
   （默认 `<gitdir>/collab-watch.json`，可 `--state` 覆盖）逐 ref 比对：新出现或 oid 变了的
   ref 为事件，按 refname 排序。**ref 删除不产生事件**（状态文件随轮覆盖）。
3. 每个事件解析 blob 得到事件字段：
   - inbox 条目 → `kind`=entry.kind、`actor`、`thread`=entry.id、`verified`=`is_verified`（§4.5）；
   - `meta/snapshot` → `kind="snapshot"`、`actor`=快照 actor、`thread=""`、`verified`=快照签名
     对 actor 注册 key 的验证（仅 provenance）；
   - `meta/principals/<p>` → `kind="principal"`、`actor`=p、`verified=true`（注册文档本身不签名，
     该字段只表示「已识别的注册表事件」）；
   - 其它 meta ref（如 `meta/rules`）→ 按 entry 路径尝试解析（principal 取不到时为空串）；
     解析不出合法 entry 时回退为 `kind="unknown"`、`verified=false`。
4. `--exec <cmd>` 对每个事件执行一次（`sh -c`）：**原始 blob 文本进 stdin**，环境给
   `WALGIT_COLLAB_REF` / `_KIND` / `_THREAD` / `_ACTOR` / `_VERIFIED`。事件字段取自**解析后的
   条目**而非渲染文本（body 里的 `\nkind=` 不能伪造信号）。exec 非零退出 → 本轮报错中止，
   状态文件不推进：下轮重报同一批事件（at-least-once）。
5. 全部事件处理完才写状态文件。

消费方幂等键：条目 oid（内容寻址）；即使重投递也解析为同一事件集。

### 11.2 其它拉取车道

- **ref tip 轮询（最便宜）**：`git ls-remote` + 与上次快照 diff，`(ref, tip oid)` 去重（CI
  runner 即此模式）。
- **WAL 回放**：`walgit wal ls <owner/repo> --from <seq> [--to <seq>]`（`--from` 含），
  `walgit wal show` 取详情，`walgit wal materialize --at-seq` 折叠后回放；游标推进到
  `seq + 1`（或定义为「下一条待读」）。
- **HTTP SSE**：`text/event-stream` 信封，best-effort（可能丢包）；要无损 push 语义必须自建
  sidecar 从 pull 车道转发（D46）。

### 11.3 去重与幂等（汇总）

| 层 | 键 | 语义 |
|---|---|---|
| 条目集合 | oid（内容寻址） | 同 oid = 一条；重放/折叠重复无害 |
| 线程顺序 | 排序键 `(ts, actor, oid)`（`parent` 只决定就绪，不参与排序）；不保证拓扑序，见 §6.2 | 确定序，任何客户端一致 |
| ref 事件 | `(refname, oid)` | watch 状态文件对比 |
| CI 触发 | `(ref, tip oid)` | runner 状态文件 processed |
| WAL | `(repo, seq)` | 游标回放 |

## 12. HTTP 薄 API 面（规则面）

线协议细节见 `web/API.md`；本节只列规则：

| 端点 | 语义 | 门禁 |
|---|---|---|
| `POST /{o}/{r}/api/collab/entries` | 浏览器写路径：条目 blob 打成单对象 pack，经 **同一条 WAL 发布路径** 写 `refs/collab/inbox/<actor>/<uuid>`；`200 → {ref, oid, seq}` | 认证写；`actor == 认证 principal`（auth=none 时匿名例外）；actor refname-safe；`status=done` 走 §7.5 门禁；`policy.json` 与 receive-pack **同一评估链**；**服务端不验签** |
| `POST /{o}/{r}/api/collab/principal` | 首次注册/轮换公钥到 `refs/collab/meta/principals/<p>`（CAS 旧值） | `principal == 认证 principal`；policy 同 receive-pack |
| `GET /{o}/{r}/api/collab/report` | §7.4 报告（含 CI runs） | 读权限；SWR，永不 immutable |
| `GET /{o}/{r}/api/collab/threads/{id}` | 线程有序条目 + 每条的 `verified`、`broken_refs`，含 patch 时附 `{pr, merge}`；未知 id `404` | 同上 |
| `GET /{o}/{r}/api/collab/board` | §8 看板（读 HEAD 的 `.walgit/board.toml`）；定义非法 `400` | 同上 |
| `GET`/`HEAD /{o}/{r}/api/collab/ci-artifacts/{sha256}` | CI 日志/产物读取（先查 size、后读取、发前验 sha256；`HEAD` 是 CLI 预检） | 同 CI 协议 §8.2 |
| `GET /api/v1/principals`、`PUT`/`DELETE /api/v1/principals/{principal}` | host 注册表（§4.4） | 读需读权限；写 self-only |

预算与缓存：collab 聚合端点受 §9.4 的 20k/64MiB 预算保护，超限 `503` 指向 gc / 离线 CLI；
三个聚合读端点一律 **SWR + ETag、永不 immutable**（输入是活跃 refs，不存在可缓存的不可变答案）。

## 13. 限额与预算（汇总）

| 项 | 限额 | 执行点 |
|---|---|---|
| principal / entry-seg | refname-safe，字节长 ≤ 255 | CLI/薄 API 写入口 |
| entry 附件 | 每文件 ≤ 64 KiB | CLI `--attach` |
| 快照 blob | ≤ 64 MiB | 服务端物化前；超限 `503`（写侧未强制，见 §9.4） |
| 未折叠 ref 数 | ≤ 20 000 / 请求（inbox + principals） | 服务端聚合；超限 `503` |
| CI body | ≤ 256 KiB | `walgit-wal::ci` 读侧（CI 协议 §11） |
| CI 产物 | ≤ 16 MiB/对象、≤ 32 个/结果 | runner/读侧（CI 协议 §8.2） |
| 注册表缓存 TTL | 60 s，写失效 | host 注册表 |

## 14. 安全与威胁模型

| 威胁 | 防线 |
|---|---|
| 伪造他人条目 | 签名 + 注册表验签；伪造者无他人私钥 → 恒 unverified（可见红，不参与计数/批准） |
| 越权写收件箱（policy 缺失/配置错） | 读侧收件箱归属校验（§4.5）：条目在他人收件箱里即使签名真也 unverified；生产仓库应对 `refs/collab/*` 配 policy（写入口分片） |
| 伪造注册表条目（顶替他人公钥） | policy 必须限制 registry ref 只允许本人写；allow-all 仓库无身份保证——这是部署责任，协议不装作有中心权威 |
| 篡改条目内容 | 签名覆盖 canonical 字节；改动任何字段 → 验签失败 |
| 重放/重复 | oid 内容寻址去重；重放无害 |
| 恶意遮蔽（同一 oid 植入他人收件箱、或同名 sha 遮蔽 CI 产物） | EntrySet 保留属主副本；产物读取在命名空间内逐候选验哈希，错的跳过 |
| 折叠覆盖（并发 gc） | CAS lease（基线 oid；无快照用空 expect）；绝不 `+`；先快照后删除 |
| 快照文档损坏/超大 | fail-closed，读整体报错；超 64 MiB 拒绝物化 |
| 撤销 key 的残留信任 | 服务端随 WAL 立即生效；客户端的 fetch **不带 prune** → 长寿命 checkout 里被删的注册 ref 可能残留，验签仍信旧 key——需 `git fetch --prune`（或删本地 ref）后聚合；`collab principal-fetch` 的 host 缓存会主动清除已消失项 |
| 浏览器私钥 | WebCrypto 密钥存 localStorage（可导出 JWK）：同源 XSS 可盗用签名身份，服务端只能人工 tombstone；升级路径 = 不可导出密钥 + 服务端登记确认（已知取舍） |
| 排序投毒（`ts`/`parent` 不被认证） | 线程顺序、卡片状态与 `done` 门禁都依赖 `(ts, actor, oid)` 顺序与作者自报的 `parent`；参与者可回拨 `ts`、制造孤儿条目影响投影（§6.2 的守卫截断让少量条目即可稳定触发）。当前防线：**无**（顺序是协作约定）——不得把顺序当安全判定；代码修复（守卫改界 / 链上 ts 回退处理）见后续 issue |
| 自审/重复批准 | merge 规则按 approve **条目数**计，不去重 principal；patch 作者可自 approve，`done` 门禁同理。当前防线：**无**——写权限与签名身份才是边界，`require_human_approvals` 是流程门禁；是否收紧见后续 issue |
| gc 写出超限快照 | CLI 写侧无 64 MiB 检查；超限后服务端全部聚合 `503`。恢复：先导出快照记录（`walgit wal materialize` / 持有快照的克隆）再移动该 ref，禁止直接删；修复见后续 issue |
| 时钟 | `ts` 不被认证；只影响排序/展示（及 CI TTL 活性），不参与签名/身份等安全判定；但投影顺序依赖它，见上一行 |

## 15. 版本与兼容

- **版本字段**：entry `version=1`（写出恒为 1，读侧结构性解析、当前不强制）；snapshot
  `version=1` + `kind="collab_snapshot"`（未知即读整体报错）；`.walgit/board.toml`
  `version=1`（未知即 fail-closed）；`.walgit/ci.toml` 见 CI 协议。
- **兼容契约是 canonical 形**：跨语言实现必须逐字节复现 §5.3；否则签名不可互认
  （§5.3.1 是现存偏差，修复前以验证端行为为准）。
- **不可变数据**：条目一经发布不改；折叠逐字携带原始字节；快照记录永不重序列化。
  仓库级「pre-1.0 无向后兼容、旧形状同批删除」的政策适用于**形状**（字段/路由/配置）；
  已落盘的条目字节与 WAL 一样是追加式数据，读取方必须在保留窗口内可回放。

## 16. 一致性验收（测试锚点）

改动协议行为时，下列测试必须同批更新并通过：

| 锚点 | 锁死的性质 |
|---|---|
| `walgit-wal/src/collab.rs::board_tests` | CI 线程不上板；board.toml fail-closed；同一集合任意读序字节一致；status 移动卡片；工作上下文继承/清空；未命中列不上板；根 title；report 投影排序（`report_lists_arrive_ordered_from_the_projection`） |
| `…::snapshot_tests` | git blob oid 已知答案（sha1/sha256）；全量/部分折叠字节等价（含 verified 标志）；撒谎 oid/不可解析记录跳过；快照 version/kind fail-closed；快照签名；EntrySet 去重与属主偏好 |
| `…::transition_tests` | `done` 门禁（needs-review + verified approve）；先 thread() 后判定；card prose 抽取 |
| `walgit-cli/src/collab_cmd.rs` 测试 | canonicalize 紧凑键序；签名/验签/篡改；gc key 匹配；折叠记录属主偏好；thread 链序/悬空；merge 只计非 `svc-`；report 确定性与计数；changed_refs；错收件箱不算 verified |
| `crates/walgit-cli/tests/collab_e2e.rs` | 真服务器全流程（注册→issue→链式评论→approve→新克隆聚合验签）；watch 回调；CLI vs 服务端看板字节一致 + status 移动；gc 折叠前后聚合字节一致且尾部存活；过期基线 lease 失败与崩溃重跑收敛；sha256 仓库折叠 |
| `crates/walgit-server/tests/web_api.rs` (`collab_*`) | 薄 API 签名条目（服务端不验签、聚合验）；report/thread 聚合；注册+verified；薄 API 遵守 policy；默认板投影；CI 产物按哈希供字节；超大快照物化前拒绝 |
| `crates/walgit-server/tests/policy_inbox.rs` | token 模式下收件箱按 actor 分片门禁 |

## 附录 A：CLI 命令与最小工作流

读（本地 refs，验签离线）：

```sh
W="walgit --config ~/.walgit/walgit.toml"
$W collab ls [--repo .]                 # 线程 id 列表
$W collab thread <id> [--repo .]        # 链序 + 每条的 oid/principal/verified，JSON
$W collab thread-heads [--repo .]       # thread id -> 链头 oid
$W collab pr <id> [--rules r.json]      # PR 视图 + merge 求值，JSON
$W collab report [--format text|markdown|html] [--rules r.json]
$W collab board  [--format text|markdown|json|hash] [--board f] [--rules r.json]
```

写（`--key` 传**文件路径**；`--push <remote>` 才上服务器）：

```sh
$W collab principal-register --principal alice --key ~/.walgit/keys/alice.ed25519 [--push origin]
$W collab entry --kind issue --id cc-ai-demo --actor alice --parent "" \
   --body '{"title":"…","body":"…"}' --key ~/.walgit/keys/alice.ed25519 --push origin
# 后续条目：--parent 填上一条命令输出第二列的 oid
$W collab principal-revoke  --principal alice [--push origin]
$W collab principal-fetch   [--remote origin] [--token $WALGIT_TOKEN]
$W collab watch --remote origin --interval 10 [--once] [--exec '<cmd>'] [--state <file>]
$W collab gc --actor alice --key ~/.walgit/keys/alice.ed25519 [--push origin]
```

host 注册表：`walgit principal register|rotate|list|revoke --url <host> [--principal p] [--key f]`。
CI：`walgit ci validate|run|status|log|artifacts`（`docs/D1_CI_PROTOCOL.md`）。

标准工作单元流程（与 `CONTRIBUTING.md` / `RULE_开发流程规范` 对齐）：issue → 开工
`comment` + `status: in-progress`（带 owner/worktree/branch/work）→ 实现后 `patch`
（base/head）+ `status: needs-review` → `review`（独立审查者）→ 合并后 `merge_result`
（`{"oid":…,"result":"merged","note":…}`，再补一跳 `{"merged":true,…}` 落看板）→
`status: closed`（或经门禁的 `status: done`）；全程可查 `board` / `pr`。
