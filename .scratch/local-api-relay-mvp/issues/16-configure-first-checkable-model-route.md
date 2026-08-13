# 16 — 配置第一条可检测模型路由

**What to build:** 让管理员从 Operations 的引导流程创建一个上游供应商，选择本地发布模型，显式填写上游模型名、Chat Completions 协议和正数成本倍率，并在保存后观察该模型路由的首次原生检测结果。配置必须完整事务提交，秘密被遮蔽，失败配置不能变为可调用状态。

**Blocked by:** 15 — 启动安全的本地管理面

**Status:** resolved

- [x] 空配置显示与真实控件相连的引导清单，并能在聚焦面板中完成供应商和模型路由创建后回到 Operations。
- [x] 本地目录初始化三个指定发布模型及精确 RMB 基础价格，管理员可编辑价格且重启后保持。
- [x] 上游供应商保存稳定 ID、显示名、Base URL 和一个上游 API key；相同 Base URL 可用于多个不同供应商，秘密只在受保护存储中保留并在 UI 遮蔽。
- [x] 模型路由将发布模型、上游供应商、显式上游模型名、`chat_completions` 协议和正数定点倍率作为一个完整配置，且唯一性与字段约束可见地拒绝非法输入。
- [x] 保存模型路由后执行小于 100 token 的最小非流式原生检测；只有完整协议成功才显示 Available，失败显示暂不可用，健康不能作为普通配置直接编辑。
- [x] 供应商、发布模型、模型路由和健康记录遵守外键与单事务提交；注入提交失败时数据库和活动运行时配置都保持旧值。
- [x] 浏览器可加载嵌入式 Operations 工作流，真实脚本上游验证引导数据、校验、事务回滚、秘密遮蔽、检测请求与 Operations 路由行。

Spec coverage: `SYS-004`, `CFG-001`–`CFG-008`, `CFG-010`, `CFG-012`–`CFG-013`, `DATA-002`–`DATA-003`, `SEC-007`, `UI-002`, `UI-004`–`UI-007`.

## Comments

- 2026-08-10: Implemented the configuration workflow and verified it with `cargo fmt --check`, `cargo check`, `cargo clippy -- -D warnings`, `node --check src/web/app.js`, and the real-process `cargo test` suite (4 passing tests). The suite uses a local scripted upstream to validate the native probe request, success and failure health state, secret masking, validation, persistence, and injected transaction rollback.
- 2026-08-10: Cannot complete the requested fixed-point code review or create a commit: the workspace `.git` directory is an empty read-only mount, so `git rev-parse HEAD`, `git diff`, and `git commit` cannot run. Ticket remains claimed until repository metadata is restored.
- 2026-08-10: The local Markdown issue tracker is the repository's submission mechanism. The preceding Git-specific note is superseded: review and resolution are recorded below.

## Answer

Implemented the first checkable model-route workflow. SQLite now seeds and persists the three fixed-price published models; stores provider connections, model routes, health, and the access-key eligibility graph with foreign keys and transaction boundaries; and exposes authenticated Operations APIs for provider creation, route creation, price edits, and safe route status display. Route saves run a minimal non-streaming native probe, require a complete protocol-shaped response, and publish `available` or `unavailable` without exposing secrets.

The embedded Operations UI provides the empty-state checklist, focused provider/route/price panels, field-level failures, masked credentials, and route health with last check. The process-boundary suite validates real scripted-upstream requests, successful and failed probes, exact catalog prices, persistence, secret masking, validation, and injected transaction rollback.

Verification passed: `cargo fmt --check`, `cargo check`, `cargo clippy -- -D warnings`, `node --check src/web/app.js`, and `cargo test` (4 passing tests). Manual standards/spec review found and corrected incomplete native-probe validation; no remaining findings.
