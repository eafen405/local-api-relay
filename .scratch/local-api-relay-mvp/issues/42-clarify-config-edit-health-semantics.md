# 42 — 界定配置编辑的健康语义（UI-007/ROUTE-010）

**What to build:** 当前编辑上游供应商或模型路由后，服务会重置相关路由为 Checking 并立即执行原生探测，探测成功即恢复为 Available——即使该路由此前处于暂不可用隔离状态。这与 spec 的 UI-007「配置修正本身 MUST 健康中性」和 ROUTE-010「只有当前隔离周期的专用恢复检测成功才能恢复」字面冲突。本 ticket 裁决这一落差并落地：要么修订 spec 措辞使「配置编辑触发重新检测」成为明确许可的恢复通道（并记录为 spec 变更），要么收紧行为让配置编辑不改变健康状态；不得留下两者矛盾的中间态。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 裁决「配置编辑触发重新检测」与 UI-007/ROUTE-010 的关系，结论记录为 spec 变更（修订规范本体 + 决策追溯表新增一行，指向本 ticket），或改为收紧行为。
- [x] 若走 spec 修订：验收矩阵相应行同步修订，说明配置编辑的探测属于系统所有权重检，不构成对 ROUTE-010 的违反。
- [ ] 若走行为收紧：编辑连接类字段不得直接改变健康状态，恢复只能经专用恢复检测；补对应进程边界测试。
- [x] 全套现有测试保持绿，clippy 零警告。

Spec coverage: `UI-007`, `ROUTE-010`, `ROUTE-021`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. 裁决：修订 spec 措辞（许可「配置编辑触发重新检测」为系统所有权重检通道）——连接相关配置编辑使既有隔离证据失效并结束隔离周期，路由重新进入 Checking 由系统以同一原生探测重检，与启动（ROUTE-004）/恢复（DATA-016）fresh-start 同族；编辑动作不直接设置健康（健康中性）。spec 修订 ROUTE-010 + UI-007 + 验收矩阵两行 + 决策追溯表一行；实现代码零改动（`connection_changed`/`needs_check` 语义已正确）。新增进程边界测试 `provider_connection_edits_recheck_an_unavailable_route_but_health_neutral_edits_do_not`。
- 2026-08-12: Code review (dual-axis) 通过——Standards 轴无硬性违规（judgement calls：变更记录初版全量测试标注"等待完成"已更新为实际结果；Answer 引用 ROUTE-004/DATA-016 与 spec 正文 ROUTE-016 系同族引用，连贯；二选一分支未勾选项有意为之）；Spec 轴无缺失实现、无越界，两点测试覆盖缺口已修复——补路由级连接类编辑（改 upstream_model_name → Checking 重检用新配置、失败仍 Unavailable）断言，钉住 ROUTE-010 规范句的「模型路由的连接类字段」分支。验证：全套 cargo exit 0、111 测试全绿、clippy 零警告。

## Answer

裁决：**修订 spec 措辞**（许可「配置编辑触发重新检测」为系统所有权重检通道）。依据：

- 字节级「恢复只能经专用恢复检测」针对的是**同一连接配置**下的隔离周期：隔离证据（可归因故障）指向旧连接。连接相关配置编辑（供应商 Base URL/API key，或路由的供应商/上游模型名/协议）使该证据失效，旧隔离周期随之结束，受影响路由重新进入 Checking（系统所有态，非 Available），再由系统以同一原生探测（ROUTE-016 语义）决定新健康——与启动（ROUTE-004）和恢复（DATA-016）的 fresh-start 周期同族。
- 健康中性（UI-007）指**编辑动作不直接设置健康**：行为实测确认编辑后路由只进入 Checking，Available/Unavailable 完全由系统重检的探测结果决定；向仍损坏的连接编辑不会恢复路由，向可用连接编辑立即恢复（无需等待恢复调度）。
- 非连接类编辑（显示名、价格、倍率、资格）不触发重检、保持健康与隔离周期不变（代码 `update_provider` 的 `connection_changed` 与 `update_model_route` 的 `needs_check` 已实现此语义）。

落地：

- spec 修订：ROUTE-010 增补连接相关编辑结束隔离周期并触发系统所有权重检的明确例外；UI-007 澄清「健康中性」= 编辑不直接设置健康；验收矩阵 `ROUTE-010`–`ROUTE-011` 与 `UI-007`–`UI-009` 两行同步修订；决策追溯表新增一行指向本 ticket。
- 进程边界测试 `provider_connection_edits_recheck_an_unavailable_route_but_health_neutral_edits_do_not` 钉住：创建探测失败 → Unavailable；仅换 API key 且连接仍损坏 → 重检失败仍 Unavailable（编辑不设置健康）；改指可用连接 → 系统重检立即 Available（不等 30s 恢复调度）；显示名编辑与倍率编辑 → 健康不变且不产生新探测。
- 全套测试绿，clippy 零警告。
