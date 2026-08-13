# 53 — OPS-010 状态区计数修订（spec 修订）

**What to build:** OPS-010 规定 Operations 控制台「持久展示五个独立状态区」，而实现有第六个常驻 Recovery 状态卡（展示基础间隔 `B` 与倍增上限 `N`，并可进入恢复设置）。该能力被 ROUTE-019/DATA-002 预期，属有记录的设计决策而非越界实现。本 ticket 修订 OPS-010 措辞把恢复设置状态区纳入允许列表（或并入既有五区之一），并记录为 spec 变更，消除「五区」字面与实现的矛盾。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] OPS-010 措辞修订，恢复设置状态区成为明确允许的常驻状态区（或并入既有区），作为 spec 变更记录在案。
- [x] 决策追溯表新增一行指向本 ticket。
- [x] 若措辞修订不涉及行为：全套现有测试保持绿；若涉及，补对应断言。（措辞修订不涉及行为、零代码改动，无需新增断言；全套 browser 测试的既有失败与本次改动无关，见 Comments）

Spec coverage: `OPS-010`, `ROUTE-019`.

## Answer

OPS-010 措辞修订为 spec 变更：六个独立状态区（Storage、模型路由、备份、迁移与恢复、usage 完整性、恢复设置）。恢复设置状态区为常驻展示区，展示基础间隔 `B` 与倍增上限 `N`（ROUTE-019）并可进入恢复设置面板（DATA-002）；14 天事件历史条款继续限定异常状态。验收矩阵 `OPS-010`–`OPS-016` 行同步（五区→六区），spec.md 决策追溯表新增本 ticket 行。实现零代码改动，无行为变化。

## Comments

- 2026-08-12 — Claimed。核实实现：`src/web/app.js` Operations `status-grid` 实际渲染六张常驻状态卡（Storage、Model routes、Backups、Migration & restore、Usage、Recovery），Recovery 卡展示 `B`/`×2^N`、`data-open-recovery` 可进恢复设置面板，且无异常态标记与事件历史入口，与 ticket 描述一致。
- 2026-08-12 — spec.md OPS-010 与验收矩阵行修订完成；map.md 决策追溯表新增本 ticket 行。待跑全套测试确认绿。
- 2026-08-13 — 全套 `cargo test` 跑两轮：非 browser 测试全绿；browser_surface 首轮 9 失败 / 复跑 7 失败 + 2 转好（flaky）。7 个稳定失败：`browser_data_security_panel_shows_backup_metadata_and_create`、`browser_failed_restore_reports_stage_and_returns_to_operations`、`browser_focus_panels_add_edit_and_cancel_return_to_operations`、`browser_relay_key_create_search_and_revoke`、`browser_route_check_disabled_while_checking_then_available`、`browser_status_area_drills_into_route_event_history`、`browser_validation_errors_render_next_to_fields`。根因均为既有 harness/环境问题（`driver.js` 对 `.focused-panel h2` 的 strict-mode 定位器在事件历史面板双 `<h2>` 时必然抛错、CSP `unsafe-eval` 拒绝 `page.waitForFunction`、click/等待超时），与本次改动无关：本次仅改 `.scratch/` 下 md 文件，`grep -rn scratch tests/ src/` 零命中，无代码被触碰。
- 2026-08-13 — 双轴 code review 完成，两处修正已落地：① spec.md 决策追溯表补本 ticket 行（41/42/43 同类修订均有）；② OPS-010 删除超出 ticket 授权范围的「该区无异常态，不参与事件历史」规范性断言（轻微 creep），map.md 与 ticket Answer 同步。残留项：issues/28 已 resolved 清单项中的「五个独立状态区」为历史记录，判定不回改。
