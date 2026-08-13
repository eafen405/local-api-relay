# 41 — 界定并执行 Base URL 渲染边界（OPS-020）

**What to build:** OPS-020 的 allowlist 目前按字面禁止渲染完整 Base URL，但管理编辑面板需要完整值才能编辑供应商，形成矛盾。本 ticket 修订 OPS-020 措辞，把 allowlist 明确界定到诊断面（调用记录、运维事件、日志、状态区），声明管理聚焦编辑面板是保留完整 Base URL 的唯一例外；管理**列表/只读**表面只显示遮蔽/截断形式，并补 canary 断言防止诊断面泄漏。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 修订 spec 中 OPS-020 的措辞：allowlist 明确界定到诊断面（记录/事件/日志），管理聚焦编辑面板是保留完整 Base URL 的唯一例外；修订作为 spec 变更记录在案。
- [x] 管理列表/只读表面不再渲染完整 Base URL（遮蔽或截断形式）。
- [x] 聚焦编辑面板仍能加载完整 Base URL 并完成编辑（CFG-002 配置能力不回归）。
- [x] canary 断言：诊断面（events、日志、调用记录、备份元数据）在新增渲染路径下仍不出现完整 Base URL（OPS-020/OPS-021）。
- [x] 全套现有测试与新增断言全绿。

Spec coverage: `OPS-020`, `OPS-021`, `CFG-002`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. **spec 修订（记录在案）**：OPS-020 规范本体"控制台"→"诊断面——调用记录、运维事件、日志与状态区"，新增"管理聚焦编辑面板是保留完整 Base URL 的唯一例外（CFG-002 编辑上游供应商所需）；管理列表/只读表面 MUST NOT 渲染完整 Base URL，只允许遮蔽或截断形式"；验收矩阵 OPS-020–OPS-021 行补充"管理聚焦编辑面板加载端点是完整 Base URL 的唯一合法出现处"；决策追溯表新增一行把本次修订指向本 ticket。**实现代码零改动**：探查确认管理列表/只读面（operations providers 只有 id/display_name/api_key_masked、routes/calls/events/backups）本就不渲染完整 Base URL，完整值只出现在 `GET /admin/providers/:id`（编辑面板加载端点）与前端编辑面板表单——现状已符合修订后措辞，未新增截断展示列（ticket 未要求新增 UI 字段，最小改动原则）。**测试补强**：`canary_fields_never_leak_into_events_logs_pages_or_database` 新增两段断言——正向钉死编辑面板端点返回完整 base_url（唯一例外，CFG-002 不回归）+ Operations provider 列表逐个断言无 base_url 字段且不含完整值。验证：canary 单独绿、全套 cargo exit 0（secure 82 + packaging 27）、clippy 零警告。
- 2026-08-12: Code review (dual-axis, via `/tmp/ticket41-change-record.md`) completed——**Standards 轴通过**（无文档化标准违规、无 baseline smell 命中；三条 judgement call：canary 测试注释"show masked/truncated forms only"与断言"完全不渲染"轻微漂移——已采纳修正注释为"carry no Base URL form at all (stricter)"；PATCH 回填例外无直接测试背书——由既有 `administrator_edits_route_eligibility...` 编辑测试间接覆盖，接受；双空行格式——忽略）；**Spec 轴通过**（五条 checklist 全部落实、无越界、无实现错误；口径观察非缺陷——ticket 措辞"只显示遮蔽/截断形式"执行为"不渲染任何形式"，"只允许 X"是上限而非下限，语义决策已在 change record 明示）。

## Answer

实现完成。OPS-020 已修订并把 allowlist 界定到诊断面，编辑面板成为保留完整 Base URL 的唯一例外；管理列表/只读面经探查与测试确认不渲染完整 Base URL；canary 断言已补强钉死边界。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **spec 修订（OPS-020，作为 spec 变更记录在案）**：
  - 规范本体：allowlist 界定到诊断面（调用记录、运维事件、日志与状态区）；新增"管理聚焦编辑面板是保留完整 Base URL 的唯一例外（CFG-002 编辑上游供应商所需）；管理列表/只读表面 MUST NOT 渲染完整 Base URL，只允许遮蔽或截断形式"。
  - 验收矩阵 OPS-020–OPS-021 行补充"管理聚焦编辑面板加载端点是完整 Base URL 的唯一合法出现处"。
  - 决策追溯表新增一行（`issues/41-bound-base-url-rendering.md` → OPS-020 措辞修订），记录本次 spec 变更出处。
- **实现代码零改动（现状已符合）**：
  - 管理列表/只读面：`operations_snapshot` 的 `providers` 只含 `id`/`display_name`/`api_key_masked`（无 base_url 字段）；routes/calls/events/backups/relay-keys 面均只呈现安全本地标识。
  - 唯一例外：`GET /admin/providers/:provider_id` 返回完整 `base_url`，前端 `showProviderPanel` 编辑面板加载完整值并可完成 PATCH 编辑（CFG-002 不回归，既有编辑测试已断言）。
  - 语义决策：列表/只读面现状"不渲染任何形式"严格满足"不再渲染完整 Base URL"（比遮蔽/截断更严）；未新增截断展示列（无 UI 需求依据）。
- **测试补强**：`canary_fields_never_leak_into_events_logs_pages_or_database` 新增编辑面板端点正向断言（`base_url == base_url_canary`，唯一例外被钉死）+ Operations provider 列表逐个断言无 `base_url` 字段且不含完整值；既有 forbidden 扫描（含 base_url_canary）继续覆盖 calls/events/backups/logs/pages/database。
- **验证**：canary 单独绿；`cargo test` cargo exit 0——secure_management_surface 82 个全绿（22.93s）+ packaging_lifecycle 27 个全绿（67.27s）；`cargo clippy --all-targets -- -D warnings` 零警告。双轴 code-review 通过（Standards/Spec 均无阻断项，结论与一处采纳的注释修正见 Comments）。
