# 19 — 支持原生 Responses 调用

**What to build:** 让管理员配置协议独立的 Responses 模型路由，并让客户端以原生 Responses 请求和对象完成非流式调用。Responses 必须与 Chat Completions 保持独立映射和健康身份，透明保留扩展字段并识别 HTTP 2xx 中的语义失败。

**Blocked by:** 17 — 签发中转访问密钥并完成首次 Chat 调用

**Status:** resolved

- [x] 管理流程允许在同一发布模型和上游模型下创建独立 `responses` 路由，并明确展示协议与独立健康状态。
- [x] `/v1/responses` 验证非空 `model` 和存在的 `input`，`stream` 缺省为 false，并原样保留 instructions、tools、reasoning、metadata 和未知字段。
- [x] 上游只收到显式 Responses 模型名与上游凭据，客户端收到完整 Response 对象和恢复后的发布模型名，不发生 Chat/Responses 转换。
- [x] HTTP 2xx 但状态为 failed/cancelled 或 error 非空的 Response 被识别为上游语义失败，而非健康成功。
- [x] Chat 与 Responses 路由可以独立 Available 或暂不可用；一个协议的健康变化不影响另一个协议。
- [x] 真实进程测试覆盖最小有效/无效 Responses、未知字段、完整响应、语义失败、协议隔离和密钥资格。

Spec coverage: `API-005`–`API-008`, `API-011`, `API-018`, `CFG-006`–`CFG-008`, `ROUTE-001`, `ROUTE-003`.

## Comments

- 2026-08-10: Implementation started. The approved test seam is the real relay process at its loopback HTTP boundary, as specified in the MVP Testing Decisions and used by tickets 17 and 18.
- 2026-08-10: Implemented and verified native non-streaming Responses calls. `cargo check`, `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`, `node --check src/web/app.js`, and `cargo test --all-targets` pass; the final real-process suite contains 15 passing tests.
- 2026-08-10: The required `code-review` fixed-point comparison and commit cannot run because `.git` is an empty read-only mount: `git rev-parse HEAD` fails. A local Standards/Spec review of the ticket-relevant server, store, Web and process-boundary test changes found no actionable findings. The local Markdown tracker is the completion record.

## Answer

Implemented `POST /v1/responses` as a native non-streaming relay path. It authenticates the relay access key, validates `model`, `input` and `stream`, selects only eligible Available `responses` model routes, substitutes the configured upstream model and credential, and restores the published model name on the complete upstream Response object. Requests and responses preserve extensions without Chat/Responses conversion.

Responses routes now participate in model discovery and remain independently selectable and healthy from Chat Completions routes. Response objects with `failed` or `cancelled` status, or a non-null `error`, are normalized to a safe semantic upstream failure; native checks likewise do not mark such routes Available. The Operations onboarding copy now describes a native protocol mapping, while the existing route table continues to show protocol and per-route health.
