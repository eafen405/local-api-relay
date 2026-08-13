# 17 — 签发中转访问密钥并完成首次 Chat 调用

**What to build:** 让管理员创建一个只显示一次完整秘密、只获准使用已配置模型路由的中转访问密钥；客户端随后可通过标准模型列表发现发布模型，并完成一次透明的非流式 Chat Completions 调用，而不会接触上游凭据。

**Blocked by:** 16 — 配置第一条可检测模型路由

**Status:** resolved

- [x] 中转访问密钥创建时要求至少一条有效路由资格，完整秘密只显示一次，持久化仅包含前缀、哈希、标签、时间和资格关联。
- [x] 管理员可搜索密钥、查看非秘密范围并经确认撤销；未认证、错误或已撤销 Bearer 密钥不能调用 `/v1/*`，中转密钥不能访问管理面。
- [x] `/v1/models` 返回调用密钥当前可调用发布模型的标准 `object=list` / `data[]` 结构和确定性顺序。
- [x] 非流式 Chat 请求验证非空 `model` 与 `messages` 数组，`stream` 缺省为 false；非法 JSON、未知模型和非法字段在接触上游前失败。
- [x] 转发只解析路由所需字段，保留未知请求字段，以显式上游模型名和上游 API key 调用脚本上游，并在完整合法响应中恢复发布模型名和保留未知响应字段。
- [x] 本地与不安全上游错误使用有界 OpenAI 风格 envelope，并遵守 `502`、`503`、`504` 网关语义而不泄露秘密或原始错误正文。
- [x] 真实进程测试覆盖密钥一次显示/哈希、资格、撤销、模型列表、成功 Chat、请求 pass-through、响应 pass-through 和错误边界。

Spec coverage: `API-001`–`API-004`, `API-006`–`API-009`, `API-013`–`API-016`, `SEC-002`, `SEC-006`, `SEC-008`, `CFG-009`, `CFG-011`–`CFG-012`, `UI-009`.

## Comments

- 2026-08-10: Implementation started. Progress and verification results will be recorded here; the ticket Markdown is the work tracker because repository Git metadata is unavailable in this workspace.
- 2026-08-10: Implemented and verified the relay-key workflow. `cargo check`, direct-toolchain `cargo-clippy --all-targets -- -D warnings`, Rust format check, `node --check src/web/app.js`, and `cargo test` pass (9 real-process tests). The suite covers eligibility validation, one-time secret return and persistence scan, search/listing, authorization separation, revocation, deterministic models, default non-streaming Chat forwarding, request/response pass-through, invalid request bodies, no available route, and unsafe upstream response normalization.
- 2026-08-10: Fixed-point `code-review` and the requested commit cannot run because `.git` is an empty read-only mount: `git rev-parse HEAD` fails. A local Standards/Spec review against this ticket and `spec.md` found no remaining issues. The local Markdown tracker is the submission record for this workspace.

## Answer

Implemented relay access keys and the first non-streaming Chat Completions call. Administrators can create keys with one or more configured model-route eligibilities, search their non-secret metadata and route scopes, and revoke them with confirmation. SQLite stores only the key prefix and SHA-256 digest alongside metadata and eligibility records; full secrets are returned only from their creation response.

`GET /v1/models` now returns each authenticated key's currently available Chat published models in deterministic OpenAI list form. `POST /v1/chat/completions` validates its JSON boundary, injects `stream: false` when omitted, substitutes only the selected route's upstream model and credentials, preserves remaining request and response fields, restores the published model name, and normalizes unsafe/local failures without exposing secrets. The Operations UI adds key creation, scope selection, search, one-time display, and confirmed revocation.
