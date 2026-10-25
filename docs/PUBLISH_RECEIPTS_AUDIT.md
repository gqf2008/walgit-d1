# 发布 / 转发的精确收据：C5 / C7 / C8 / C9 与 LogSlotClaim 审计

审计对象：本仓库的发布路径（`crates/walgit-wal/src/publish.rs`、`crates/walgit-wal/src/handle.rs`、
`crates/walgit-server/src/forward.rs`、`crates/walgit-server/src/smart.rs` 的 receive-pack 分支）。
契约来源：`upstream/main` 的 `docs/spec/README.md`（C5 / C7 / C8 / C9、LogSlotClaim、FrontReplay）与
提交 `40d0899`（"Check bounded publication contracts and correct ambiguous receipts"）。本仓库在
该提交之前分叉，下列缺口均在本变更内修复并附 reproducer。

约定：

- 「代码位置」指本仓库修复前的代码；修复后同名符号仍在原文件。
- 「现有测试」= 本仓库修复前已有、能覆盖该义务的测试；「缺口」= 修复前没有测试能给出该义务的
  确定性证据。
- 每个缺口都先加测试（红），再改实现（绿）；红/绿命令与结果记录在文末「验证记录」。

## C5 — Complete submissions commit all-or-none; independent batch members remain independent

**代码位置**

- `process_batch`（`publish.rs`）逐请求 `verify_txn`，验证结果写入 `Verified { per_ref, valid }`；
  批内共享一次 log slot 与一次 manifest CAS。
- `enqueue_publish_at`（`handle.rs`）是 `publish_push` / `publish_ref_update` 的唯一入口。
- 回执形状：`PublishResult { seq, per_ref }`（`publish.rs`）。

**现有测试**

- `test_zero_oid_means_absent_for_create_and_delete`：all-zero oid 的 create/delete 语义。
- `a_landed_cas_is_ok_even_when_the_local_apply_fails_and_the_next_sync_repairs_it`：CAS 已提交后
  本地 apply 失败仍回 `ok`（回执以 WAL 为准）。

**缺口**

1. **同一条提交流内不是 all-or-none 回执。** `verify_txn` 逐 ref 判定；当同一提交里有一个 ref
   失败时，`valid = all_ok` 会阻止整条提交落盘（这部分是对的），但回执里**通过前置检查的 ref 仍然
   报 `Ok(())`**。客户端会看到「一条提交一半成功一半失败」，违反 C5 的 complete-submission
   all-or-none；正确回执是把该提交内其余 ref 一并标成拒绝。
2. **空 ref 事务 + pack 可以写入。** `publish_push(Some(pack), empty txn)` 修复前会为「只上传
   pack、不推进任何 ref」创建一条 PUSH log entry，把对象塞进 durable inventory 却没有引用；
   这不是一条合法的发布，必须在入口拒绝。
3. **独立批成员互不影响** 已满足：`valid_indices` 只收集通过验证的请求，一条被拒不会拖垮同批其它
   请求。这一点不需要改动，新测试 `noop_receipts_never_claim_a_siblings_log_entry` 同时固定它
   （no-op 与改 ref 同批、no-op 与被拒提交同批）。

**修复**

- `process_batch` 对每个请求计算 `changes`（pack 存在，或任一 ref 的 new 值/符号目标不同于当前
  working view）。`valid = all_ok && changes`。
- `all_ok == false` 时，把该提交内仍然 `Ok` 的 ref 改写为
  `RefError::Rejected("another ref in the submission was rejected")`——回执与提交边界一致。
- `enqueue_publish_at` 在入口拒绝 `pack.is_some() && txn.updates.is_empty()`（`WalError::Invalid`）。

**测试**：`noop_receipts_never_claim_a_siblings_log_entry`（`crates/walgit-wal/tests/wal.rs`）。

## C7 / C8 — Uncommitted candidates confer no serving authority; receipts distinguish committed, rejected and unknown

**代码位置**

- log slot 先 `PutMode::Create` 写入 `log/<first_seq>.pb`，再 CAS manifest（`claim_log_slot` /
  `process_batch`）。未进 manifest 的 log 对象是候选，读路径按 `manifest.log_segments` 解析，
  因此候选没有服务权——这一点架构上已经成立（`test_orphan_log_invisible_and_cleaned`）。
- CAS 结果解析：`cas_landed`（非 412 错误时判定「是否已提交」）；错误回执：`finish_with_error_msg`。

**现有测试**

- `test_orphan_log_invisible_and_cleaned`：孤儿 log 对象不可见、不被 `read_log` 返回。
- `a_landed_cas_is_ok_even_when_the_local_apply_fails_and_the_next_sync_repairs_it`：landed CAS
  回 `ok`。
- `sim::liveness_orphaned_log_segment_does_not_block_writers`：孤儿不阻塞后续写入。

**缺口**

1. **「没有证据」被当成「确定拒绝」。** 修复前非 412 的 CAS 错误经 `cas_landed` 返回 `None` 时，
   等待者收到 `WalError::Corrupt(...)`；但 `cas_landed` 返回 `None` 只表示**这一刻没看到**我们的
   segment——写可能晚到。调用者于是拿到确定的失败（git 侧即 `ng`），而 push 可能已提交。契约要求
   三态：committed / rejected / **typed unknown**。本修复新增 `WalError::CommitUnknown`，并在
   这条路径上回给调用者；manifest 重读本身失败也按 unknown 上报。
2. **「已提交」判定过弱。** 修复前 `cas_landed` 只比较 `key + first_seq`；同一个 `log/<seq>.pb`
   被删除后重建（content-token 版本令牌在 S3 形状下会 ABA）时，旧尝试会把新尝试的 segment 认成
   自己的提交。见下方 LogSlotClaim。

**修复**

- 新增 `WalError::CommitUnknown(String)`；`finish_with_error_msg` 用它回答等待者（publisher 任务
  自身仍返回原错误供日志记录）。
- `cas_landed` 只在 manifest **精确列出**本尝试的 segment（key、first_seq、size 都匹配）**并且**
  该 key 的字节与本次写入的字节逐字节相等时才返回 committed；否则返回 unknown，且不删除自己的
  segment（晚到的提交仍然有效）。

**测试**：`lost_cas_reply_is_resolved_or_unknown_without_losing_the_commit`
（`crates/walgit-wal/tests/wal.rs`）。

## C9 — Ambiguous forwarding never causes local replay; duplicate no-op requests publish no new entry

**代码位置**

- `forward.rs::receive_pack`：把 receive-pack body 流式转发给 `wal.push_broker_url`，返回
  `ForwardOutcome`。
- `smart.rs::receive_pack` 的转发分支：`Fallback` 且 body 可缓冲时，走本地 receive-pack 路径。

**现有测试**

- `forward::tests::broker_401_falls_back_instead_of_relaying`（issue #92 / D28）。
- `forward::tests::relayed_4xx_carries_www_authenticate`（issue #92）。
- `noop_receipts_never_claim_a_siblings_log_entry`（本变更）覆盖 C9 的另一条腿：no-op 不产生新
  WAL entry，因此 transport 的 at-least-once 重放不会造成 log 增长。

**缺口**

1. **502 / 503 / 504 触发本地重放。** 修复前只要 broker 回 gateway 状态就直接 `Fallback`，若 body
   可缓冲就本地再发一次。但 gateway 响应可能发生在 broker 已提交之后（或由 broker 前面的代理在
   转发后产生），本地重放会造成双发；客户端还可能拿到「本地这次」的结果而覆盖掉 broker 的真实结局。
2. **连接建立后的 send 错误也触发本地重放。** 修复前任何 `request.send()` 错误都 `Fallback`。
   只有**连接未建立**（`reqwest::Error::is_connect()`）才能证明请求没送达；连接建立后的超时/断流
   是 ambiguous。
3. **重定向与 transport 重试未禁用。** 默认 client 会跟随重定向，且 reqwest 的默认 retry 策略可能
   重发请求——两者都是「把一次推送变成多次」。307 还可能在跟随失败后落进 `Fallback`。
4. `Ambiguous` 结局缺一个明确的对外回答：修复前没有这个枚举值，`smart.rs` 只能把 ambiguous 当
   fallback 或 503。

**修复**

- `ForwardOutcome` 增加 `Ambiguous`。`reqwest::Client` 用
  `redirect(Policy::none())` + `retry(never())` 构造。
- send 错误按 `error.is_connect()` 分类：连接未建立 → `Fallback`；其余 → `Ambiguous`。
- 删除 gateway 状态的 fallback：任何 HTTP 响应都按 `Response` 原样回给客户端（保留
  `Retry-After` 透传）。
- `smart.rs` 对 `Ambiguous` 返回 **502** + `push broker response lost; outcome unknown;
  inspect remote refs before retrying`，不读 `fallback_bytes`、不本地发布。
- **保留 401 fallback**（issue #92 / D28）：broker 的 401 是 hop 凭据在做 receive-pack 之前的
  确定性拒绝，不是歧义；把它原样转发会让 git `erase` 用户的好 token。401 与 C9 不冲突，因为
  「ambiguous」的前提是结果不可判定，而 401 的结果是确定的「未处理」。
- 未送达/不确定时不再对不可缓冲的 body 说「503 后重试」：ambiguous 走 502 + unknown 文案，语义是
  「先检查远端 refs 再决定」。

**测试**：`crates/walgit-server/tests/forward.rs::ambiguous_delivery_and_gateway_responses_never_publish_locally`
（memory store + 真实 front/broker socket；断言三种结局下 WAL log 与 refs 都不变）。
单元测试 `forward::tests::{gateway_status_is_relayed_not_replayed, post_delivery_loss_is_ambiguous,
redirects_are_relayed_not_followed}` 固定同一分类。

## LogSlotClaim — per-attempt byte uniqueness; a receipt resolves to exact bytes, never a content-token alias

**代码位置**

- `claim_log_slot`：`log/<seq>.pb` 的 `Create`、孤儿 grace、burn 序号。
- `LogSlot { key, version, first_seq, burned }`：一次尝试的句柄。
- `sweep_burned` / `drop_own_slot`：提交后清理 burned 孤儿 / 412 后删除自己写的版本。
- `cas_landed`：lost-CAS 的解析。

**现有测试**

- `test_orphan_log_invisible_and_cleaned`（orphan 不可见）。
- `sim::liveness_orphaned_log_segment_does_not_block_writers`（orphan 不阻塞）。
- `a_supersession_without_its_marker_does_not_commit`（supersession marker 的失败原子性；相邻机制）。

**缺口**

1. **重试尝试的字节是确定性的。** `encode(seq)` 对相同的 entries 产生相同字节；S3 形状的版本令牌
   是内容哈希，删除后重建同一 key 会得到相同 token，迟到的第一次 DELETE 可以匹配并删除第二次尝试
   的 segment。契约要求的可移植守卫是**每次尝试字节唯一**（一个 fresh nonce）。
2. **提交证据只按 key+seq。** 见 C7/C8 缺口 2；同一 key 的旧/新尝试无法区分。
3. **segment size 来自闭包副作用。** 修复前三处调用用 `let mut frame_len = 0` 在 `encode` 闭包里
   记录长度；它与实际写入的字节只在「闭包最后一次调用」上偶然一致，无法作为证据。

**修复**

- `claim_log_slot`：`encode(seq)` 后 `frame::decode_entries` 校验完整、非空；在第一条 entry 的
  `meta` 写入 `walgit.publication_nonce = uuid v4`，重新编码为本次尝试的字节。`LogSlot` 保存
  `bytes: bytes::Bytes`，manifest 的 `LogSegmentRef.size` 一律取 `slot.bytes.len()`。
  （`meta` 是 proto 的既有 free-form 字段；无 proto 变更，append-only 语义不变，读路径忽略未知 key。）
- `cas_landed`：manifest 必须列出 (key, first_seq, size) 且对象的**精确字节**等于 `slot.bytes`。
- 删除三处 `frame_len` 副作用（push / compact / settings 发布）。

**测试**：`publish::claim_tests::recreated_claims_have_distinct_bytes_and_resolution_checks_identity`
（`publish.rs` 内置单元测试；MemoryStore）。另由
`lost_cas_reply_is_resolved_or_unknown_without_losing_the_commit` 从调用者角度固定「解析必须校验身份」。

## 范围外但记录在案

- 当时 `publish_settings_impl` 的非 412 CAS 错误不调用 `cas_landed`，直接返回 `WalError::Store`；
  2026-10-25 追补已让 SETTINGS 与其它两个发布入口共用同样的 landed 判定，见下节。
- `txn.atomic` 字段在本仓库的 WAL 发布路径中未被读取；C5 的 all-or-none 由「一条提交要么整条
  valid、要么整条拒绝」在 `process_batch` 内保证，与 `atomic` 的值无关。本卡不改这个语义。

## 2026-10-25 追补：已生效的 CAS 也可能返回 412

线上症状为 manifest 仍引用最新 `log/0000000000003000.pb`，对象存储却返回 NotFound。代码审计确认一个
可产生完全相同损坏形态的缺口：第一次 manifest CAS 已在服务端生效但响应丢失，SDK/代理透明重试后，
第二次请求因旧 precondition 返回 412。此前 push / compact / settings 都把 412 直接当成「本次未提交」，
删除自己的 log slot 后重试；这会把已经提交的 WAL 段删掉。

修复后三个入口都在**任何** manifest CAS 错误后调用 `cas_landed`，以 manifest descriptor + 精确 segment
bytes 判定是否已提交；只有证据表明未提交的普通 412 才允许 `drop_own_slot`。无法读取提交证据时保留 slot，
绝不把「未知」当成删除许可。回归测试
`applied_cas_returning_412_keeps_every_committed_log_segment` 用 `FaultPlan::p_cas_fail_after` 注入
「应用写入后返回 412」，覆盖 push、compact、settings；修复前在 push 即以缺失 log 段失败，修复后三段均保留。
成功路径 round trips 不变；探测只落在 CAS 错误路径（`docs/ROUNDTRIPS.md` §2）。

第二次 review 补充 landed 采纳的两个约束：`cas_landed` 返回的 `(manifest, version)` 必须来自同一次
manifest GET（禁止内容一次读、version 另一次 HEAD 的跨读配对）；若 probe 读到的是后续 writer 在我们
landed 之后发布的 raced manifest，三个发布入口只安装**本次 attempt 自己的 snapshot**并留空 version，
让下一次 sync 无条件重取、重放 foreign tail 并 reconcile packs——不能把 foreign manifest 记成
`applied_seq`/`packs_revision` 已应用。tier-2 compact 的 witness 在 raced 采纳时也使用本次 COMPACT
seq 的 pack set，而不是 foreign manifest 的 pack set。

## 验证记录

环境说明：本机没有安装 `just`（`just: command not found`），所以没有运行 `just` 本身；按 justfile 的
recipe 逐条执行其底层命令，并在提交说明中如实标注：

- `just test` = `cargo test --workspace --lib --bins`
  + `cargo test -p walgit-store -p walgit-git -p walgit-wal -p walgit-bundle --tests`
  + `cargo test -p walgit-server <SERVER_TESTS>`（本变更已把 `--test forward` 加入该清单）。
- `just clippy` = `cargo clippy --workspace --all-targets -- -D warnings`。
- sim = `cargo test -p walgit-server --test sim`。
- `just web-build` 的等价命令已先运行（`web/` 下 `pnpm install --frozen-lockfile && pnpm run build`），
  避免 `api_v1` 对 SPA 资产的占位假红。

### 红（修复前）

- `cargo test -p walgit-wal --test wal noop_receipts_never_claim_a_siblings_log_entry`
  → FAILED：`assert!(matches!(error, Err(walgit_wal::WalError::Invalid(_))))`（空 ref 事务 + pack
  被接受）。
- `cargo test -p walgit-wal --test wal lost_cas_reply_is_resolved_or_unknown_without_losing_the_commit`
  → FAILED：`absence of evidence is not a definite rejection`（unknown 被报成 `Corrupt`）。
- `cargo test -p walgit-server --test forward`
  → FAILED：`left: 200, right: 502`（lost reply 触发了本地重放，且本地真的发布了 ref）。

### 绿（修复后）

- `cargo test -p walgit-wal --test wal noop_receipts_never_claim_a_siblings_log_entry` → ok。
- `cargo test -p walgit-wal --test wal lost_cas_reply_is_resolved_or_unknown_without_losing_the_commit` → ok。
- `cargo test -p walgit-wal --lib claim_tests` → ok
  （`recreated_claims_have_distinct_bytes_and_resolution_checks_identity`）。
- `cargo test -p walgit-server --test forward` → ok（lost → 502、gateway → 503、redirect → 307；
  三种结局下 log 与 refs 均不变）。
- `cargo test -p walgit-server --lib forward::tests` → 5 passed（含新增 3 个分类测试）。
- `cargo test -p walgit-store -p walgit-git -p walgit-wal -p walgit-bundle --tests` → 全绿
  （含 `wal.rs` 44 passed）。
- `cargo test -p walgit-server <SERVER_TESTS>` → 全绿（15 个 suite，含 `forward`）。
- `cargo test -p walgit-server --test sim` → 20 passed。
- `cargo clippy --workspace --all-targets -- -D warnings` → 绿。
- `cargo test --workspace --lib --bins`：除下列与本变更无关的既有时间敏感测试外全绿。
  `walgit_git::copy_into_place_tests::concurrent_readers_observe_notfound_or_the_complete_file_never_a_prefix`
  在并行满载时自报 “reader took no samples during the copy — the race went untested”，单独运行通过
  （两次）；随后用 `-- --skip <该测试>` 跑完整 workspace，再用原命令单跑该测试，均绿。本变更未触碰
  `walgit-git`。
