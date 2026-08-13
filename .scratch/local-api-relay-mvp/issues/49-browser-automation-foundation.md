# 49 — 浏览器自动化测试地基（UI-001..013 + ROUTE-022）

**What to build:** spec 测试决策要求「Web 管理流程使用浏览器自动化从真实服务进入，验证用户可见状态、校验、创建/撤销、探测和 drill-down」，且好测试「不断言前端组件树」而断言外部行为。当前 UI 证据要么缺失（UI-001..013 几乎无浏览器自动化），要么退化为 grep 嵌入静态脚本里的字符串标记（违反测试原则）。本 ticket 引入浏览器自动化地基（如 Playwright），从真实本地中转进程进入，覆盖主要管理流程：登录、Operations 默认视图、Calls & usage 次级视图、按发布模型分组的路由表、聚焦面板新增/编辑/保存返回、密钥创建/搜索/一次显示/撤销确认、路由检查交互与恢复检测状态流转、调用记录 drill-down 与尝试链、数据安全面板。为后续 UI ticket（44/45/46）与 ROUTE-022 提供可复用的浏览器验收通道。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 引入浏览器自动化依赖与最小 harness，可从真实 loopback 服务进入管理面。
- [x] 自动化覆盖 UI-001..003 视图结构（Operations 默认、Calls & usage、分组路由表）。
- [x] 自动化覆盖聚焦面板新增/编辑供应商、模型路由、密钥并返回原上下文（UI-005）。
- [x] 自动化覆盖密钥创建一次显示、搜索、撤销确认（UI-009）。
- [x] 自动化覆盖路由检查交互的 disabled/loading/success/error/retry 状态与安全结果（UI-008/ROUTE-022）。
- [x] 自动化覆盖调用详情 modal 中的 metadata-only 尝试链（UI-010/UI-011）。
- [x] 移除依赖 grep 前端脚本字符串的断言方式，改为断言用户可见行为。
- [x] 全套现有测试保持绿。

Spec coverage: `UI-001`–`UI-013`, `ROUTE-022`.

## Answer

浏览器自动化地基已交付：真实 headless Chromium（Playwright 1.49）通过真实 loopback 中转进程进入管理面，14 个进程边界测试覆盖 UI-001..013 + ROUTE-022 的用户可见行为，并把 6 处 grep 前端脚本字符串的断言替换为浏览器断言。全部测试在 Playwright 缺失的环境自动 skip（其余套件不受影响）。

**新增文件**
- `tests/browser/driver.js` — Playwright 驱动（14 场景）：`node driver.js <scenario> --base <url> --credential <pw> [--new-credential <pw>] [--extra <json>]`，stdout 输出结构化 evidence JSON，只收集用户可见观察。
- `tests/browser/package.json` — 声明 `playwright@1.49.1`（测试专用，生产不依赖 Node）。
- `tests/browser_surface.rs` — 14 个 `#[tokio::test]` + harness（发现/调用/跳过/串行化）。

**修改文件**
- `Cargo.toml` — tokio 增加 `"process"` feature（harness 超时跑 driver）。
- `tests/secure_management_surface.rs` — 移除 6 处「grep 前端脚本字符串」断言（`renderShell("operations")` 标记、route-group 标记、checklist 标记、`renderFieldErrors` 标记、restore 面板标记、`data-open-events` 标记），保留凭据/隐私扫描。

**测试清单（映射验收）**
1. `browser_login_lands_on_operations_default_view` — UI-001/003/013：登录强制改密（SEC-004）、Operations 默认、两持久视图导航、status grid、无 Accounts/Groups/Channels。
2. `browser_calls_usage_is_the_secondary_view_and_navigation_round_trips` — UI-001/OPS-008：次级视图、六时间窗、往返。
3. `browser_operations_groups_routes_by_published_model` — UI-002：分组路由表 + 行字段。
4. `browser_focus_panels_add_edit_and_cancel_return_to_operations` — UI-005：聚焦面板新增/编辑/回填/取消返回。
5. `browser_relay_key_create_search_and_revoke` — UI-009：一次显示、列表 prefix、搜索、撤销确认。
6. `browser_route_check_recovers_an_unavailable_route_with_a_fixed_native_probe` — UI-008/ROUTE-022：loading/success、固定原生探测、无任意 prompt/模型输入。
7. `browser_route_check_disabled_while_checking_then_available` — UI-008 disabled 态（重启启动探测窗口）。
8. `browser_route_check_error_shows_safe_feedback_and_leaves_retry_available` — UI-008 error/retry（409 安全消息、不泄 key）。
9. `browser_call_detail_expands_metadata_only_attempt_chain` — UI-010/011：fallback 尝试链 metadata-only。
10. `browser_onboarding_checklist_tracks_six_steps_and_hides_when_callable` — UI-004。
11. `browser_validation_errors_render_next_to_fields` — UI-006：字段级错误贴近输入。
12. `browser_data_security_panel_shows_backup_metadata_and_create` — UI-012：元数据 + 手工备份 + 无云/下载/删除。
13. `browser_failed_restore_reports_stage_and_returns_to_operations` — UI-012/OPS-015：失败阶段 + 可操作原因 + 返回 Operations。
14. `browser_status_area_drills_into_route_event_history` — OPS-010：事件历史钻取。

**Harness 关键决策（后续 agent 必读，详见 `/tmp/49-change-record.md`）**
- 隔离环境：Node + Playwright npm 前缀 `/tmp/local-api-relay-playwright/`，Chromium `/tmp/local-api-relay-playwright-browsers/chromium-1148/`（镜像无 headless-shell zip，用完整版 + `--no-sandbox`）；harness 缺失时测试 skip。
- 串行化：`BROWSER_SERIAL` mutex；时序敏感场景（route-check-disabled）在锁内做 setup。
- CSP `default-src 'self'`：不用 `waitForFunction`，统一 `waitForSelector` + `:has-text` / `filter({hasText})`。
- `create_model_route` 同步执行 probe → checking 态只能在重启后的启动探测窗口观察；Operations 不自动刷新 → driver 等探测时长后轮询 reload。
- `window.confirm` 须在 click 前注册 dialog handler 自动 accept。
- driver 的 90s race 定时器与 trace 文件流都必须在结束时清理，否则 node 事件循环不退出（harness `child.wait()` 挂死）。
- 上游 worker 只服务确切数量的连接（否则 `worker.join()` 挂死）。
- 时序测试把 recovery-settings 基间隔设为 1h。

**验证**
- `cargo test --test browser_surface`：14/14 绿，约 43s。
- 全套 `cargo test`：135 测试（secure 92 + packaging 29 + browser 14）全绿；并行下偶发既有 flaky（secure `restore_reports_in_flight_stage_progress_at_the_process_boundary`、packaging `interactive_sigint_or_launcher_sigterm_stops_gracefully`）单独重跑即绿，与本变更无关。
- `cargo clippy --all-targets`：零警告。

## Comments

- 2026-08-13：实现完成。变更记录 `/tmp/49-change-record.md`；双轴 code review 通过，审查修复见下。
- 2026-08-13（review 修复）：Spec 轴 4 缺口 + 2 断言问题已修——UI-002 行全列断言（状态年龄/最近检测）、UI-007 编辑面板无健康字段、UI-006 密钥资格字段错误、UI-009 行 scope 断言、数据安全面板改断言「Last verified/Retained」元数据、route-check-disabled 对 create/startup probe 都断言固定原生探测；Standards 轴——fresh 登录显式传 `Some(FINAL_CREDENTIAL)`（消除跨语言常量隐式耦合）、提取 `RECOVERY_TEST_BASE_INTERVAL_MS` 与 `BROWSER_SKIP_NOTICE`、800ms 固定 sleep 改为等待按钮 disabled、移除恒真 `chainVisible` 证据。修复后 14 浏览器测试全绿，全套 135 测试全绿，clippy 零警告。
