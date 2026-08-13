# 35 — 路由健康状态类型化

**What to build:** 纯重构。模型路由健康的三态（检测中 / 可用 / 暂不可用）目前以字符串字面量表示并在多处比较；把它换成与存储健康等已有类型一致的类型化枚举，消除 stringly-typed 的 Primitive Obsession。对外行为不变，全套现有测试保持绿色。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 路由健康三态在代码中只有一种类型化表示，不再用字符串字面量比较（`"available"` 等）。
- [x] 启动时全部路由置为检测中、检测中路由排除出候选集、可用路由进入候选、暂不可用路由排除的既有行为不变（ROUTE-003/004/005）。
- [x] 与 `StorageState` 等既有类型的风格一致。
- [x] 全套现有测试通过，无任何行为变化。

Spec coverage: None — pure refactor. 建议在本 ticket 之后、38 之前执行（两者都动路由健康代码）。

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. Added `store::RouteHealth`（`Checking` / `Available` / `Unavailable`，风格对齐 `StorageState` 的 enum + `as_str()`，另有 `from_persisted` 边界解析）；`RouteSummary` / `RelayRouteCandidate` / `EligibleModelRoute` 的 `health` 字段从 `String` 类型化为 `RouteHealth`，四个行读取点在 `query_map` 闭包内经 `from_persisted` 转换；`record_probe_result` 的 k 推进判断与 `RouteHealthOverride.state`（`&'static str` → `RouteHealth`）、`effective_health`（返回 `RouteHealth`，生命周期签名随 Copy 简化）全部类型化；Operations 页三态计数改为按 `RouteHealth` 变体累计，wire 值经 `as_str()` 序列化，JSON 输出与前端 `app.js` 契约逐字节不变。全量验证：`cargo check --all-targets` 通过、clippy 零警告、`cargo test` exit 0（secure_management_surface 78 + packaging_lifecycle 27 全绿）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket35-change-record.md`) completed. Standards axis: no documented-standard violations (repo has no coding-standards file); judgement calls evaluated — (1) `if available { RouteHealth::Available } else { RouteHealth::Unavailable }` appears three times (server.rs probe responses + `record_probe_result` override) but is pre-existing duplication re-expressed from the identical old string form, left as-is per minimal-change scope; (2) Operations-page counting inside the row-builder `map` closure is mildly impure but behavior-equivalent, left; (3) `from_persisted`'s unknown→Checking fallback is sound and behavior-preserving at every comparison point, with the schema CHECK making the branch unreachable. One accuracy issue fixed: the enum variant doc comments cited ROUTE-003/004 for behaviors that belong to ROUTE-005/006/010 (verifiable against spec.md), corrected. Spec axis: no missing or partial requirements — every string-literal health comparison in `src/*.rs` is gone except the allowed boundaries (enum `as_str`/`from_persisted`, JSON response keys, the `routes.check` event payload's `result` strings); no scope creep (quarantine/recovery/candidate semantics identical); one inaccurate claim in the change record corrected — for the CHECK-unreachable unknown-value case the old Operations count/wire put the raw string in no bucket while the new code counts it as Checking, so the parity claim now explicitly scopes to the three reachable values (no reachable behavior difference). Full suite re-verified after the fixes: `cargo check --all-targets` + clippy clean, 105 tests green.

## Answer

实现完成，纯重构。模型路由健康三态（检测中 / 可用 / 暂不可用）的 stringly-typed 字符串字面量表示已整体替换为类型化枚举 `store::RouteHealth`，消除 Primitive Obsession；对外行为完全不变，全套现有测试通过、clippy 零警告。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **新增 `store::RouteHealth`**：`Checking` / `Available` / `Unavailable` 三个变体，风格与 `StorageState` 一致（`#[derive(Debug, Clone, Copy, PartialEq, Eq)]` + `as_str()`）。`as_str()` 输出 `"checking"` / `"available"` / `"unavailable"`（JSON wire 值与数据库字符串不变）；`from_persisted(&str)` 在行读取边界解析 `model_route_health.state`，未知值（schema CHECK 下不可达的损坏行）归为 `Checking`——与旧实现中未知字符串在各比较点（候选排除、k 不推进、`current_interval_ms` 为空）的行为逐点一致。
- **store.rs 字段类型化**：`RouteSummary.health`、`RelayRouteCandidate.health`、`EligibleModelRoute.health` 从 `String` 改为 `RouteHealth`，四个 `query_map` 行读取点在闭包内先取 `String` 再 `from_persisted`；`record_probe_result` 的 k 推进判断 `state == "unavailable"` 改为 `RouteHealth::from_persisted(&state) == RouteHealth::Unavailable`。SQL 写入字面量（`'checking'` / `'available'` / `'unavailable'`）与 schema CHECK 未动。
- **server.rs 类型化**：`RouteHealthOverride.state` 从 `&'static str` 改为 `RouteHealth`（两处构造 `quarantine_route` / `record_probe_result` 的内存 override）；`effective_health` 返回 `RouteHealth`（`RouteHealth: Copy` 使原 `'a` 生命周期签名简化为按值传递）；三处 `== "available"` 过滤（`list_relay_models` / `effective_candidates`）改为 `== RouteHealth::Available`；Operations 页 `current_interval_ms` 判断改为 `== RouteHealth::Unavailable`，三态计数从"JSON 行再过滤字符串"改为在构造行时按 `RouteHealth` 变体累计，`"health": health.as_str()` 序列化保持 wire 值逐字节不变。
- **行为保持**：ROUTE-003/004/005 三态语义不变（启动/恢复置 Checking、Checking 排除出候选、Available 进入候选、Unavailable 排除）；DATA-005 内存 override 优先/持久值回退顺序不变；前端 `app.js` 与全部测试的 `"health"` 字符串断言契约不变。
- **验证**：`cargo check --all-targets` 通过、clippy 零警告；`cargo test` 全套 exit 0——`secure_management_surface` 78 个全绿（含 Checking 排除、恢复探测、quarantine、`await_route_health("available"/"unavailable")` 等健康用例），`packaging_lifecycle` 27 个全绿。
