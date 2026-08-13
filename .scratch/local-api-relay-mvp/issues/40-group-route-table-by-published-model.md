# 40 — Operations 路由表按发布模型分组（UI-002）

**What to build:** Operations 管理页的模型路由表目前是扁平表加一个 "Published model" 列；UI-002 要求"按发布模型分组的紧凑模型路由表"。本 ticket 把路由表改为按发布模型分组展示，组内保留既有排序与全部字段。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 路由表按发布模型分为若干组展示，取代扁平表加 "Published model" 列（UI-002）。
- [x] 现有路由行的全部字段保留（上游供应商、上游模型、协议、倍率、健康、最近故障/检测、下次检测）。
- [x] 组间与组内排序明确且可重复。
- [x] 页面/管理 API 测试断言分组结构存在；全套现有测试保持绿色。

Spec coverage: `UI-002`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill.
- 2026-08-12: Implementation completed. 前端 `routesMarkup` 改为按 `published_model_name` 分组渲染：用 `Map` 按快照数组顺序收集组（后端 `operations_snapshot` 本就 `ORDER BY p.name, r.id`，组间模型名升序、组内稳定 route-id 排序，前端不重排），每组 `<section class="route-group">` = 组标题 `<h3 class="route-group-title">` + 一张 `.data-table.routes-table`；表头去掉 "Published model" 列（9 列），行去掉发布模型 cell，其余字段与按钮（provider/upstream/protocol/multiplier/health 及 detail/state age/last check/next probe/Edit/Check）逐字保留；空路由分支不变。CSS：routes 表格 grid 10 列 → 9 列（min-width 1080px → 990px），新增 `.route-group`/`.route-group-title` 样式。后端与管理 API 契约零改动（`/admin/operations` 的 `routes` 数组结构与字段不变）。测试：新增 `operations_route_rows_group_by_published_model_in_the_operations_table`——建 2×gpt-5.6-sol + 1×deepseek-v4-flash 路由（创建顺序不匹配分组顺序），断言 `/admin/operations` 的 routes 按发布模型连续分组且组间升序（`names == ["deepseek-v4-flash","gpt-5.6-sol","gpt-5.6-sol"]`），且整体等于 `(published_model_name, id)` 稳定排序（首版按"创建顺序"断言失败——route id 是随机稳定字符串而非自增序号，组内是 id 字典序，修正为对排序规则的直接断言）；GET `/assets/app.js` 断言 `contains("route-group")`（页面分组结构存在，符合既有嵌入脚本静态检查模式；不做负向断言——calls 表的 "Published model" 列是 UI-010 的合法要求）。既有脚本行为 `querySelector(".routes-table")?.scrollIntoView` 分组后命中第一个路由组，功能保留。验证：`node --check src/web/app.js` 通过、clippy 零警告、`cargo test` cargo exit 0（secure_management_surface 82 + packaging_lifecycle 全绿，含新增 1 个）。
- 2026-08-12: Code review (dual-axis, adapted for the git-less repo via the change record `/tmp/ticket40-change-record.md`) completed. **Standards 轴**：通过——无文档化标准违规（repo 无编码标准文件，clippy/`node --check` 由工具强制跳过）；baseline smells 无实质命中，唯一发现为 judgement call：`.route-group:first-child` 是永不匹配的死选择器（`.route-group` 在 `.table-region` 内位于 `.table-heading` 之后，非父容器 first-child），意图"首个组不加顶边距"未生效——已采纳修复为 `.table-region > .route-group:first-of-type`。**Spec 轴**：UI-002 四项 checklist 全部落实、无越界（后端与管理 API 契约零改动，CSS 列数/宽度变化为删列的必要后果，`scrollIntoView` 行为有记录且保留）；两处观察：页面级断言（`script.contains("route-group")`）为嵌入脚本静态检查、非渲染 DOM 断言——与 repo 既有模式一致（无浏览器自动化），已加强为同时断言 `route-group-title`（组标题渲染结构）；两个 API 断言（names 连续分组 + 整体 `(model, id)` 排序）存在蕴含关系——保留以提升失败可读性，非错误。修复后测试单独全绿 + clippy 零警告。

## Answer

实现完成。Operations 路由表已按发布模型分组展示（UI-002），取代扁平表加 "Published model" 列；组间/组内排序明确可重复，全部字段保留。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **前端 `src/web/app.js` `routesMarkup`**：按 `published_model_name` 分组，`Map` 保持后端数组顺序（不重排）；每组 = `.route-group` 容器（`<h3 class="route-group-title">` 组标题）+ 独立 `.data-table.routes-table`（表头 9 列，去掉 "Published model"；行去掉发布模型 cell，其余字段与 Edit/Check 按钮逐字保留）。空路由分支不变。
- **排序**：组间发布模型名升序、组内 route id 升序（`operations_snapshot` 既有 `ORDER BY p.name, r.id`；id 为随机稳定字符串，组内为 id 字典序）——前端仅消费后端序，确定性且可重复。
- **CSS `src/web/app.css`**：routes 表格 grid-template-columns 10 列 → 9 列（min-width 1080px → 990px），新增 `.route-group`（margin-top 18px、first-child 0）与 `.route-group-title`。
- **后端零改动**：`/admin/operations` `routes` 数组结构与字段不变，分组纯为展示层。
- **测试**：新增 `operations_route_rows_group_by_published_model_in_the_operations_table`（分组连续 + 组间升序 + 整体等于 `(model, id)` 稳定排序 + `app.js` 含 `route-group`）。
- **验证**：`node --check src/web/app.js` 通过、clippy 零警告；`cargo test` cargo exit 0——secure_management_surface 82 个全绿（32.11s，含新增 1 个），packaging_lifecycle 全绿。双轴 code-review 完成（结果见 Comments）。
