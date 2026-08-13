# 48 — 配置验证矩阵测试（CFG-006/007/008/010）

**What to build:** 验收矩阵 CFG-006–CFG-010 要求配置验证矩阵覆盖空模型名、非法协议、非正倍率、重复路由、双协议独立健康、重复资格和禁止手改健康。当前只有非正倍率有直接测试，其余验证路径无进程边界测试背书。本 ticket 为这些校验路径补齐表驱动进程边界测试，断言非法配置被拒绝、不产生可调用路由，且错误信息可操作。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] 空/空白上游模型名被拒绝，且不产生可调用路由。
- [ ] 非法协议值被拒绝（只接受 Chat Completions / Responses 原生协议）。
- [ ] 同一发布模型+供应商+协议+上游模型名重复路由被拒绝。
- [ ] 双协议独立健康：一条协议的失败不影响另一条协议同发布模型路由。
- [ ] 重复的路由资格被拒绝；无有效资格密钥不可调用。
- [ ] 管理面无法手改系统所有的健康状态（只能通过检测/恢复通道变化）。
- [ ] 全套现有测试保持绿。

Spec coverage: `CFG-006`–`CFG-010`.
## Answer

CFG-006–CFG-010 配置验证矩阵已补齐表驱动进程边界测试（`tests/secure_management_surface.rs`，4 个新测试），全部断言非法配置被拒绝、不产生可调用路由、错误信息可操作；同时修复了测试暴露的 create 重复路由错误信息不可操作问题（详见 `/tmp/48-change-record.md`）。

清单逐项结论：

- [x] **空/空白上游模型名被拒绝，且不产生可调用路由**：`config_validation_matrix_rejects_invalid_routes_without_a_callable_route` 表驱动覆盖 `""` 与 `"   "` → 422 + `error.fields.upstream_model_name`「upstream model name must be between 1 and 256 characters」；随后断言 operations 路由表为空、无资格密钥创建被拒；同一供应商仍可创建合法路由并真实调用 200。
- [x] **非法协议值被拒绝（只接受 Chat Completions / Responses）**：同一矩阵测试 `"not-a-protocol"` 行 → 422 + `error.fields.protocol`「protocol must be chat_completions or responses」。
- [x] **重复路由被拒绝**：`duplicate_model_route_identity_is_rejected_with_an_actionable_message` — create 相同身份 → 422「a model route with this published model, provider, upstream model, and protocol already exists」（先红：原来返回原始 UNIQUE constraint 消息，已修复）；协议维度使同模型/供应商/上游名的不同协议路由合法共存（CFG-007）；update 冲突编辑 → 422 且原路由不变。
- [x] **双协议独立健康**：由既有进程边界测试 `responses_semantic_failures_do_not_make_chat_routes_unavailable` 背书（responses 语义失败只隔离 responses 路由，同发布模型 chat 路由保持 available 且可调用），本次未重复。
- [x] **重复的路由资格被拒绝；无有效资格密钥不可调用**：`duplicate_route_eligibility_is_rejected_and_keys_require_valid_eligibility` — `[A, A]` → 422「eligible model routes must not contain duplicates」；`["missing-route"]` → 422「does not exist」；`[]` → 422「at least one eligible model route」；合法 `[A, B]` 是唯一被创建的密钥并可调用 `/v1/models`。
- [x] **管理面无法手改系统所有的健康状态**：`admin_cannot_directly_edit_system_owned_route_health` — create/编辑载荷夹带 `health` 字段均被忽略，健康只由探测/系统所有权重检决定（探测失败→unavailable、成功→available；health-neutral 编辑不改变健康）。
- [x] **全套现有测试保持绿**：`cargo test` exit 0，packaging 29 + secure 92 = **121 个测试全绿**；`cargo clippy --all-targets` 零警告。

生产代码改动（1 处，review 阶段再重构）：`src/store.rs` 新增共享辅助 `route_identity_conflict` + 常量 `DUPLICATE_ROUTE_IDENTITY_MESSAGE`，`create_model_route` 与 `update_model_route` 共用同一身份唯一性预检与同一可操作消息（此前 create 路径依赖 schema UNIQUE 约束，返回不可操作的原始 SQLite 错误）。

Spec coverage：`CFG-006`–`CFG-010`。

## Comments

- TDD red→green：先写 `duplicate_model_route_identity…` 测试，红态观察到 create 重复返回 `"UNIQUE constraint failed: model_routes.published_model_id, …"`（非可操作消息），再做最小生产修复转绿；其余矩阵行验证路径已存在，直接绿。
- 双轴 review（Standards/Spec 并行子代理，基于 `/tmp/48-change-record.md` + 实际文件）：无硬性违规、无缺失需求。修复项——store.rs 身份检查提取共享辅助（消除 create/update 双份查询与消息拷贝）；`assert_route_rejected` 改收整体 payload（消除 9 参 data clump，并移除 `#[allow(clippy::too_many_arguments)]`）。保留项——两个测试内局部 `create_route` 辅助（作用域内、返回类型不同，抽文件级辅助会引入新参数团）；bail 消息沿用既有 "provider" 措辞（现由共享常量保证两路径一致；CONTEXT.md 的 Avoid 适用范围是 agent 输出命名）。
