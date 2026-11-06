# CLAUDE.md

@AGENTS.md

读入上面这份通用协作框架后遵守它；**本仓的项目协作章程是
[`docs/CHARTER.md`](docs/CHARTER.md)**（角色、派工、验收门禁、tracker 约定、
D1–D69 决定链的引用），与本框架冲突时以章程与全局规则为准。本文件只是入口指针，
规则一律不改写在这里。
当前已知与规则不符的现实（agent 不要误判）：

- `clippy -D warnings` 工作区门禁已全绿（issue #1 清偿，2026-09-03）；CI 的 clippy
  job 不再是 known-red——ruleset 将其列入 required checks 前，红即该 PR 引入回归、
  按约定阻塞合并。
- e2e 与 sim 套件已在 Windows 上跑通（issue #2 已合并）；CI 的 windows leg 覆盖
  快速层 + server 集成套件 + sim + e2e + web-test 与零 rustc 警告门（各 lane 清单
  单一来源在 justfile：SERVER_TESTS / WILDCARD_TEST_PKGS / wgl 车道（wgl-test、
  wgl-e2e），完备性守卫 scripts/suite-guard.sh 推广到全部 crate，issue #137/#141；
  D69 起 walgit-cli 不再有集成套件——协作命令家族迁去了独立的 wgl Zig 客户端。
  跑 `just test` 若断言 SPA 资产（api_v1 的 /repos.js）须先 `just web-build`，否则
  占位 web/dist 造成环境性假红。测试偶发红先查 docs/WORKING_RULES.md known-flaky 名单与
  docs/RELEASE.md 的 CI 信号。
