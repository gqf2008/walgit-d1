# CLAUDE.md

@AGENTS.md

读入上面这份架构与协作规则后遵守它；本文件只是入口指针，规则一律不改写在这里。
当前已知与规则不符的现实（agent 不要误判）：

- `clippy -D warnings` 工作区门禁已全绿（issue #1 清偿，2026-09-03）；CI 的 clippy
  job 不再是 known-red——ruleset 将其列入 required checks 前，红即该 PR 引入回归、
  按约定阻塞合并。
- e2e 与 sim 套件已在 Windows 上跑通（issue #2 已合并）；CI 的 windows leg 覆盖
  快速层 + server 集成套件 + sim + e2e + web-test 与零 rustc 警告门（各 lane 清单
  单一来源在 justfile：SERVER_TESTS / WILDCARD_TEST_PKGS / CLI_TESTS，完备性守卫
  scripts/suite-guard.sh 推广到全部 crate，issue #137/#141）。walgit-cli 集成套件
  （ci_e2e/collab_e2e）**仅 ubuntu 跑**——平台缝逐文件注在 justfile CLI_TESTS 上方。
  跑 `just test` 若断言 SPA 资产（api_v1 的 /repos.js）须先 `just web-build`，否则
  占位 web/dist 造成环境性假红。测试偶发红先查 docs/WORKING_RULES.md known-flaky 名单与
  docs/RELEASE.md 的 CI 信号。
