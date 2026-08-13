# 34 — 抽取中继共享的预提交 fallback 循环

**What to build:** 纯重构。非流式与流式中继路径目前逐函数重复同一段"预提交 fallback 循环"（尝试开始、发布模型替换、失败后隔离当前模型路由并按原候选顺序转交下一条）；把它抽成单一共享 helper，Fallback 与隔离语义完全不变，全套现有测试保持绿色。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 非流式与流式中继共用同一套预提交尝试循环逻辑，不再逐函数重复。
- [x] 预提交失败 → 隔离当前模型路由 → 按原有序候选集转交的行为与重构前一致（现有 Fallback 测试全绿）。
- [x] 首事件已提交后的失败仍只终止下游流，不拼接其他路由生成、不重试（ROUTE-014/015 不变）。
- [x] 全套现有测试通过，无任何行为变化。

Spec coverage: None — pure refactor.

## Comments

- 2026-08-12: Implementation completed. Extracted the shared pre-commit fallback loop `relay_precommit_fallback_loop` in `src/server.rs`, used by both `relay_non_streaming` and `relay_streaming`; the loop owns candidate iteration, model substitution, upstream send, status triage (attributable vs health-neutral), attempt bookkeeping, quarantine, and candidate-exhaustion finalization, with each path's success handling supplied as an owned-recorder closure. Behavior preserved exactly: full suite (78 + 27 = 105 tests) green, clippy clean.
- 2026-08-12: Code review (dual-axis) completed. Standards axis: no documented-standard violations; two judgement calls fixed — renamed the outcome enum `PreCommitSuccess` → `PreCommitOutcome` (it carries both a committed and a fall-through path) and added the small `precommit_fallthrough(recorder, failure)` constructor so the six fall-through branches in both closures name only the failure. The remaining optional smells (three-layer `published_model_name` shadowing plus one unconditional String clone per candidate in the failure path; the owned-recorder BoxFuture signature) were evaluated and intentionally left. Spec axis: conforms to the ticket, no missing or extra behaviour, attempt-chain/ROUTE-009/ROUTE-014/015 semantics match the old implementation line by line. Full suite re-run after the fixes: 105 tests green, clippy clean.

## Answer

实现完成，纯重构。非流式（`relay_non_streaming`）与流式（`relay_streaming`）中继路径逐函数重复的"预提交 fallback 循环"已抽取为单一共享 helper `relay_precommit_fallback_loop`（`src/server.rs`），两处成功路径作为闭包注入；对外行为完全不变，全套现有测试（78 + 27 = 105）通过、clippy 零警告。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **共享 helper `relay_precommit_fallback_loop`**：拥有整个预提交尝试循环——按原有序候选集逐条 `begin_attempt`、替换 `request["model"]` 为上游模型名、发送请求、状态三分类（可归因失败 → 隔离 + 转交下一条；健康中立 4xx → 结束调用，ROUTE-009；成功 → 交给 `on_success`）、attempt 簿记与 `quarantine_route`、候选耗尽后 `finalize(false)` + 写入调用记录 + 返回最终安全错误。原 `relay_non_streaming` 与 `relay_streaming` 的重复实现全部删除。
- **成功路径闭包**：发送成功后的差异逻辑留在两个路径各自的闭包里——非流式读取并验证完整响应（body 读取 / JSON 解析 / `validate_complete_response` / `is_semantic_failure` 任一失败均以 `PreCommitOutcome::Fallthrough` 交还共享循环完成隔离与转交，恢复发布模型身份后 `Committed`）；流式校验 SSE content-type、`SseRelay::prime` 首个协议事件（失败同样 Fallthrough），成功后 `mark_committed` 并构造下游流式响应（`Committed`）。失败分支统一经构造器 `precommit_fallthrough(recorder, failure)` 表达。闭包接收 `CallRecorder` 所有权以便流式响应把 recorder 随 unfold 状态携带到流结束（post-commit 失败仍在流内按 ROUTE-014/015 终止，不重试不拼接）。
- **语义保持的关键点**：预提交失败 → 隔离当前模型路由 → 按原有序候选集转交；健康中立 4xx 不 Fallback；首事件已提交后的失败只终止下游流、隔离已提交路由，绝不拼接或重试；候选耗尽返回最终错误（`all_upstream_attempts_failed`）；调用记录与 attempt 链（OPS-001/OPS-003）逐字段与原实现一致（http_status、failure_category、commit_phase、outcome、usage、completion/first-token 计时）。
- **验证**：`cargo check` 通过、clippy 零警告；`secure_management_surface` 78 个（含 `streaming_precommit_failures_fallback_to_the_next_candidate_without_leaking_bytes`、`non_streaming_candidate_exhaustion_quarantines_all_routes_and_returns_the_final_error` 等 Fallback/隔离/耗尽用例）与 `packaging_lifecycle` 27 个全绿，完整 `cargo test` 复跑 105 个通过。
