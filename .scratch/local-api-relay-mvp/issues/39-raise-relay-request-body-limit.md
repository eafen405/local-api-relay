# 39 — 放宽中继请求体上限

**What to build:** 中继目前对请求体设有一个无出处且偏小的上限（16 KiB），真实 harness 的多轮对话/工具调用负载会超过它而被 413 拒绝。本 ticket 把上限放宽到一个有出处、能覆盖真实负载的合理值，让这类合法请求端到端成功；请求体超限的明确错误仍按 API-016 处理。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 超过 16 KiB 的合法 Chat Completions / Responses 请求端到端成功，不再被 413 拒绝。
- [x] 新上限取值有明确出处（spec 或 README 中记录），不再是隐式的任意小值。
- [x] 超过新上限的请求仍立即返回明确错误且不触发 Fallback、不影响路由健康（API-016）。
- [x] 新增测试覆盖大请求通过与超限请求失败两条路径；全套现有测试保持绿色。

Spec coverage: `API-006`, `API-016`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. 请求体上限从无出处的 `16 * 1024` 字面量放宽为命名常量 `MAX_RELAY_REQUEST_BODY_BYTES = 1024 * 1024`（1 MiB，64 倍），`src/server.rs` 路由层 `.layer(DefaultBodyLimit::max(...))` 使用该常量；取值出处记录于 README "Relay Calls" 节（"Inbound request bodies are limited to 1 MiB … rejected immediately with 413 and never reaches upstream routing (API-016)"）。覆盖真实 harness 多轮对话/工具调用负载（百 KB 量级）；对 loopback 单用户中转 1 MiB 全量读入无内存压力。413 语义不变：`chat_completions`/`responses` 处理器对 `BytesRejection` 的 `RelayError` 规范化映射（`PAYLOAD_TOO_LARGE` + `{"error":{"message":"request body is too large","type":"invalid_request_error","param":null,"code":null}}`）未改动，认证先于体积检查（既有顺序），超限不解析、不选路、不探测、不改健康（API-016）。`DefaultBodyLimit` 是 Router 层设置，admin API 一并受益（其 payload 本就远小于 1 MiB，行为不变）。测试：新增 `relay_accepts_request_bodies_larger_than_16_kib_and_rejects_oversized_ones`（TDD red→green：16 KiB 上限下 128 KiB chat 请求实测 413 → 放宽后 200）——128 KiB Chat Completions 消息端到端 200 且上游捕获转发体长度保持 128 KiB（API-006 保留）、128 KiB Responses input 文本端到端 200 且上游转发体保持、超限请求（≈1 MiB + 1 KiB）仍 413 + 既有错误体 + 上游未调用 + 路由健康 available；两处既有超限 payload（17 KiB / 20 KiB）更新为 `1024 * 1024 + 1024` 字节以继续断言超限语义（否则新上限下它们变为合法请求）。全量验证：`cargo check --all-targets` 通过、clippy 零警告、`cargo test` cargo exit 0（secure_management_surface 81 + packaging_lifecycle 全绿，含新增 1 个）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket39-change-record.md`) completed. **Standards 轴**：通过——无文档化标准违规（repo 无编码标准文件，clippy/rustfmt 由工具强制跳过）；baseline smells 均为 judgement call 且无实质命中，四条建议按取舍处理：超限魔法数字三处重复 → 采纳，新增测试常量 `OVER_LIMIT_BODY_CHARS = 1024 * 1024 + 1024` 统一；新测试的 413 尾部与既有 "Body limit client" 场景重复 → 保留（新测试需在同一测试内覆盖两条路径，且 payload 已统一为常量）；测试名含 `16_kib` → 保留（与 ticket 验收措辞 AC1 直接对应）；const 注释与 README 逐字重复 → 采纳，注释精简为引用 README 节。**Spec 轴**：四项 checklist 全部落实、无越界（`DefaultBodyLimit` 全局生效使 `/admin/*` 同步放宽至 1 MiB，为有记录的设计决策且行为不变）、无 implemented-but-wrong；一处变更记录夸大已修正（revocation 测试的 probe 上游 worker 在超限请求前已 join，实际只断言 413 + 错误体，"不触达上游"由新测试与 health-neutral 测试覆盖）。修复后 3 个受影响测试单独全绿 + clippy 零警告。

## Answer

实现完成。中继请求体上限从 16 KiB 放宽到有出处的 1 MiB，真实 harness 多轮对话/工具调用负载不再被 413 拒绝；超过新上限的请求仍按 API-016 立即 413 且不触达上游路由/健康。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **取值与出处**：`src/server.rs` 新增常量 `MAX_RELAY_REQUEST_BODY_BYTES: usize = 1024 * 1024`，路由层 `DefaultBodyLimit::max(MAX_RELAY_REQUEST_BODY_BYTES)`；README "Relay Calls" 节记录 1 MiB 上限及其 API-016 语义。覆盖多轮/工具调用文本负载，超限路径行为与错误体完全不变。
- **语义**：认证先于体积检查（handler 先 `require_relay_access_key` 再映射 `BytesRejection`）；超限 413 + 既有 OpenAI 风格错误体；`DefaultBodyLimit` 为 Router 层设置，admin API 一并受益（payload 远小于 1 MiB，无行为变化）。
- **测试**：新增 `relay_accepts_request_bodies_larger_than_16_kib_and_rejects_oversized_ones`（128 KiB chat + responses 端到端成功且上游转发体完整保留；超限 ≈1 MiB + 1 KiB 仍 413 + 无上游调用 + 健康不变）；两处既有超限断言（`relay_access_keys_reject_invalid_calls_and_stop_working_after_revocation`、`health_neutral_failures_do_not_quarantine_the_route_or_start_a_fallback`）payload 更新为 `1024 * 1024 + 1024` 字节。
- **验证**：`cargo check --all-targets` 通过、clippy 零警告；`cargo test` cargo exit 0——secure_management_surface 81 个全绿（22.40s，含新增 1 个），packaging_lifecycle 全绿，全套通过。双轴 code-review 完成（结果见 Comments）。
