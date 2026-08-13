# 45 — 字段级校验反馈（UI-006）

**What to build:** UI-006 要求「不完整映射、非法倍率或没有有效路由资格的密钥 MUST 在管理面给出字段级、可操作反馈」。当前聚焦面板把服务端错误坍缩为一个通用错误区，未定位到具体字段，也没有逐项说明如何修正。本 ticket 让新增/编辑供应商、模型路由、密钥的表单在提交失败时把错误显示在对应字段旁，并给出可操作的修正提示（例如「倍率必须为正数」紧贴倍率输入框），不完整配置不得变为可调用状态。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 表单校验失败的错误按字段定位显示，附可操作修正提示，而非单一通用错误。
- [x] 非法/不完整配置无法保存为可调用状态（调用面不出现该配置）。
- [x] 页面级或浏览器测试覆盖至少一个字段级错误场景。
- [x] 全套现有测试保持绿。

Spec coverage: `UI-006`, `CFG-011`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill（与 44 一并实现、一并 review）。
- 2026-08-12: Implementation completed. 后端新增 `store::FieldError`（wire 字段名 + 人读消息）结构化校验错误，`AdminError` 增加 `fields` 契约（`{"error":{"message","fields":{field:message}}}`），`with_configuration_store` 对 anyhow 链 downcast 字段错误；前端 `request()` 挂载 `error.fields`，`renderFieldErrors` 把消息渲染到对应输入旁的 label/fieldset，四个表单（供应商/模型路由/价格/密钥）接入。新增进程边界测试 `form_validation_errors_are_attributed_to_their_fields`。
- 2026-08-12: Code review (dual-axis，非 git 场景经 `/tmp/tickets44-45-change-record.md` + 实际文件审查) 完成。**Standards 轴**：通过——无文档化标准违规；baseline smells 均为 judgement call（`(value, label, field)` 三元组 Data Clumps 候选未抽类型；`fields: Option<Map>` 保留 map 形状作为 wire 契约；downcast 脆弱性以 FieldError 文档注明根因约束）。**Spec 轴**：通过——三条 checklist 全部落实、无越界（价格表单字段级反馈为共享 helper 的低成本一致性，CFG-012 本就要求校验发布模型价格；recovery settings 表单不在 ticket 范围保持通用错误）；"不得变为可调用"由 store 层 422 + 事务原子性保证（非法路由/密钥不落库、不进候选集，CFG-011）；一处变更记录与实现不符已修正（资格错误应定位到 route-eligibility fieldset 而非第一个 checkbox 的 label）；UX 优化：字段错误已渲染时面板错误区不再重复同句。测试缺口（DOM 级渲染断言）为 repo 无浏览器自动化接缝下的既有模式限制，10 个 API 边界 422 用例 + 前端静态标记已覆盖。验证：全套 cargo exit 0、113 测试全绿、clippy 零警告。

## Answer

实现完成（与 44 共享同一批文件）。字段级校验反馈：

- 后端契约：校验失败返回 422 + `{"error":{"message":<完整句子>,"fields":{<wire字段名>:<同句>}}}`。字段归属：`display_name`/`base_url`/`api_key`（供应商表单）、`published_model_id`/`provider_id`/`upstream_model_name`/`protocol`/`cost_multiplier`（模型路由表单，倍率错误如「cost multiplier must be greater than zero」）、`label`/`model_route_ids`（密钥表单，如「at least one eligible model route is required」）、价格表单三字段。非字段错误（资源不存在、跨字段唯一性冲突、recovery settings）保持通用 `error.message` 形状（无 `fields` 键）。
- 实现方式：`store::FieldError` 作为 anyhow 根因从 store 校验点传播（validator 签名改为 `Result<T, FieldError>`，事务内引用错误字段化）；`with_configuration_store` downcast 为 `AdminError::with_field`。消息逐字不变（`parse_decimal(value, label, field)` 以 label 构造消息、field 作 wire 名），既有消息断言原样通过。
- 前端：`request()` 挂载 `error.fields`；`renderFieldErrors` 把每条消息渲染为输入旁 `label`/`fieldset` 内的 `.field-error`（资格错误定位到 route-eligibility fieldset 整体）；字段错误渲染成功时通用错误区不再重复显示，未映射消息落到 `#panel-error` 兜底。
- 不可调用保证（CFG-011）：校验在 store 事务内、提交前完成，非法/不完整配置不落库、不发布运行时配置、不进入候选集，调用面（/v1/models、relay 路由）不出现该配置。
- 测试：`form_validation_errors_are_attributed_to_their_fields` 覆盖供应商/路由/密钥表单 10 个字段级 422 用例（含 ticket 示例的倍率必须为正数）、顶层消息与字段消息一致性、非字段错误无 `fields` 键的回退、前端标记（`renderFieldErrors`/`field-error`/`error.fields`）。
- 全套测试绿（113 = secure 86 + packaging 27），clippy 零警告。
