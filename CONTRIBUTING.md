# Contributing

动手前先读 [AGENTS.md](AGENTS.md) —— 它是这个仓库的宪法：约束、决策、工作规则都在里面，本文件只是路标。

## 环境

- 需要 stable Rust 工具链（`rustup` 默认即可，不再锁定版本）；还需 `protoc`、`git ≥ 2.46`；web UI 需要 `node 24 + pnpm`。
- 一键环境：`nix develop`（flake.nix），或按 README 的 Platforms 段落自备。
- Windows 原生可编译可测（NTFS 卷 + Developer Mode）；容器仍是推荐部署形态。

## 流程

主仓在 walgit（`origin = http://127.0.0.1:8081/gqf2008/walgit.git`），GitHub 只做镜像与发布；
issue/PR/评审/看板都是 `refs/collab/*` 里的签名条目（见 `AGENTS.md` 的 "Where this repository lives"
与 `walgit` skill），不再走 GitHub 的 label/PR 流程。

1. 从线程开始：用 `walgit collab entry --kind issue` 建工作单元（同类同机制 ≥3 条合并为一个批次线程 + checklist）。
2. 在 worktree/分支上开发（基于 `origin/main`），不在 main 直接改；开工与状态流转用 `status` 条目记账。
3. 自跑门禁：`just warnings && just clippy && just test`（当前 clippy/warnings 有预存债务，
   见跟踪 issue —— 增量必须为零）；smart HTTP 改动加 `just e2e`。
4. 实现完挂 `patch` 条目（base/head），重大改动必须有独立审查者，审查结论写 `review` 条目。
5. 本地合并进 main 并 `git push origin`，随后记 `merge_result {"merged": true}`（卡片进 `merged` 列），
   需要归档再补 `status closed`（`status done` 是等价的门禁终态，看板同样有列），清理分支/worktree。

## 提交

Conventional Commits（`feat|fix|chore|docs|refactor|test|perf(scope): 描述`），一个提交一个逻辑变更，
信息说"为什么"。

## 发布

- 版本语义化（semver）：`v<major>.<minor>.<patch>`；在 walgit 侧打 tag 并 push `origin`，镜像循环把它带到
  GitHub，触发 `release.yml`（构建 linux/windows 产物、从 Conventional Commits 生成 changelog、挂到 GitHub Release）。
- 发布本身是一个工作单元：开 `batch` issue 列发布清单（里程碑 `v0.1` 是首个目标）。
- 里程碑与批次的关系：一个里程碑一个版本，issue 挂里程碑表示"进这个版本"。
