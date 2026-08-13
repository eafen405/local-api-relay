# 36 — 对齐模型路由尝试链的领域命名

**What to build:** 纯重构（wide rename）。调用记录下"模型路由尝试链"相关的类型与前端标识目前使用丢失领域限定词的通用命名（如 `AttemptRecord`、`RelayRouteCandidate`），与 CONTEXT.md 领域词汇（模型路由尝试、模型路由）不一致；把它们对齐到领域术语，同时同步前端展示文案。行为不变，全套现有测试保持绿色。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 模型路由尝试相关的类型/函数命名使用领域词汇（模型路由尝试），不再使用丢失 "route" 限定词的通用名。
- [x] 候选路由类型命名与领域术语（模型路由）一致。
- [x] 前端展示文案与类型命名同步更新。
- [x] 全套现有测试通过，无任何行为变化。

Spec coverage: None — pure refactor.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. Wide rename 对齐 CONTEXT.md 领域词汇：`RelayRouteCandidate`→`store::ModelRouteCandidate`、`AttemptRecord`→`store::ModelRouteAttempt`、`AttemptDraft`→`ModelRouteAttemptDraft`、`AttemptOutcome`→`ModelRouteAttemptOutcome`、`begin_attempt`/`finish_attempt`/`resolve_committed_attempt`→`begin_model_route_attempt`/`finish_model_route_attempt`/`resolve_committed_model_route_attempt`；前端按钮文案 `Attempts`→`Model route attempts`（app.js 初始 markup 与 toggle handler 两处同步）、CSS 类 `.attempt-*`→`.model-route-attempt-*`（app.css 7 条规则 + app.js 构造/querySelector 点）、JS 函数 `attemptChainMarkup`→`modelRouteAttemptChainMarkup`。JSON wire 字段 `attempts` 与 `attempt.*` 子字段、`outcome`/`commit_phase` 字符串、`call_attempts` 表、`aria-label="Model route attempt chain"` 与空态文案均保持原样（对外契约冻结）；测试只断言 JSON wire 未引用 Rust 类型名故零改动。全量验证：`cargo check --all-targets` 通过、clippy 零警告、`cargo test` exit 0（secure_management_surface 78 + packaging_lifecycle 27 全绿，105/105）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket36-change-record.md`) completed. Standards axis: 无 documented-standard 违规（repo 无编码标准文件，AGENTS.md 仅流程性内容；rustfmt/clippy 已由工具强制）；重命名完整无残留（旧标识符在 src 与 tests 中零命中），新命名诚实一致（Draft/持久记录区分清晰、wire 字段名与类型名差异为有意的契约冻结）；两处轻微不对称作为 judgement call 记录：`mark_committed`/`committed_route_id`（server.rs）未加 `model_route_attempt` 前缀——前者命名的是提交阶段而非尝试、后者已含 "route" 限定词，不在 ticket 的"丢失 route 限定词的通用名"范围内，且有意的函数级范围界定（checklist 第 1 项覆盖 begin/finish/resolve 三个尝试生命周期方法）；按钮 toggle 文案与初始 markup 的动词不对称为既有行为。Spec axis: 四个 checklist 项全部落实（尝试类型/函数用领域词汇、候选类型对齐模型路由、前端文案两处同步、全套测试 105 绿），无缺失/无越界（wire 字符串、DB DDL/查询、测试均未动），无 implemented-but-wrong（无残留旧类选择器、无引用旧命名的注释）。

## Answer

实现完成，纯重构（wide rename）。模型路由尝试链相关的类型、函数与前端标识已全部对齐 CONTEXT.md 领域词汇（模型路由 / 模型路由尝试），前端展示文案同步更新；对外行为逐字节不变，全套现有测试通过、clippy 零警告。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **store.rs 公共类型**：`RelayRouteCandidate` → `ModelRouteCandidate`（候选路由对齐"模型路由"；struct、`eligible_chat_routes`/`eligible_responses_routes` 返回类型与两处行读取构造）；`AttemptRecord` → `ModelRouteAttempt`（尝试记录对齐"模型路由尝试"；struct、`CallRecord`/`NewCallRecord` 的 `attempts` 字段、`list_call_records` 行读取构造）。
- **server.rs**：`AttemptOutcome` → `ModelRouteAttemptOutcome`（枚举定义、`as_str`、全部变体引用，`as_str` 输出字符串不变）、`AttemptDraft` → `ModelRouteAttemptDraft`（CallRecorder 在途草稿，与持久记录区分）、三个尝试生命周期方法 `begin_attempt`/`finish_attempt`/`resolve_committed_attempt` → `begin_model_route_attempt`/`finish_model_route_attempt`/`resolve_committed_model_route_attempt`；`relay_precommit_fallback_loop`/`relay_non_streaming`/`relay_streaming` 的候选参数与 `effective_candidates` 同步为 `ModelRouteCandidate`。
- **前端**：按钮文案 `Attempts` → `Model route attempts`（初始 markup app.js:197 与 toggle handler app.js:250 同步）；CSS 类 `.attempt-chain`/`.attempt-chain-wrap`/`.attempt-head`/`.attempt-row` → `.model-route-attempt-*`（app.css 7 条规则与 app.js 全部构造/`querySelector` 点）；JS 函数 `attemptChainMarkup` → `modelRouteAttemptChainMarkup`（定义与调用点）。`aria-label="Model route attempt chain"` 与空态文案本已领域对齐，未改。
- **对外契约冻结**：JSON wire 字段名 `attempts` 与 `attempt.*` 子字段、`outcome`/`commit_phase` 字符串值、数据库 `call_attempts` 表与列均未动；`tests/secure_management_surface.rs` 只断言 wire 形状（`call["attempts"]` 等）未引用 Rust 类型名，测试文件零改动。
- **验证**：`cargo check --all-targets` 通过、clippy 零警告；`cargo test` 全套 exit 0——`secure_management_surface` 78 个全绿（含 `fallback_attempts_form_an_ordered_chain_with_normalized_failures`、`stream_terminated_after_commit_records_one_attempt_without_usage`、`canary_fields_never_enter_call_records_or_attempts` 等尝试链用例），`packaging_lifecycle` 27 个全绿。src 全量扫描确认旧标识符零残留。
