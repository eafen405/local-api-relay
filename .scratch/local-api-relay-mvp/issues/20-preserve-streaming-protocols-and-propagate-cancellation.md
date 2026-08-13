# 20 — 保持流式协议并传播取消

**What to build:** 让 Chat Completions 与 Responses 客户端接收原生、顺序正确的 SSE 流；本地中转在提交前验证首个协议事件，提交后绝不拼接另一生成，并在下游断开时及时取消上游且不改变健康。

**Blocked by:** 17 — 签发中转访问密钥并完成首次 Chat 调用; 19 — 支持原生 Responses 调用

**Status:** resolved

- [x] Chat streaming 返回 `text/event-stream`，无损保持合法 chunk 顺序与 `[DONE]`；首个已验证 chunk 在提交后准确重放一次。
- [x] Responses streaming 保持具名事件及顺序，以类型化 completed/failed/incomplete 事件作为终止判据，不要求 `[DONE]` 且不转换成 Chat chunk。
- [x] 首事件前连接、读取、协议或语义失败不向下游泄露 header/body；单路由场景返回可替换的规范化失败。
- [x] 一旦任意流字节提交，后续失败只终止当前下游流，不生成第二段响应、不改变协议，也不在同一次调用内重试。
- [x] 下游取消或断开及时终止活动上游，不开始新尝试，并保持模型路由健康中性。
- [x] 进程边界测试使用可控 SSE 上游覆盖 chunk/event 顺序、终止、首事件失败、提交后截断、空闲超时和真实客户端取消。

Spec coverage: `API-010`, `API-012`, `API-017`–`API-018`, `ROUTE-009`, `ROUTE-012`–`ROUTE-015`.

## Comments

- 2026-08-10: Implementation started. The approved test seam is the actual local relay process at its loopback HTTP boundary, using a controllable SSE upstream, as prescribed by the MVP Testing Decisions and established by tickets 17 and 19.
- 2026-08-10: Implemented primed native SSE relay paths for Chat Completions and Responses. The relay validates the first protocol event before committing, preserves event order and native terminators, restores the published model identity in Chat chunks, stops rather than rewrites after committed upstream failures, applies a 5-second per-event idle timeout, and drops the active upstream response when the downstream socket closes. A separate streaming client prevents the 15-second response-head deadline from truncating active streams.
- 2026-08-10: Process-boundary coverage passed for Chat ordering, mixed LF/CRLF framing, published-model mapping, Responses named events and all three typed terminators, invalid first events, committed truncation, first-event idle timeout, real socket cancellation, and an 18-second active stream. `rustfmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `node --check src/web/app.js` pass.
- 2026-08-10: Standards/Spec review surfaced and resolved the whole-stream deadline, repeated send-policy, mixed framing, typed Responses terminal, native Responses request-capture, and arbitrary 64 KiB SSE-buffer issues. Route isolation and fallback findings are intentionally deferred to blocking successor ticket 21, which owns `ROUTE-006`–`ROUTE-015` state transitions.
- 2026-08-10: Final execution of the newly added >64 KiB process-boundary regression and the post-fix full suite is pending loopback permission: two `cargo test` escalation attempts were rejected because automatic approval review timed out before process launch. Git fixed-point review and the required commit are also unavailable because `.git` is an empty read-only mount and `git rev-parse HEAD` fails.
- 2026-08-10: Final verification complete. `relay_preserves_a_chat_sse_event_larger_than_64_kib` passes at the real process boundary, and the full suite passes with 25 tests. `cargo fmt -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `node --check src/web/app.js` all pass on the final tree. The commit cannot be produced because `.git` is an empty read-only mount (`git rev-parse HEAD` fails); the local Markdown tracker is the completion record, per the commit constraint in the handoff.

## Answer

Implemented primed native streaming relay paths for Chat Completions and Responses SSE. The relay reads and validates the first protocol event before committing any downstream header or body, then forwards ordered native events without rewriting. Chat streams preserve legal chunk order and `[DONE]`, restore the published model identity in streamed chunks, and replay the first verified chunk exactly once. Responses streams preserve named events and order, using typed `completed`/`failed`/`incomplete` terminal events as the termination criterion without requiring `[DONE]` or converting to Chat chunks.

Failures before the first event (connection, read, protocol, or semantic) leak no header/body downstream and return a replaceable normalized failure in the single-route scenario. Once any stream byte is committed, later failures only terminate the current downstream stream — no second generation is appended, protocol is unchanged, and no retry occurs within the same call. Downstream cancellation or disconnection terminates the active upstream promptly without starting a new attempt and keeps model-route health neutral.

Streaming uses a dedicated Reqwest client with no total body deadline, retaining a 15-second response-head deadline and a five-second per-event idle timeout; committed streams stop on post-commit read/protocol failures, and body drop cancels the upstream when the downstream socket closes. Process-boundary coverage uses a controllable SSE upstream and covers event order, native and mixed LF/CRLF framing, typed terminations, invalid first events, committed truncation, idle timeout, long-lived active streams, real client cancellation, published-model mapping, and a single event larger than 64 KiB.
