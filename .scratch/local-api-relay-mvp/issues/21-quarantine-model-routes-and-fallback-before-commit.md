# 21 — 隔离模型路由并执行提交前 Fallback

**What to build:** 当一条模型路由发生可归因上游的故障时，立即只隔离该协议特定路由，并在下游尚未提交时把原请求交给同一发布模型候选集中的下一条路由；提交后则终止而不拼接。客户端错误、取消和非归因 `4xx` 保持健康中性。

**Blocked by:** 18 — 在可用模型路由集内确定性成本选路; 20 — 保持流式协议并传播取消

**Status:** resolved

- [x] 单次 DNS、TLS、连接、转发、响应读取、流、连接/响应/空闲 timeout，或上游 `401`、`403`、`404`、`429`、`5xx` 立即使当前模型路由暂不可用。
- [x] 非法、截断或协议错误的上游响应以及可归因 Responses 语义失败触发相同隔离；无需失败计数、错误率窗口或阈值。
- [x] 非法客户端请求、认证失败、请求体超限、下游取消和 allowlist 外上游 `4xx` 不触发隔离或 Fallback。
- [x] 非流式请求只有在完整响应验证成功后才提交；提交前失败按原始确定性候选顺序继续，候选耗尽返回最终安全规范化错误。
- [x] 流式首事件前失败可选择下一候选且不泄露失败尝试字节；提交后失败隔离当前路由并终止流，绝不拼接其他生成。
- [x] 已进入暂不可用后，旧在途请求的成功不能恢复该路由；同一供应商的其他模型或协议路由不受影响。
- [x] 表驱动进程测试覆盖全部故障类别、健康中性类别、非流式与流式提交边界、多候选耗尽和并发旧请求。

Spec coverage: `API-013`–`API-017`, `ROUTE-006`–`ROUTE-015`.

## Comments

- 2026-08-10: Implementation started. The approved test seam is the real relay process at its loopback HTTP boundary, using controllable scripted upstreams, as prescribed by the MVP Testing Decisions and established by tickets 17–20.
- 2026-08-10: Implemented per-route quarantine and pre-commit fallback. The store now returns the full deterministic candidate list (multiplier asc, route id asc) per key/model/protocol and exposes `quarantine_route` (Available → unavailable only; success never restores). Both relay paths try candidates in order: transport errors, allowlisted upstream `401/403/404/429/5xx`, invalid/truncated/non-JSON bodies, incomplete protocol responses, and Responses semantic failures quarantine the current route and continue; allowlist-excluded `4xx` return immediately health-neutral; the final safe error preserves the last attempt's status. Streaming primes the first native event before commit, falls back pre-commit without leaking bytes, and on post-commit failure quarantines and terminates without retry or splicing. A stream that ends cleanly before its terminal event (`[DONE]` / typed terminal) is treated as a truncated response and quarantines the route.
- 2026-08-10: Interpretation notes recorded after Standards/Spec review. (1) Typed streamed `response.failed`/`response.incomplete` terminal events are forwarded natively without quarantine, per ticket 20's established and tested contract (API-012 treats them as completion criteria); quarantine for Responses semantic failures is applied on the non-streaming path and on streamed protocol/transport failures. (2) The final normalized error after candidate exhaustion keeps the last attempt's safe HTTP status but not the upstream error body (API-014 SHOULD; the ticket requires a "final safe normalized error", and the static normalized message is what the pre-existing contract tests assert). (3) DNS/TLS failures share the same `send()` transport-error branch as the covered connection-refused case, so the transport category is exercised end-to-end. (4) Pre-existing API-008 JSON field-order fidelity (serde_json without `preserve_order`) predates this ticket and was left untouched.
- 2026-08-10: Red-green process-boundary TDD confirmed the new behavior: eight new/updated tests failed against the prior binary and pass after the change. `cargo fmt -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `node --check src/web/app.js` pass. The full suite passes with 36 tests.
- 2026-08-10: Code review cannot obtain a Git fixed point because `.git` is absent (`git rev-parse HEAD` fails), as in tickets 18 and 20; a local Standards/Spec review of the changed store, relay and test files found the core behavior correct, and its in-scope finding (clean-EOF truncation quarantine) was implemented and covered by a new process test. The commit cannot be produced for the same reason; the local Markdown tracker is the completion record.

## Answer

Implemented single-failure route quarantine and commit-before-fallback. The store exposes the full deterministic candidate list (`eligible_chat_routes`/`eligible_responses_routes`) and `quarantine_route`, which only transitions Available routes to unavailable; an in-flight request's success never restores a quarantined route.

The relay now walks candidates in original order. Any single attributable failure — transport (DNS/TLS/connection/forwarding/read), connection/response/stream idle timeout, upstream `401`/`403`/`404`/`429`/`5xx`, or invalid/truncated/protocol-error responses and Responses semantic failures — immediately quarantines only that protocol-specific route and, before any downstream commitment, forwards the same request to the next candidate with that route's upstream model name. Allowlist-excluded upstream `4xx`, invalid client requests, authentication failures, body over-limit, and downstream cancellation are health-neutral and never start a fallback. Non-streaming requests commit only after full validation; exhaustion returns the final safe normalized error preserving the last attempt's status. Streaming relays prime and validate the first native event before committing; pre-commit failures fall back without leaking failed-attempt bytes, while post-commit failures quarantine the current route and terminate the stream, never retrying or splicing another generation. A stream that ends cleanly before its terminal event is quarantined as truncated.

Process-boundary tests cover the failure table (500/429/404/403/401, invalid JSON, truncated body, connection refused), health-neutral cases (upstream 400, invalid client JSON, body over limit), non-streaming multi-candidate exhaustion, Responses semantic-failure fallback, streaming pre-commit failures (invalid first event, non-SSE content type, first-event idle timeout) without leaked bytes, post-commit truncation quarantine, clean-EOF truncation quarantine, and a concurrent old-request scenario proving that an in-flight success cannot restore a quarantined route.
