# packfile-uris 评估：是否应成为第二条静态交付通道

Context: **这是一份决策文档，不是实现。** 读者是决定交付通道走向的维护者、审查 bundle-uri 的 reviewer，
以及将来若真要实现 selection/静态 pack 路由的工程师。依据是 `upstream/main` 的
`docs/PACKFILE_URI_DESIGN.md`、`docs/PACKFILE_MIGRATION.md`、`AGENTS.md` D42–D49，以及提交
`60fde9e` / `bf65c01` / `72b5e07` / `4ff4f7a` / `40d0899` / `ff82208` / `d9ed2a6`（2026-09-11/12）。
本仓库 bundle-uri 的设计之记录仍是 `docs/BUNDLE_URI_DESIGN.md`；成本模型仍是 `docs/ROUNDTRIPS.md`。

**状态：建议保持（keep）——不新增 packfile-uris 通道、不迁移、不删除任何 bundle 代码/配置/UI。**
本文件给出五维对比（§2）、两项机制（retired membership / exact coverage certificate）的独立采纳分析（§3）、
若将来实现的验收矩阵与引擎 guard（§4），以及**触发重新评估的条件**（§5）。上游明确写了
"stack position is not permission"：下面区分「设计可借鉴」与「本仓库现在要不要做」。

## 0. 决策摘要

**建议：保持 bundle-uri 作为唯一静态交付通道；不把 packfile-uris 作为第二通道引入；不从 bundle-uri 迁移。**

不采纳的理由（按决定性排序）：

1. **鉴权是硬阻断。** stock git 的 packfile URI 下载是一次普通 HTTP GET，不走 git 的 credential
   helper；`fetch.uriProtocols=https` 只开启协商，不增加认证下载（上游 design §6 自己这样写）。
   本仓库的生产形态是 `token` / `oidc`（`none` 仅限 loopback），所以 stock 客户端在 URI GET 上必然
   401，通道在最需要它的部署里直接不可用。剩下的路径只有 signed URL（有 TTL、绕过请求级授权、
   缓存必须私有）或自带补丁/独立发行的客户端——后者上游自己也留在未完成的 release gate 里。
   bundle-uri 则天然带凭据：git 的 bundle 下载走它自己的传输/凭据路径，`require_read` 在路由上照常生效。
2. **淘汰语义与我们的成本模型冲突。** packfile URI 指向的就是 `wal/<checksum>.pack`；我们的
   `gc` 在 `compaction.retention_superseded`（默认 168h）之后删除 superseded pack，而 weekly base
   rebuild 每周替换旧的 tier-2 base。上游要求 retired membership **永不过期**（因为不存在"所有下载
   都已完成"的证明），在本仓库等于每周多留一个约 32 GB 的 base、外加每次折叠的旧 pack，全部不可回收；
   这与 AGENTS §1.1 / §2.5 直接冲突。详见 §2.5、§3.1。
3. **覆盖证明的缺失不是补一个字段就能解决。** 我们的 pack 是任意几何折叠结果；tier-2 base 是
   `git repack -a -d`（**全 refs**，含 `refs/collab/*`、`refs/walgit/*`），没有 code/meta 分组、
   没有 packing-policy identity、没有多 pack 的 closure/依赖证明。安全发出 URI 需要上游的第 2/3 层
   （groups + conserving lifecycle + exact certificates），也就是重写 §2.5 与 D18/D21/D22，属于
   D 级决策，不是本评估的后续小步。详见 §3.2。
4. **收益是边际的。** bundle-uri 已经把 clone 字节移出 upload-pack：实测 full clone 只有 2.77 MB
   过前端、32.70 GB 走静态文件；stale fetch 过前端 52.4 MB。weekly full 是 base 的 compose
   （32.3 GB 约 2 s、零字节过 host、无 index-pack）。packfile-uris 省掉的是 calendar 调度和 list
   CAS（maintainer 每 pass ≤ 3 个 round trip），不是每客户端成本。见 `BUNDLE_URI_DESIGN.md` §7。
5. **它不解决最大的剩余成本：客户端 `index-pack`。** monorepo full clone 的 1,499.9 s 里有
   1,063.6 s 是客户端索引 weekly；URI 交付同样要让客户端索引 base pack。上游的 `packfile-indexes`
   是 stock 客户端不请求的可选扩展，而且仍需独立验证/分发客户端；同样的收益可以走我们已有的
   bundle + git patch 通道（`BUNDLE_URI_DESIGN.md` §6c 选项 a，`docs/patches/`）。
6. **最需要它的引擎恰好不支持。** tmpfs 上 base 放不下时 `run_fetch` 自动切到 gix + remote reader；
   gix 引擎自己组包、不会写 URI section。native-only 的 URI 通道会在大型仓库（也就是唯一值得做 URI
   的场景）自动关闭。上游也承认这条：每个引擎要么真正交付 URL，要么不得做 synthetic subtraction。

「设计可借鉴」的部分——精确覆盖证书、retired membership、引擎 guard——在 §3、§4.3 单独给出结论；
它们都可以独立于「是否新增通道」评估，目前也都建议**不做**（或只保留为 #195 witness 的已有形态）。

## 1. 事实基线

### 1.1 本仓库今天的形状

- **解析但不广告。** `crates/walgit-server/src/smart.rs` 的 `parse_fetch_request` 接受
  `packfile-uris <proto…>` 并写入 `UploadPackRequest.packfile_uris_protocols`；
  `crates/walgit-git/src/lib.rs::build_v2_fetch_request` 会把它原样转发给 stock git。
  但 `v2_capability_advert` 只广告 `bundle-uri`，**从不广告 `packfile-uris`**，也从不写 URI section，
  也没有 `/{o}/{r}[.git]/wal/<checksum>.pack` 路由。因此该字段今天是惰性的：客户端不发，服务端不选。
- **引擎选择。** `git.upload_pack_engine = auto | git | gix`；`smart.rs::run_fetch` 把 `Auto` 解析为
  stock git，`handle.remote_served()` 非空时强制 gix + `remote::Faulter`。gix 路径调用
  `upload_pack_gix_with`，按 commit-graph/tree-diff 自己组包。
- **鉴权。** `server.auth.mode = none | token | oidc`；`none` 要求 `server.listen` 是 loopback
  （fail closed）。`token`/`oidc` 下 `require_read` 对缺失/失效凭据返回真 401，challenge 按通道区分
  （git 通道含 Basic + Bearer，浏览器面只有 Bearer）。
- **bundle 通道。** v2 `bundle-uri` 能力 + `bundle-uri` 命令；列表是 CAS 的 `bundles/list.pb`
  （clone list `bundles/list`、catch-up list `bundles/catchup`、blobless `?filter=blob:none`）；
  静态文件走 `/{o}/{r}[.git]/bundles/...`，由 `static_object` 提供强 ETag / 304 / Range / If-Range /
  416 / HEAD / `immutable`，可选 `X-Accel-Redirect`（loopback + `X-Walgit-Capabilities`）或
  `serve_via = "signed_url"`（失败回退 proxy）。路由先 `require_read`。
- **GC/retention。** superseded pack 写 `wal/_superseded/<checksum>` 标记；`gc` 单元在标记年龄超过
  `compaction.retention_superseded` 后，经 `Manifest.reclaiming` CAS 声明、fresh manifest 存活复查，
  删除 pack + side-files + 标记（`crates/walgit-server/src/ops.rs::gc_superseded_packs_with_grace`，
  #175/#177）。bundle 的 retention 在列表 CAS 之后立刻 `ops::delete_pruned` 删除被剪掉的 bundle 对象。

### 1.2 上游的事实与边界（只记与本仓库决策相关者）

- 第一刀（`60fde9e`）是**删除 bundle runtime**，保留普通 dynamic clone/fetch 与旧格式解码器；
  后续的 group/certificate/lifecycle/transport 各层都还未被它证明。
- 上游设计文本自己声明：`packfile-indexes` 是可选扩展、stock 客户端不请求、也不能凭它认定
  trusted-index 客户端；protected clients 在可分发、经过验证的认证客户端出现前保持 dynamic；
  retired membership 与字节**不设过期、不设数量上限**；"No optimal-cost or zero-extra-read claim is made"。
- 上游迁移文档把「删除 bundle runtime」当作第 1 个 review unit，并明确：**stack position is not
  permission to leave a half-proved URI path enabled**；representative scale receipts 是单独 gate。
- 结论：上游方案的经济前提是 **conserving lifecycle + 不可回收的 retired pack**；我们不是这个前提，
  所以不能把"上游已经这么做了"当作本仓库的采纳理由。

## 2. 五维对比

### 2.1 交付路径

| | bundle-uri（现状） | packfile-uris（上游方案） |
|---|---|---|
| 交付物 | `bundles/<strategy>/<slot>-<sha>.bundle` = git bundle header + pack 字节。大仓库的 weekly full 是 `header ∘ tier-2 base` 的 compose；其余 full 走 `git bundle create`/gix，daily/hourly 是自包含 `pack-objects` | 直接复用 `wal/<checksum>.pack`（+ `.idx`；上游另有可选的 `packfile-indexes` sidecar） |
| 生产者 | maintainer 按 calendar slot；weekly slot 先触发 BaseRebuild（SSD host），再 compose；每个 strategy 一个 lease | **无调度器**；每次 fetch 从 live/retired manifest + 证书里选择 |
| 发现 | v2 `bundle-uri` 能力 + `bundle-uri` 命令；列表 CAS；recipe 把 `bundles/catchup` 写进 `fetch.bundleURI` | v2 `packfile-uris` 能力 + fetch 参数；URI section 在 `packfile` section 之前内联返回 |
| 字节流向 | bucket/edge → client，作为**一个静态文件**；客户端 `index-pack`（bundle unbundle） | bucket/edge → client，作为 **pack 文件**；客户端逐个 `index-pack`；服务端对已选 roots 做 subtraction，余量动态生成（非 thin） |
| 客户端要求 | git 的 bundle-uri（git ≥ 2.38）；凭据由 git 自己的传输路径携带；filter 匹配需要我们的 client patch | 支持 `fetch.uriProtocols` 的客户端；stock 客户端 URI GET **不带凭据**；index sidecar 需要定制客户端 |
| 服务端状态 | `bundles/list.pb`（slots/skips/retention），无 per-client 状态 | 无 catalog；选择从 manifest + 证书得出，无 per-client 状态 |
| 失败行为 | 单次 bundle 下载失败后 git 不会重试；D17 给"1 小时内取过 list 的 principal"一次 upload-pack 全量 fallback；slot 是 slot 的纯函数，之后可重建 | URI 下载失败就是该次 fetch 失败（客户端不会自动改走 dynamic）；pack 被 GC 删除后该 URL 永久 404 |
| 它**不**解决 | 客户端 index-pack；advertisement 时看不到 clone 的 filter/shallow（所以 blobless/shallow 配方要显式选链或关闭 bundle-uri） | 客户端 index-pack；token/oidc 的认证下载；淘汰；gix/remote 引擎 |

**结论：**两者在"把历史字节从应用前端移走"这件事上是同一类方案；packfile-uris 的独特优势只有一个——
选择发生在 fetch 内部，**能看到 filter/depth**，因此 blobless/shallow 可以在请求期选择而不是在
advertisement 期猜。我们已经有更便宜的等价物：blobless 独立列表 + client filter patch
（`docs/patches/`、`BUNDLE_URI_DESIGN.md` §6b）。其余交付路径差异都不改变成本模型。

### 2.2 调度与清单

- **bundle-uri 的调度是 amortized 的**：calendar slot 是 as-of 语义（`seq = 最高 created_at ≤ slot`），
  `creationToken = slot epoch`，backfill 最老优先；`bundles/list.pb` 一次 CAS 发布，retention 与
  `skipped` verdict 都在同一个列表里；每个 maintainer pass 的最坏成本是 `ROUNDTRIPS.md` 的那几行
  （list GET → ≤ 1 retention CAS → ≤ 1 verdict CAS），list render 由 list 自身 version 缓存
  （一次 ~15 ms HEAD + 变更时一次 GET）。**客户端 clone 只读一次 list**，与 ref 数/pack 数无关。
- **packfile-uris 的"清单"是每次请求的证明**：要回答"哪些 pack 的对象集恰好是 wants 闭包减去
  真实 haves"，需要 (i) 与 captured seq 绑定的 exact refs snapshot，(ii) 同一 generation 的完整
  checksum 集与依赖闭包，(iii) packing-policy identity，(iv) 每次发布 CAS 时重新验证 prospective
  live members。上游预算是一次成功的普通选择读 1 个 snapshot、阈值选择最多 3 次。
- 本仓库已有的最强积木是 #195 的 **witness checkpoint**：
  `walgit-wal/src/checkpoint.rs::write_witness_checkpoint` 在 tier-2 publish 时写入 exact
  `checkpoints/<seq>/{refs.pb,checkpoint.pb}`（create-if-absent，不动 live fold）；
  `log_reader.rs::refs_at_seq` 负责 replay。它绑定的是"base pack 集 + base seq 的 refs"，
  足以支撑 weekly compose，但**只覆盖 tier-2 base**，且验证它需要读 checkpoint 对象（额外的 GET；
  今天只有 bundle compose 走这条路）。
- **结论：**bundle 的调度成本落在 maintainer 的每 pass 预算里；packfile-uris 把成本移到每个客户端
  fetch 的选择上，并要求写路径多一条"发布时证明覆盖"的不变量——我们的 `publish_compact_impl`
  现在没有这条不变量。没有证据表明这笔交换在本仓库的场景里划算。

### 2.3 缓存与鉴权

- **bundle-uri：**路由 `GET /{o}/{r}[.git]/bundles/...` 先 `require_read`；`static_object` 给强
  ETag（store version）、`If-None-Match` → 304、`If-Range` + 单段 Range/206/416、HEAD 只读元数据、
  `Cache-Control: public, max-age=31536000, immutable`。可选 edge offload：仅当请求带
  `X-Walgit-Capabilities: accel-redirect` 且 TCP peer 是 loopback，服务端回
  `X-Accel-Redirect` + store URL/authorization/key。可选 `signed_url`：`signed_get_url(key, ttl)`
  （默认 TTL 1h），store 不支持/拒绝时回退 proxy，永不因签名失败让列表失败。
  **凭据可用**：git 的 bundle 下载在它自己的传输栈里，token/oidc 下 `require_read` 看到的是正常凭据。
- **packfile-uris：**路由形态相同（上游：`/{o}/{r}[.git]/wal/<checksum>.pack` + `.idx`，
  普通读授权 + live/retired membership 复查 + 同样的 GET/HEAD/ETag/304/Range/416/If-Range）。
  问题在于**谁在下载**：stock git 的 URI GET 是裸 HTTP，没有 credential helper，也没有 401 重试。
  因此在 `token`/`oidc` 下：
  - 直接指向本站的 URI → 401，fetch 失败；
  - signed URL → stock 客户端可用，但等价于把受保护字节在 TTL 内变成"持有 URL 即可读"，
    缓存必须是私有的，且 TTL 与 32 GB 级下载/恢复的时长要匹配；上游要求的
    "trusted edge must authenticate even warm cached requests" 在 stock 客户端上无法同时满足；
  - 自带补丁/独立发行的客户端 → 上游也未完成的分发与资质 gate。
  上游自己的结论与此一致：protected clients stay dynamic until a distributable authenticated
  client is independently qualified；不要为了迁就客户端把受保护静态路由开放给匿名。原文（design §6）：

  > `fetch.uriProtocols=https` enables standard URI negotiation; it does not add authenticated download
  > behavior to stock clients.

  一个容易混淆的点：测试 harness（`tests/lib-auth.sh`）用静态 `http.<host>.extraHeader` 认证，那个
  头会被 curl 层套到任何 HTTP 请求上；但这不是我们发给用户的 recipe（`setup.rs`/install.sh 发的是
  `credential.https://<host>.helper`），而 credential helper / 401 重试不在 URI 下载路径上。把 bearer
  写进 config 也不是可刷新、可撤销的认证路径，不能作为通道的鉴权设计。
- **唯一无需客户端补丁的逃生口是"匿名读"**：`token` 模式可以把 `anonymous_read = true`，那样 stock
  客户端的裸 URI GET 也能工作——但这等于把整个仓库的读开放给匿名（`oidc` 则被 `validate` 强制
  `anonymous_read = false`），与 `none` 仅 loopback 的 fail-closed 设计相悖，也不是大仓库的部署形态。
  把它当作 packfile-uris 的采纳路径，等于用鉴权模型换一条边际收益。
- **immutable/Range 两者都满足**（内容寻址 + `static_object`）；差别不在缓存契约，而在**授权能否
  附着到下载请求上**。

### 2.4 round-trip 深度

今天（bundle-uri，happy path，`ROUNDTRIPS.md` 口径）：

| 步骤 | 深度/请求 |
|---|---|
| `info/refs`（能力） | 1 次 conditional GET manifest（`freshness_ttl=0` 时；否则 0） |
| `bundle-uri` 命令 | 1 次 refs sync（通常 304） |
| `bundles/list` | 1 次 HEAD（~15 ms，读 list 自身 version）+ 变更时 1 次 GET；render 按 version 缓存 |
| N 个 bundle | N × 1 静态 GET（signed URL 时完全不过应用；proxy 时 1 GET/文件） |
| 余量 fetch | 1 次 fetch + 动态 pack |

应用侧顺序深度 ≈ 3 + N 个静态 GET；list 成本与 ref/pack 数无关。

假设实现 packfile-uris：

| 步骤 | 深度/请求 |
|---|---|
| `info/refs` | 1 次 conditional GET（新增要广告 `packfile-uris`） |
| `fetch`（selection 在这一请求内） | 0 次额外 manifest（我们本来就要 sync）；若证书内联则为 0，若读 checkpoint 对象则 0–2 次（上游上限 3） |
| N 个 pack | N × 1 静态 GET；每个路由若要"freshly revalidated membership"，再加 0–1 次 conditional GET（或用一个 freshness TTL 摊掉） |
| 余量 | 同一个 fetch 内动态生成（非 thin） |

**差异只有 ±1–2 个顺序 round，且被静态字节和客户端 `index-pack` 淹没。**它省掉 bundle-uri 命令
和 list GET/HEAD，但增加了证书读取和每个 URI 路由的 membership 复查；上游也明确不声称零额外读。
若将来实现，`ROUNDTRIPS.md` §2 需要新增一行（例如 "v2 fetch with packfile-uris selection |
1 manifest（已同步）+ 0–2 certificate reads | ≤3 | `selection.rs`"）以及静态路由的 0–1 行；
**本次不建议新增通道，因此不改该表**（按卡片要求，§5 给出重新评估条件）。

### 2.5 淘汰与在途下载语义

这是**本仓库最不能照搬上游**的一维。

- **bundle-uri：**bundle 对象按名字内容寻址；retention 在列表 CAS 后立即删除被剪掉的条目对象
  （`walgit-bundle/src/ops.rs::delete_pruned`）。一个已经读了旧 list、正在下载的客户端可能撞上 404。
  风险被三件事压小：(i) 每个 strategy 保留 2 个（fulls 2 个 + 其下 chain；hourlies 2 个），
  列表刚读到时提到的对象在短时间内仍在；(ii) slot/内容/token 都是纯函数，缺失 slot 之后可重建
  （D22 自愈），只是不在读路径上重建；(iii) D17 给"1 小时内取过 list 的 principal"一次 upload-pack
  全量 fallback，失败后果是成本（或大仓库 tmpfs 上的 TooLarge），不是静默错误。
- **packfile-uris：**URL 指向 live 或 superseded pack。我们的 GC 在
  `retention_superseded`（默认 7 天）后删除 superseded pack；weekly BaseRebuild 每周把旧 base
  变成 superseded。上游的规则相反：**已发出的 URL/字节永不过期、不设数量上限**，因为不存在
  "每个已发出的下载都完成了"的证明；他们把存储/manifest 增长当作显式接受的代价。若照搬：
  - 每周约 32 GB 的旧 base + 每次折叠的旧 pack 永久保留；monorepo 的 bucket 成本线性增长；
  - `Manifest` 是 CAS 且每次读都要 GET 的"小对象"，retired set 放进去会让它随 pack 总数增长
    （每次 build 都变大，push 的 CAS payload 也变大）；放独立对象则每个下载多一次 GET；
  - `gc` 的 `retention_superseded` 语义、`wal/_superseded` 标记的"年龄记录"（#175）、
    以及 bundle compose 依赖的 checkpoint 保留（#195/#206）都要改成"永不回收"或永久的例外；
  - 在途/断点续传跨过一次 GC 就是硬失败；bundle 的失败至少还有 D17 fallback，而 URI 失败没有
    dynamic 重试。
- **结论：**引入 packfile-uris 必然连带重做 `gc`/retention 与 weekly base 生命周期；这是 D 级决策，
  不是"加一条通道"。若只在 bundle 通道上担心同样的在途问题，可考虑一个有界的
  "retired bundle grace"，但当前 chain/retention 设计已把窗口压得很窄，不建议为此先改协议。

## 3. 两项机制的独立采纳分析

即使不切换通道，卡片要求单独判断这两项是否值得独立引入。

### 3.1 Retired membership（已发出的 URL/字节不再过期）

**上游定义：**manifest 永久记录 retired checksum；静态路由接受 live 或 recorded-retired；retired
只授权旧 URL，不能支撑新的 coverage；checkpoint trimming 必须保留存活 proof 需要的 snapshot
descriptor；retired 记录与对象没有时间/数量 cap，直到出现单独的安全回收协议。原文（design §3）：

> Retired checksum records and objects do not expire and are not count-capped: no general collector
> proves that every issued download has finished. Storage/manifest growth is an explicit tradeoff
> until a separate safe collection protocol exists.

**本仓库代码落点：**

- `crates/walgit-proto/proto/walgit/v1/wal.proto`：`Manifest.packs` 只有 live 集；
  `ReclaimingPack` 是"GC 正在回收"的 fence，不是 retired 授权；没有 retired 字段。
- `crates/walgit-wal/src/publish.rs::write_superseded_markers` / `update_reclaiming`；
  key `wal/_superseded/<checksum>`（`keys::superseded_key`）。
- `crates/walgit-server/src/ops.rs::gc_superseded_packs_with_grace`：标记年龄 →
  `retention_superseded` → claim CAS → fresh manifest 存活复查 → 删除 pack + side-files + 标记。
- `crates/walgit-config/src/lib.rs::CompactionConfig::retention_superseded`（默认 168h，
  `walgit.example.toml`: "keep old packs for provenance / rewind"）。
- bundle 侧：`crates/walgit-bundle/src/lib.rs::apply_retention` + `ops::delete_pruned`。
- 未来静态路由：`crates/walgit-server/src/static_object.rs`（复用它即可拿到完整 HTTP 契约）。

**协议影响：**

1. 需要一个下载路径可读、且**不能是 LIST** 的 retired 集合（原则 VII）。三个候选：
   (a) 内联进 `Manifest`——每次读已付费，但 manifest 是 CAS 的"小对象"，O(packs ever) 增长不可接受；
   (b) 每 checksum 一个独立对象（现有 `wal/_superseded/<checksum>` 已经是这个形状）——每个下载
   1 次 GET/HEAD，或者用 TTL 缓存摊掉；
   (c) 一个 retired index 对象——多一次 GET 且需要自己的 CAS。
2. `gc` 必须永不删除 retired 对象；`retention_superseded` 从"provenance window"变成"never"，
   `wal/_superseded` 从可回收标记变成永久记录。
3. checkpoint/WAL GC 必须永远保留被任何 retired proof 引用的 refs/checkpoint descriptor。
4. 静态路由与缓存策略要把 retired pack 当作可服务对象（private immutable），并处理 404 与
   `Manifest.reclaiming` 的竞态（reader 与 GC 之间没有 fence；目前 fence 只保护 publisher）。

**风险：**

- **无界存储**：monorepo 每周一个 ~32 GB base 永不回收；这是最直接、最不可接受的成本。
- **manifest 增长与 CAS 负担**：内联会让每次 push 重写越来越大的 CAS 对象；独立对象把成本移到
  每个下载请求。
- **与现有 GC 不变量冲突**：#175 的 marker 是"年龄记录"，#195/#206 的 checkpoint 回收有 fail-closed
  规则；改成"永不"需要重新论证整个 `gc` 单元的边界与 `Manifest.reclaiming` 的语义。
- **没有安全回收协议**：上游也承认这一点；任何有限窗口都是猜测，而"猜测"在 URI 通道上的后果是
  断点续传硬失败。

**是否独立引入？****不。** 对 bundle 通道，当前"确定性重建 + 每 strategy 保留 2 个 + D17 fallback"
已经把风险压到"成本退化"而非"克隆损坏"；对 pack-uri 通道，它只在我们要引入该通道时才成为前置条件。
若现场真的出现 bundle 404（见 §5 触发条件 4），更可取的第一步是"有界 retired grace"（按对象年龄，
不是按 manifest 条目），而不是永久保留。

**结论：保留为「设计可借鉴」，不实现。**

### 3.2 Exact coverage certificate（绑定策略/refs 快照 seq/完整 checksum 集）

**上游定义：**每个可用的 coverage certificate 必须绑定 group name + 规范化的 packing-policy
identity + captured WAL seq + 精确的 content-addressed refs snapshot key + 重建该 group 闭包所需的
完整 checksum 集；snapshot 的 digest 必须验证；发布 CAS 必须重新检查 prospective live members 与
scope；stale policy 让旧证书失去资格但不删对象；label / tier 2 / 全局 `covers_seq` / 宽泛 audience
marker 都不能替代证明。

**本仓库代码落点：**

- `crates/walgit-wal/src/checkpoint.rs::write_witness_checkpoint`（#195）：在 tier-2 publish 时写
  `checkpoints/<seq>/{refs.pb,checkpoint.pb}`，`Checkpoint.refs_key` 指向 exact `RefSnapshot`，
  `Checkpoint.packs` 记录当时的 pack 集；create-if-absent、不动 live fold。
- `crates/walgit-wal/src/log_reader.rs::refs_at_seq`：checkpoint + tail 的纯函数 replay；
  bundle compose 的 `refs_at_seq(base_seq)` 就走它。
- `crates/walgit-wal/src/publish.rs::publish_compact_impl`：tier-2 publish 的 choke point，
  未来任何"发布时证明"都必须挂在这里，并在每次 CAS retry 上重验。
- `crates/walgit-proto/.../wal.proto::PackRef`：有 `checksum/pack_size/object_count/seq/tier/
  kind/derived_from`；`PackKind::HISTORY` 是 D18 的 blobless 派生 pack。
- `crates/walgit-proto/.../wal.proto::Manifest`：live `packs` 内联（refs 级零额外读），
  `checkpoint` 指针，`settings` 内联。

**已经等价拥有的部分：**tier-2 base + 它的 HISTORY pack，在 base seq B 上，通过 #195 witness
**已经**绑定"B 时刻的精确 refs snapshot + 当时的 pack 集"；base 是 `repack -a` 的闭包，history 是
其 commits+trees 派生。bundle weekly 的 compose 用的就是这个 base 和 `refs_at_seq(B)`；客户端还会用
bundle header 里的 refs/prerequisites 自行验证。因此**我们唯一真正可证书化的交付物**（weekly full /
blobless weekly）已经有一份事实上的证书，只是它没有以通用 `CoverageCertificate` 消息的形式存在。

**要泛化到「任意 pack 可作为 URI」还缺：**

1. **group/audience 切分。** 我们的 base 是 `git repack -a -d`（全 refs），push pack 也可能混合
   `refs/heads/*`、`refs/tags/*`、`refs/collab/*`、`refs/walgit/*` 的对象。上游禁止 metadata-only
   对象进入 code group 的 coverage；我们没有 group 概念，也没有"这个 pack 的对象都属于哪个闭包"的
   元数据。
2. **packing-policy identity。** 上游要求把 group 定义/subtraction 规范化后纳入 policy identity；
   我们的配置变更没有这个身份，证书无法表达"按哪个策略切的"。
3. **多 pack 闭包与依赖证明。** 除 tier-2 base 外，fresh/medium pack 是几何折叠的产物，不是
   "refs@S 的闭包"；没有依赖图/完整 checksum 集，`Manifest.packs` 的 live 集不等于一个可证明的
   coverage。
4. **发布时重验。** `publish_compact_impl` 现在只保证 CAS 与 supersede 列表；没有"prospective set
   对 snapshot 的闭包检查"。
5. **请求期选择状态机。** wants/haves/filter/depth/unknown OID/tag-only baseline/have-only 的判定，
   真实 ACK 保留、synthetic ACK 抑制、URI/索引 section 的顺序，以及 §4.3 的引擎 guard。

**协议影响（若实现）：**

- append-only proto：新 certificate 消息/字段（`PackRef` 之外的 group、policy identity、refs key、
  checksum set、generation）；旧的 WAL/checkpoint 解码器必须继续可读。
- 新的 per-repo settings section（D24 只能覆盖 `[bundles]/[maintenance]/[compaction]/[upstream]`，
  需要扩展 schema 与校验），`walgit.example.toml` 同步，bounds（上游提议 32 groups / 256 selectors /
  16 KiB）。
- 读路径：验证 snapshot digest（多一次或两次对象读，除非内联）；写路径：每次 CAS 重验。
- 文档/测试：`docs/ROUNDTRIPS.md` 新行、sim 的请求预算、selection/engine 单测、`web/SKILL.md` 与
  `skills/walgit/SKILL.md`（若暴露配置面）。

**风险：**

- **错误的正证书 = 坏克隆**：客户端缺对象、`fsck` 失败；更糟的是 synthetic have 污染协商。
  负证书是安全的（回退 dynamic），所以实现必须 fail closed。
- base 含 collab/meta 对象这一事实，让"code coverage"在不改 lifecycle 的前提下无法诚实声称。
- 证书生命周期与 checkpoint/GC retention 耦合（§3.1）；跨 SHA-1/SHA-256、跨 object format。
- 维护成本：这是 §2.5/D18/D21/D22 层级的重写，远超一条交付通道。

**是否独立引入？****不。** 现有 witness checkpoint 对 weekly compose 已经够用；bundle header 本身
就是客户端可验证的 refs+prerequisites 描述。若将来引入静态 pack 交付，证书是**正确的地基**，但
最小安全切片只能是"tier-2 base + HISTORY，且 code 闭包无 meta 混入"——那仍然绕不开 group 与
lifecycle。**结论：保留为「设计可借鉴」，不实现；不要把 #195 witness 泛化当作本卡片的后续小步。**

## 4. 若将来实现：验收矩阵与引擎 guard

本节是**给未来实现者的验收表**，不是本仓库现在的行为（现在是全部 dynamic）。矩阵的行是请求形态，
列是三种鉴权；每个单元格再按三种引擎给出结果（读作 7 形态 × 3 鉴权 × 3 引擎 = 63 个判定）。
图例：

- `U` = 该引擎可以走 URI 通道（真正发出 URL）；
- `D` = dynamic（不选择、不合成 have）；
- `U*` = 只有 signed URL 或经过资质验证的补丁客户端才能走（stock 客户端在本鉴权下拿不到凭据）；
- `C` = 条件性：只有证书/请求条件满足才 `U`，否则 `D`；
- `C*` = `C` 的条件性 + `U*` 的凭据限制。

### 4.1 请求形态 × 鉴权（每格：native / gix / remote）

| 请求形态 | `none`（loopback） | `token` | `oidc` |
|---|---|---|---|
| full clone（zero-have，无 filter/deepen） | U / D / D | U* / D / D | U* / D / D |
| branch fetch（want heads/x，有 haves） | C / D / D | C* / D / D | C* / D / D |
| tag fetch（tag/peeled，含 include-tag） | D / D / D（tag-only baseline 无 branch root，上游明确 dynamic） | D / D / D | D / D / D |
| blobless（`filter=blob:none`，full history） | U（只允许 HISTORY pack，绝不把混合 OBJECTS pack 当 history）/ D / D | U* / D / D | U* / D / D |
| shallow / deepen / deepen-since | D / D / D（请求特定形状，上游明确 dynamic） | D / D / D | D / D / D |
| stale fetch（haves 远落后） | C（只发有证书的成员；余量 dynamic；不得假设任意 topic have 覆盖默认分支历史）/ D / D | C* / D / D | C* / D / D |
| explicit-OID fetch（want 任意 sha） | D / D / D（除非证书显式覆盖该闭包） | D / D / D | D / D / D |

补充规则（三列共有）：

- `token`/`oidc` 下，只要客户端是 stock git，`U*` 就**不是**默认交付；它要求 signed URL（TTL 与
  大文件恢复时长匹配、缓存私有）或独立验证的认证客户端。默认必须是 `D`。
- 任何一行在 `anonymous_read=false` 且请求未通过 `require_read` 时都不得进入选择逻辑。
- `U*`/`C*` 也不得把受保护字节放到可被匿名命中的 warm edge cache 上；上游对 edge 的要求是
  "warm cached requests 也必须认证"。

### 4.2 引擎维

| 引擎 | 选择 | synthetic have/ACK | 说明 |
|---|---|---|---|
| native（`UploadPackEngine::Git`；`Auto` 在非 remote-served 时解析到这里） | 仅此引擎允许 | 仅对**本次响应真正写出的 URI** 允许 | splice：URI（+可选 index）section 在 `packfile` 之前；有 URI 时余量 pack 禁用 thin；保留真实 ACK，仅抑制 synthetic ACK；保持 sideband-all/ready/done/wait-for-done/no-progress |
| gix（显式 `Gix`） | 永不 | 永不 | `local.upload_pack_gix_with` 自己组包，不知道 URI section；必须在请求被改写**之前** gate |
| remote-served（base remote-served，自动 gix + `Faulter`） | 永不 | 永不 | 这是 tmpfs 大仓库的自动路径；native-only 通道在这里不可用，正是它最该有用的地方 |

### 4.3 引擎 guard：**未真正发出 URL 不得合成 have/ACK**

上游对这一条的原文是（design §5）：

> **Every engine must either deliver the selected URLs or avoid synthetic subtraction.** Native, gix,
> remote-served and mount/local dispatch need explicit tests.

机器可校验的表述（未来实现必须写成测试）：

1. **同生同灭。** 一个 synthetic have 只能为"同一响应中已经成功写入 URI section 的 pack"添加。
   如果 URI section 写入失败、请求被拒绝、或响应在写出前中止，转发的请求必须与客户端原请求
   **逐字节一致**（不得部分扣减）；宁可不选择，也不能只合成一半。
2. **ACK 只抑制自己合成的。** 只有服务端自己加的 synthetic have 对应的 ACK 可以抑制；客户端真实
   发送的 have 必须得到真实 ACK。
3. **选择只发生在执行该 fetch 的引擎分支里。** 今天 `run_fetch`（`smart.rs`）才决定引擎；
   如果在 `parse_fetch_request` 或通用 handler 里提前选择，就会泄漏到 gix/remote。具体 guard：
   `Selection` 只能从 `UploadPackEngine::Git` 的 arm 返回；`upload_pack_gix_with` 与 remote
   `Faulter` 分支收到 `packfile_uris_protocols` 时必须忽略它，并按原请求组包。
4. **不转发客户端的 `packfile-uris` 给 stock git 后假装有 URI。** 现在
   `build_v2_fetch_request` 会原样转发；stock git upload-pack 不会因此产生 URI section。未来若做
   splice，必须在转发前剥掉客户端的 `packfile-uris` 参数，由我们自己的 section 取代它；否则
   既没有 URL，也不能合成 have。
5. **选择的前置条件（任一不满足即 `D`）：** 没有 `packfile-uris` 参数；协议列表不含 `http`/`https`；
   have-only 协商（没有 wants）；shallow/deepen/filter 不受支持；want 是未知对象；证书缺失/过期/
   不完整/跨 generation；鉴权形态不允许下载；pack 数量/大小超过上限（若采纳上游的 64 URI / 32 MiB
   阈值）。
6. **测试形状。** (a) 单元测试：把同一请求分别交给三种引擎，断言 gix/remote 的输出与不带
   `packfile-uris` 时逐字节一致，且只有 native 路径写出 section；(b) 响应级测试：逐 pkt-line 断言
   "没有 URI line 就不存在 synthetic ACK"，以及 URI section 一定在 `packfile` 之前；
   (c) 负例：证书缺失、stale policy、错误 generation、lost CAS、部分 seal 都必须回退 dynamic。

## 5. 重新评估的触发条件

本评估**不建议新增通道**，因此 `docs/ROUNDTRIPS.md` 的预算表保持不变（现状行仍然有效）。
只有满足下列任一条件，才重新打开本评估并更新预算表；「上游已经实现/已经删除 bundle」本身不是条件：

1. **认证下载进入 released stock git**：一个已发布的 stock git 在 packfile URI GET 上携带凭据
   （或发行版认证客户端通过资质验证），并能对 `token` 模式主机完成真实 clone。
   上游也把这条列为未完成的 release gate。
2. **trusted index 进入 released stock git**：客户端会请求/使用 `packfile-indexes` sidecar。
   这是唯一能真正移动 monorepo 客户端 `index-pack`（1,063.6 s / 1,499.9 s）的杠杆；在它之前，
   同样的收益应走 bundle + 我们已有的 client patch 通道（`BUNDLE_URI_DESIGN.md` §6c(a)）。
3. **本仓库采用 conserving、coverage-certified 的 lifecycle**：有 group/policy identity、exact
   certificates、frozen segments，并且发布 CAS 重验闭包——即对 §2.5/D18/D21/D22 的 D 级替换。
   *仅*把 tier-2 base 证书化（我们已有 #195 witness）不够：它不改变鉴权、淘汰或客户端成本。
4. **出现安全的有界回收协议**：能证明"某个已发出 URL 的下载已经结束"（下载计费/edge 侧的期限），
   从而不必永久保留 pack；或者存储预算变化到可以接受每周一个旧 base 永久保留。
5. **bundle 通道的现场证据越线**：出现 bundle 下载 404、`bundles.require` 的一次性 fallback 告警、
   或因 list/chain 导致的 clone 失败（给定阈值，例如一周内任何一次 404）；或者某个部署根本无法
   启用 `transfer.bundleURI`，且前端字节被 upload-pack 主导。
6. **上游给出代表性规模的性能/正确性 receipts 和明确的 rollback floor**（他们自己的 §7）。
   在 receipts 之前，只能借鉴设计，不能借鉴结论。

## 6. 本次不做什么

- 不删除、不迁移、不替换任何 bundle 代码/配置/UI：`crates/walgit-bundle`、`bundles/list.pb`、
  `bundles/require`、recipes、Clone 菜单、`web/API.md` 的 bundle 面全部保持。
- 不新增 `packfile-uris` 能力广告、不新增静态 pack 路由、不改 `gc`/retention、不改
  `docs/ROUNDTRIPS.md` 预算表（因为不建议新增通道）。
- 本文件把 `docs/BUNDLE_URI_DESIGN.md` §8 的 "packfile-uris evaluation" 开放项标记为已完成
  （见该文件的交叉引用）；不改变任何运行时行为。
- 相关但独立的后续（不在本卡片范围）：(a) 若出现现场 404，考虑有界的 retired bundle grace；
  (b) 通过现有 client patch 通道做 pre-indexed bundle 交付；(c) 继续把 #195 witness 作为
  未来任何证书实现的种子，但现在不泛化。

## 7. 证据与门禁

本评估读取的原始材料（可复现）：

```sh
git show upstream/main:docs/PACKFILE_URI_DESIGN.md
git show upstream/main:docs/PACKFILE_MIGRATION.md
git show upstream/main:AGENTS.md | sed -n '389,500p'      # 上游 D41–D49
git show --stat --oneline 60fde9e                          # 删除 bundle runtime
git show upstream/main:docs/ROUNDTRIPS.md                  # 上游成本表
grep -n "packfile-uris" crates/walgit-server/src/smart.rs  # 解析点，无广告
grep -rn "packfile_uris_protocols" crates/walgit-git/src   # 转发点
```

本仓库事实的代码锚点：`smart.rs::parse_fetch_request`（`packfile-uris` 分支）、
`smart.rs::v2_capability_advert`（只有 `bundle-uri`）、`smart.rs::run_fetch`（引擎/Serve 选择）、
`walgit-git/src/lib.rs::build_v2_fetch_request`、`static_object.rs`（静态契约）、
`bundles.rs`/`lib.rs`（bundle 路由）、`ops.rs::gc_superseded_packs_with_grace`（淘汰）、
`checkpoint.rs::write_witness_checkpoint`（#195 witness）、`log_reader.rs::refs_at_seq`。

门禁（本卡片要求）：`just test`、`just clippy` 均须保持绿；本文件不触碰 Rust 代码，预期不影响结果。
实际执行结果记录在 patch entry 的 body 里（命令 + 结果）。
