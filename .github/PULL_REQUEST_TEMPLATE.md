> 提案（PR）= 一次 CAS 提交的包装：改动 + 验证 + 关联工作单元。
> 合并前：CI 绿 + 独立审查通过（重大改动必须有非作者审查者）。

## 关联

walgit collab thread: `<thread-id>`（GitHub 已只做镜像+发布，Issues 关闭；PR 模板仅作历史保留，
流程见 AGENTS.md 的 "Where this repository lives"）

## 变更

<!-- 一个职责一段：为什么改，而不只是改了什么 -->

## Verification

<!-- 每条：命令 + 结果（红前绿后；重构附行为等价证明） -->

- [ ] 编译/clippy 增量为零：
- [ ] 快速层（`just test` 等价命令）：
- [ ] 其他（性能基准 / sim / e2e）：

## 契约同步

<!-- schema/API/文档/配置是否随代码更新；无则写"无" -->

## Model Used

<!-- 使用的模型/工具，如 Claude Code (claude-opus-4-8)；纯人工写 human -->

## 审查

- [ ] 作者自查完成
- [ ] 独立审查者：@\<reviewer\>（重大改动必填）
