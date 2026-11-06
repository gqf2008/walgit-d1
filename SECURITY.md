# Security Policy

## 报告

私有漏洞一律走 GitHub 的 **Private vulnerability reporting**（仓库 Security 标签页 → Report a
vulnerability），不要开公开 issue。

## 范围

- 接收端与鉴权（smart HTTP、`policy.json`、token/oidc 三模式）——见 docs/ARCHITECTURE.md §1.3 的安全契约。
- 对象存储完整性（CAS 语义、不可变对象、租约）。
- LFS 与 bundle 分发路径。

报错优先级里"fail-closed"是硬约定：`Config::validate` 拒绝不安全配置；修复不得引入静默降级。
