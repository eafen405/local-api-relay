# 23 — 记录调用与模型路由尝试链

**What to build:** 让管理员在 Calls & usage 中看到每次客户端调用的一条 metadata-only 记录，并在发生 Fallback 或异常时原位展开模型路由尝试链，从而理解哪个上游最终成功、何时转交以及失败发生在提交前还是提交后。

**Blocked by:** 21 — 隔离模型路由并执行提交前 Fallback

**Status:** resolved

- [x] 每个下游调用最多保存一条调用记录；一次或多次 Fallback 不创建独立调用记录，全部候选失败仍保留一条失败记录。
- [x] 成功行展示调用时间、发布模型、最终成功上游供应商、成功尝试报告的 token/缓存 token、估算费用占位、完成耗时及流式首字耗时。
- [x] 展开的尝试链只含顺序、安全本地路由/供应商标识、开始时间、耗时、安全 HTTP 状态、规范化错误类别、提交阶段和 Fallback/终止结果。
- [x] 只有最终成功尝试的可靠 usage 可进入调用记录；失败尝试不计入且不估算，全失败的 token、费用和耗时显示 `-` 并排除聚合。
- [x] Calls & usage 使用可分页高密度表和原位详情，发布模型为主标签、成功上游为次标签，不提供请求、响应或原始错误入口。
- [x] 持久化与 API/UI 响应使用 metadata allowlist，不保存正文、prompt、tool 参数、原始 header、query、Authorization 或原始上游错误。
- [x] 进程和浏览器测试覆盖直接成功、多次 Fallback、全失败、流提交后失败及 canary 字段不进入记录或页面。

## Answer

实现（server/store/UI 数据链路）此前已完成：`CallRecorder`（server.rs:117）、尝试链驱动（relay_non_streaming / relay_streaming）、`extract_usage`、`/admin/calls-usage` 分页 API、`call_records`/`call_attempts` 表（schema v6）。本 ticket 收尾补上验收点 7 的进程边界测试，全部通过。

新增测试（tests/secure_management_surface.rs）：
- `successful_chat_call_records_usage_attribution_and_completion_time` — OPS-001/002/004：单条成功记录含 usage 归属、成功供应商、completion_ms、首字耗时（非流式 null）、费用占位 null。
- `fallback_attempts_form_an_ordered_chain_with_normalized_failures` — OPS-001/003：一次调用一条记录，尝试链顺序、`upstream_http_5xx` 规范化类别、pre_commit→committed、fallback→success，usage 只来自成功尝试。
- `exhausted_candidates_record_one_failed_call_with_unknown_values` — OPS-005：全失败保留一条记录，succeeded=false、无成功供应商、token/费用/耗时全部 null（未知而非零）。
- `stream_terminated_after_commit_records_one_attempt_without_usage` — OPS-003/004 + ROUTE-014：提交后流终止记为 committed + `stream_terminated` + `invalid_upstream_response`，无 usage。
- `call_records_page_paginates_and_clamps_page_size` — UI-010：分页正确、page_size 上下限 clamp（1..100）。
- `canary_fields_never_enter_call_records_or_attempts` — OPS-020/021：prompt、tool 参数、header、原始上游错误正文的 canary 不出现在 calls-usage API 与 sqlite 字节中，失败类别保持规范化。

验收证据：`cargo test --test secure_management_surface` 49 通过 0 失败（43 存量 + 6 新增）。

## Comments

### code-review follow-up（2026-08-10）

Standards/Spec 双轴 review 发现并修复：流式 Responses 以 `response.failed`（或 status failed/cancelled、error 非空）终止时，此前被记为 succeeded=true 且 usage 入账（违反 OPS-004/API-011/ROUTE-008）。现在 `SseRelay` 检测语义失败终止事件，记录为 `stream_terminated`/`upstream_semantic_failure`、succeeded=false、无 usage，并隔离路由。新增测试 `streaming_responses_semantic_failure_is_recorded_failed_and_quarantines`；既有 `relay_uses_failed_and_incomplete_responses_events_as_native_terminators` 改为每场景独立路由/密钥（语义失败与非法终止现在都会隔离各自路由）。
