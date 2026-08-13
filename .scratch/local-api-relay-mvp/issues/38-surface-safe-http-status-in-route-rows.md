# 38 — 运维路由行显示安全 HTTP 状态（OPS-013）

**What to build:** Operations 路由行目前展示了状态年龄、最近检测/故障时间与类别、下次恢复检测和当前倍增间隔，但缺少最近一次检测/可归因故障的**安全 HTTP 状态**（OPS-013 要求展示它）。本 ticket 从数据库到页面补齐该字段：新增 schema 迁移记录最近安全 HTTP 状态，管理 API 与路由行渲染它，无状态时显示未知而非零。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 路由健康持久化最近一次检测/可归因故障的安全 HTTP 状态（新增 schema 迁移，走既有备份门控迁移合同 DATA-007/008）。
- [x] 管理 API 的运维路由数据包含该状态；没有该状态时显示未知而非零。
- [x] Operations 路由行渲染该字段（OPS-013）。
- [x] 迁移、恢复、回退后的行为符合既有合同（恢复成功后所有路由重新进入检测中，DATA-016）。
- [x] 全套现有测试与新增断言全绿。

Spec coverage: `OPS-013`, `DATA-007`, `DATA-008`, `DATA-016`. 建议在 35 之后执行。

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. Schema v11（迁移臂 10）为 `model_route_health` 增加 nullable 列 `last_http_status INTEGER`（旧库迁移后为 NULL=未知）；`RouteSummary` 新增 `last_http_status: Option<i64>`，`operations_snapshot` 查询/行构造带上该列；`record_probe_result` 与 `quarantine_route` 增加 `http_status: Option<i64>` 参数并在 UPDATE 中记录（传输错误/截断体读取失败/流式语义失败传 None → 置 NULL，与 OPS-003 调用记录 attempt 的安全 HTTP 状态语义一致）；`native_probe` 返回 `(bool, Option<i64>)`（收到任何 HTTP 响应即记录其状态码，含非 2xx；传输错误为 None）；Operations 路由行 JSON 新增 `last_http_status`（无状态为 null）；前端 `routesMarkup` 健康格新增 `HTTP <status>` 小字（`.route-detail` 复用，null 不渲染即未知而非零）。语义决策：把路由置回 Checking 的既有重置 SQL（启动/供应商编辑/路由编辑）不清理该列——它是"最近一次检测/可归因故障"的历史状态而非当前健康（状态列仍显示 checking/available/unavailable），且启动重置运行于二进制可能打开的任何旧 schema（pin-9 演练），引用 v11 列会破坏旧库演练；恢复后路由重新进入 Checking 的 DATA-016 行为不变。测试：新增 `operations_route_rows_surface_the_last_safe_http_status`（200 探测→200、500 探测→500、死端点→null）与 helper `route_last_http_status`；扩展 `attributable_upstream_failures_quarantine_the_route_and_fallback_to_the_next_candidate` 对 8 种失败模式断言 last_http_status（HTTP 状态码 / InvalidJson→200 / TruncatedBody→null / ConnectionRefused→null）；schema 升版联动更新：secure 侧 `schema_version`/`running_schema`/`supported_schema` 断言 10→11、"新于支持版本"伪造版本 11→12、`downgrade_to_schema` 对版本 <11 额外 DROP 该列并补 `pragma_table_info` 断言、`old_schema_startup` 迁移后列存在断言；packaging 侧 6 处 `database_schema == Some(10)`→`Some(11)` 与 restore-gate 备份 schema 断言。全量验证：`cargo check --all-targets` 通过、clippy 零警告、`cargo test` cargo exit 0（secure_management_surface 80 + packaging_lifecycle 27 全绿，107/107）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket38-change-record.md`) completed. **Standards 轴**：通过——无文档化标准违规（repo 无编码标准文件，clippy/rustfmt 由工具强制跳过）；baseline smells 无实质命中，均为 judgement call（`native_probe` 返回 `(bool, Option<i64>)` 元组在 6 处调用点规模下取舍合理，`Option<i64>` 与既有 `call_attempts.http_status` 持久化惯例一致；迁移写法 nullable 无 NOT NULL DEFAULT 对非空表 ADD COLUMN 安全；`downgrade_to_schema` 的 DROP COLUMN 因该列无索引/CHECK 且 data_change 触发器不带列清单而安全；新测试 expected 为独立字面量非 tautological；唯一可改进点：两处提交后 quarantine 裸 `None` 参数缺注释——已在修复中顺带补上）。**Spec 轴**：五项 checklist 主体全部落实，无越界（`native_probe` 记录非 2xx 探测状态码在"最近检测…的安全 HTTP 状态"要求内）；指出一处 implemented-but-wrong 与一处未验证行为，均已修复——(1) 流式提交后 quarantine 传 `None` 但上游 HTTP 状态已知（已 relay 的 200，`status` 不在 unfold 闭包作用域内）：OPS-003 尝试记录保留 `Some(200)` 而路由行被清成 NULL，口径不一致且抹掉已知状态；修复为 unfold state 扩为 `(relay, recorder, http_status)`，两处提交后 quarantine 传 `Some(http_status)`（附注释）。(2) "重置（启动/编辑/恢复）不清理 last_http_status"语义决策此前无测试；在 `explicit_restore_preserves_current_database_and_rechecks_restored_routes` 的 restore Checking 窗口（600ms 延迟 re-probe 可观测）新增断言：路由行 `health == "checking"` 且 `last_http_status == 200`（备份前历史状态经 restore 保留、不显示为当前健康，OPS-015/DATA-016）。修复后受影响测试单独全绿；`cargo check --all-targets` + clippy 零警告；全套测试 cargo exit 0（secure_management_surface 80 + packaging_lifecycle 27 全绿，107/107）。

## Answer

实现完成。Operations 路由行现已展示最近一次检测/可归因故障的安全 HTTP 状态（OPS-013），从数据库持久化、管理 API 到前端渲染全线补齐；无状态时显示未知而非零。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **schema v11（迁移臂 10，走既有备份门控合同 DATA-007/008）**：`ALTER TABLE model_route_health ADD COLUMN last_http_status INTEGER`（nullable；旧库迁移后 NULL=未知；创建路由的既有 INSERT 不列该列→NULL 安全）。
- **store.rs**：`RouteSummary` 新增 `last_http_status: Option<i64>`；`operations_snapshot` 查询/行构造带上该列；`record_probe_result(route_id, available, http_status)` 与 `quarantine_route(route_id, category, http_status)` 的 UPDATE 均记录该列。语义：最近一次健康事件无 HTTP 状态（传输错误、截断体读取失败）时置 NULL，与 OPS-003 调用记录 attempt 的安全 HTTP 状态口径一致；重置 SQL（启动/供应商编辑/路由编辑）不清理该列——它是历史"最近一次检测/故障"状态而非当前健康（状态列仍显示 checking/available/unavailable），且启动重置运行于二进制可能打开的任何旧 schema（pin-9 演练），引用 v11 列会破坏旧库演练；恢复后路由重新进入 Checking 的 DATA-016 行为不变。
- **server.rs**：`native_probe` 返回 `(bool, Option<i64>)`——收到任何上游 HTTP 响应（含非 2xx）即记录其状态码，传输错误/超时为 None，body 解析失败仍保留已收到的状态码；6 处探测调用点（启动/恢复调度/更新供应商/创建路由/更新路由/手动检查）与 5 处 quarantine 调用点透传状态（预提交：传输错误 None、可归因 HTTP 状态 Some、PreCommitFailure 带 failure.http_status；提交后：语义失败/流中断传 Some(上游状态码)，因为已 relay 的 SSE 响应必然携带该状态）；Operations 路由行 JSON 新增 `last_http_status`（无状态为 null）。
- **前端 app.js**：`routesMarkup` 健康格新增 `HTTP <status>` 小字（复用 `.route-detail`；null 不渲染——未知而非零）。
- **测试**：新增 `operations_route_rows_surface_the_last_safe_http_status`（200 探测→200、500 探测→500、死端点→null）与 helper `route_last_http_status`；扩展 `attributable_upstream_failures_quarantine_the_route_and_fallback_to_the_next_candidate` 对 8 种失败模式断言 last_http_status（HTTP 状态码 / InvalidJson→200 / TruncatedBody→null / ConnectionRefused→null）；restore 演练在 Checking 窗口断言历史状态保留；schema 升版联动：secure 侧 schema_version/running_schema/supported_schema 断言 10→11、"新于支持版本"伪造版本 11→12、`downgrade_to_schema` 对 <11 额外 DROP 该列 + `pragma_table_info` 列存在断言；packaging 侧 6 处 `Some(10)`→`Some(11)` 与 restore-gate 备份 schema 断言。
- **验证**：`cargo check --all-targets` 通过、clippy 零警告；`cargo test` cargo exit 0——`secure_management_surface` 80 个全绿（22.86s），`packaging_lifecycle` 27 个全绿（67.08s），共 107/107。双轴 code-review 完成（Standards 无违规；Spec 一处 implemented-but-wrong 与一处未验证语义均已修复并复验）。
