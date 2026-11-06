# AGENTS.md — 通用协作框架 + 本仓项目章程入口

> **先读 [`docs/CHARTER.md`](docs/CHARTER.md)** —— 那是本仓库的项目协作章程
> （角色、派工、验收门禁、tracker 约定、D1–D70 决定链的引用）。本文件是通用
> 协作框架；两者冲突时以 `docs/CHARTER.md` 与全局规则（`~/.claude/CLAUDE.md`、
> `~/.agents/rules/`）为准，安全、授权、删除类规则以全局最高。

## 0. 参数表

| 参数 | 值 |
| --- | --- |
| 项目名 `<PROJECT>` | walgit — 对象桶为唯一事实源的 Git 宿主（Rust workspace + React SPA + 独立 Zig 客户端 `wgl/`） |
| 仓库根 `<REPO_ROOT>` | 本检出根目录 |
| 主分支 `<MAIN_BRANCH>` | `main` |
| 成员 worktree 根 `<WORKTREE_ROOT>` | `.worktrees/<unit>`（worktree 是身份，永不删除——见 CHARTER D62） |
| 成员分支前缀 `<BRANCH_PREFIX>` | `feat/<thread>`（每卡一分支，成员 worktree 内创建） |
| 构建命令 `<BUILD_CMD>` | `cargo build --workspace --all-targets`；SPA `just web-build`；wgl 客户端 `cd wgl && zig build`（zig 0.17.0） |
| 检查命令 `<CHECK_CMD>` | `just warnings`（零 rustc 警告）、`just clippy`（`-D warnings`）；SPA `pnpm run build`（oxlint `--deny-warnings` + tsc） |
| 测试命令 `<TEST_CMD>` | `just test`（lib/bins + 通配 crate + server 集成 + `wgl-test`）；SPA `just web-test` |
| 集成测试 `<INTEGRATION_CMD>` | `just wgl-e2e`（wgl 传输端到端）、`just e2e`（smart HTTP 对真 git）、`just sim`（故障注入集群）；全跑 `just ci` |
| 评审方式 `<REVIEW_CMD/流程>` | Leader 审查 + **另一位 principal** 的 verified `approve`（`merge_rule_eval`，CHARTER §2/§7） |
| 定时推进间隔 `<TICK_INTERVAL>` | 无定时器（D55）：`wgl collab watch --once` 挂监督者或外部 timer |
| 决策辅助模型 `<DECISION_MODEL>` | `Jev`（`jev-decision` 技能；仅用于 §6 列出的重大非确定性判断，确定性门禁永远是命令） |
| 跨团队 tracker `<TRACKER>` | 本仓 D1 协作层：`refs/collab/*` 卡片（经 `wgl mcp`（stdio）或 `wgl collab`）；镜像的 GitHub Issues 已停用（D46）——§7 的 tracker 流程以 D1 卡片执行 |

## 1. 角色

### Leader（架构师兼项目负责人）

- 架构设计与技术方案决策
- 任务拆解、派工与节奏管理
- 代码/设计审查
- 验收与合并分支
- 必要时与人类沟通、请求决策

**原则上 Leader 不写代码。** 代码、测试、配置、脚本的变更由团队成员完成；
人类明确要求时，Leader 可以直接处理非代码文件（如本文件、文档），但仍应优先
交给团队成员。（项目加严约定：写了自己补丁的 Leader 不得再审查它——CHARTER D59。）

### 团队成员（子代理）

- 以**子代理**形式工作，每个成员拥有独立的 git worktree 与分支。
- worktree 位于 `<WORKTREE_ROOT>/<unit>`，不得放在项目目录之外；分支命名
  `<BRANCH_PREFIX><unit>`。worktree 是协作身份，**永不删除**（退休 principal
  用 `wgl collab principal-revoke`，CHARTER D62）。
- 只在自身 worktree 内工作，不直接修改主工作区或其他成员的工作区；**先 claim
  再动手**（`wgl collab claim`，退出 2 = stand down 不得开工——CHARTER §8.4）。
- 完成后必须提交，并在最终报告给出：分支名、commit SHA、改动摘要、
  运行的命令与结果、风险或未验证项；每个问题都以签名 comment/status 写回卡片。
- 成员不得再派生下级子代理，避免任务失控；不 push main、不合并。

## 2. 交付流程

1. Leader 在 `<MAIN_BRANCH>` 上维护可构建、可测试的基线。
2. Leader 拆解任务并创建 worktree/分支派工；任务边界必须互不重叠；每张卡有
   机器可判定的验收标准（CHARTER §2 的派工模板）。
3. 成员在各自 worktree 内实现、自测、提交（小提交、Conventional Commits、
   每个 commit 可构建）。
4. Leader 审查 diff 与设计，按验收标准逐项验证；findings 带 `file:line` 与
   失败场景。
5. 通过后由 Leader 以 `--no-ff` 合并（合并后删除已合并分支；worktree 保留），
   推送并在卡片落 `merge_result` + `status: closed`；未通过则附具体失败信息
   退回返工。
6. 以**可交付**为目标：文档、测试、示例配置与实现同等重要。

## 3. 验收标准

合并任何分支前，Leader 至少执行（与 CHARTER §3 同源）：

```bash
just warnings        # 零 rustc 警告
just clippy          # -D warnings
just test            # 快速层（含 wgl-test）
just wgl-e2e         # wgl 传输端到端
just e2e             # smart HTTP 对真实 git
just sim             # 故障注入集群
just web-test        # SPA 改动时；另跑 just web-build
```

并通过以下检查：

- 不破坏对外契约（协议格式、公开 API、配置项、工具 schema）；协议改到桶通道的
  按被删面名 `docs/ROUNDTRIPS.md` 补往返注记；
- 审查发现的 Critical/Important 问题已修复，或有明确、可接受的处置结论；
- 新增配置项同步更新 `walgit.example.toml`、`docs/CONTRACT.md` 与示例；
  用户可见面（CLI/MCP/工作流）同批更新 `web/SKILL.md`、`skills/*/SKILL.md` 且
  `scripts/check-skill-covers-cli.sh` 保持绿；
- 新增行为有测试；修复缺陷有回归测试；不删除、跳过或削弱测试来换绿；
- 日志只写 stderr，标准输出不被调试信息污染；无凭据、无硬编码主机。

## 4. 推进节奏

- 本仓**无定时器**（D55）：Leader 以读板推进——`wgl collab watch --once` 挂在
  监督者或外部 timer 下；若一轮无新进展，只汇报状态，不重复派发相同任务。

## 5. 沟通与权限

- 卡片是记录：决策、阻塞、红测试与例外都以签名条目写在 `refs/collab/*` 的
  卡片上，过程闲聊不入卡。
- 影响交付方向、需要人类拍板的问题，Leader 直接向人类提问，不自行假设。
- **不要求在对话中粘贴密钥**。凭据统一放在约定的密钥存放位置（macOS Keychain、
  环境变量、私有凭据仓库），由人类配置后再通知验收。
- 对外的真实联调、发布、推送远端（`github` 镜像）、发版 tag、删仓库等外向或
  不可逆操作，需要人类确认后进行（`docs/RELEASE.md` 是发布路径）。
- 删除文件、强制推送、改写历史等难以恢复的操作，先确认再执行。

## 6. 决策辅助（Jev）

项目使用 **Jev**（`jev-decision` 技能）作为独立第三方决策参考。

- **适用范围**：重大且非确定性的判断——架构方案取舍、分支合并/退回、风险等级、
  优先级排序、是否接受某项变更。
- **不适用**：构建、测试、验签、格式校验、权限门禁等确定性校验必须由代码/命令完成，
  不得用模型替代（CHARTER §0：确定性门禁是命令，判断是 Leader 的）。
- **调用方式**：优先使用 `jev-decision` 技能
  （`~/.agents/skills/jev-decision/scripts/jev.py` 的 `choice` / `score` / `noul`
  子命令；若会话配置了 jev MCP，则使用对应工具）。MCP 方式先跑 `jev_health`，
  区分网络问题与模型拒绝；一次请求尽量合并多个问题以降低输入 token 成本。
- **输入要求**：`state` 必须包含决策所需全部事实（候选方案、约束、已知风险、
  验收结果），并做脱敏——不得发送 App Secret、访问令牌、用户隐私或未公开凭据。
- **效力定位**：Jev 输出只是**独立第三方参考**，不是决定。Leader 保留最终决定权，
  并必须在决策记录（`docs/DECISIONS.md` 或卡片签名条目）写明：问题、Jev 的
  答案/概率/confidence、最终决定及理由。Jev 不能覆盖人类指令、全局安全规则或
  确定性门禁。
- **失败处理**：Jev 不可用、超时、`answers` 缺键或落到未知选项时，按**保守分支**
  处理（不合并 / 不放行 / 维持现状），并注明「Jev 不可用」；不得因 Jev 故障阻塞
  交付。必须由人类拍板的决策，直接向人类提问。
- **成本与幂等**：按输入 token 计费；同一决策不要反复调用，结果按输入摘要缓存。
  不打印 key，key 的解析顺序遵循 `jev-decision` 技能文档。

## 7. 跨团队协同：以卡片为准

- **适用场景**：涉及其他团队、仓库、外部贡献者，或需跨团队排期与验收的工作。
- **唯一事实源**：每个跨团队请求开一张 D1 卡片（`refs/collab/*` 的 `issue`
  条目；镜像的 GitHub Issues 已停用，D46），至少写清：背景、期望结果、验收
  标准、负责人/团队、截止时间、相关链接（分支、commit、卡片线程）。
- **流程**：
  1. Leader 创建或指派卡片，指定 owner；状态变化以 `status` 条目留痕。
  2. 成员在独立 worktree 开发；提交信息引用卡片线程 id。
  3. 方案/接口变更、延期与风险在卡片留痕；需人类拍板的写 `needs-human`。
  4. 验收标准满足、卡片 `status: closed`（带 `merge_result`）后，分支才算交付。
- **工具**：`wgl mcp`（stdio：`collab_board` / `collab_thread` /
  `collab_report` / `ci_status` / `collab_publish` / `collab_register` 与
  `walgit://collab/*` 资源）或 `wgl collab`，两者是同一事实源的两种车道。
  宿主 `/mcp` 只保留仓库读工具（`repo_*`、`wal_ls`），协作面不再在那里（D70）。
- **安全**：卡片与条目中不得粘贴密钥、令牌、用户隐私；日志与配置必须脱敏。

## 8. 软件工程纪律

- 小提交、单逻辑变更、Conventional Commits、每个 commit 可构建。
- `main` 受保护：变更走成员分支 + 合并，绝不直推。
- 质量门禁（§3）在合并前跑；红门即阻塞，绝不绕行。
- 评审由不同 principal 做；findings 带 `file:line` 与失败场景。
- 契约（协议、API、配置键、工具 schema）与文档、示例同批变更；破坏性的先过人类。
- 新依赖要有理由；不加新认证路径；热路径不 `LIST`；不做无声长任务。
- 桶即仓库——其余一切是缓存或日志的读者（`docs/ARCHITECTURE.md` §3）。

## 9. Definition of Done

见 [`docs/CHARTER.md`](docs/CHARTER.md) §9（逐项核对表）。合并后 Leader 在卡片
落 `merge_result` + `status: closed`，或把阻塞写上卡。

## 10. 文档地图

见 [`docs/CHARTER.md`](docs/CHARTER.md) §10（一个事实一个家）。快速入口：
`GOAL.md`（为什么）→ `docs/ARCHITECTURE.md`（是什么）→ `docs/CHARTER.md`
（怎么协作）→ `docs/DECISIONS.md`（为什么是这样，D1–D70，append-only）。
