# 25 — 暴露存储降级与 usage 缺口

**What to build:** 当调用、usage、费用、健康历史或运维记录无法持久化时，客户端已经成功的调用仍然成功，而管理员能在 Operations 立即看到 Storage Degraded、受影响类别、丢失数量和 usage gap；持久化恢复后当前状态可受控清除，但历史缺口不会被掩盖。

**Blocked by:** 22 — 启动检测并恢复暂不可用模型路由; 24 — 计算费用并聚合用量

**Status:** resolved

- [x] 运维记录在可靠事实可用时事务写入，但失败不推翻或延迟已完成的成功 API 响应。
- [x] 模型路由内存状态立即转换，即使健康历史写入失败；系统不为中断流、缺失上游 usage 或失败本地写入发明数据。
- [x] Operations 持久状态区显示 Storage 的 Healthy/Degraded/Not ready、开始时间、受影响记录类别、规范化错误和已知丢失数或 unknown。
- [x] Usage 完整性显示选定窗口的每个已知 gap 及起止；缺口不得估算、回填或因当前存储恢复而消失。
- [x] 只有相同记录类别重新写入成功且 SQLite 轻量完整性检查通过，当前 Degraded 才自动清除；历史事件与 gap 保留。
- [x] 故障注入进程测试逐类阻断运维写入，断言客户端响应、内存路由、状态区、恢复条件和永久不完整标记。

Spec coverage: `DATA-004`–`DATA-005`, `OPS-010`–`OPS-012`, `OPS-016`.

## Answer

实现（schema v7→v8 新增 `usage_gaps` 表，全部行为经真实进程 loopback 边界验证）：

- **Storage 健康状态（OPS-010/011）**：AppState 共享 `StorageHealth`（Healthy/Degraded/Not ready、`since` 开始时间、受影响类别 `BTreeMap<category, {since, error, lost_records}>`）。三类降级来源：`write_call_record`（call_records）、`quarantine_route`/`record_probe_result`（route_health）。任何运维写入失败即时置 Degraded 并 eprintln；成功写入触发 `mark_storage_category_recovered`。
- **内存路由立即转换（DATA-005）**：路由健康以 SQLite 行为持久真相，另加 `route_health_override` 内存覆盖层。候选查询不再过滤 `h.state='available'` 而是返回持久健康，server 用 `effective_candidates` 按「覆盖层优先、否则 SQLite」过滤；`/v1/models`、operations 路由行同样合并覆盖层。quarantine 持久化失败时路由立即在内存中排除并显示 unavailable，恢复需同类别成功写或重启。
- **自动清除（OPS-012）**：类别重新持久化成功（含 quarantine 成功本身）且所有降级类别已恢复后运行 `PRAGMA quick_check`，通过 → Healthy，失败 → Not ready；`usage_gaps` 历史行与 gap 永不删除。
- **Usage gap（OPS-016）**：`usage_gaps(category, started_at_ms, ended_at_ms, lost_records)` 持久化本地持久化失败；`record_call` 失败时 open/extend，成功时同事务 close。`usage_integrity(window, now_ms)` 按窗口列出持久化 gap（重叠判定）+ 缺失上游 usage 的成功调用（`succeeded=1 AND input_tokens IS NULL`，派生点 gap），`complete=false` 当且仅当有重叠 gap。operations 的 `usage` 区基于 all-time 完整性（no_data/complete/incomplete）。
- **故障注入**：`LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE`（always）与 `_ONCE`（仅第一次匹配写入失败，用 AtomicBool）取值 `call_records`/`route_health`/`all`，沿用 `fail_config_commit_if_requested` 风格。
- **API/UI**：operations `storage`（state/since/categories/accounting_gaps）与 `usage`（state/gaps）真实化；calls-usage 新增 `usage_integrity`；app.js 渲染 Storage 卡详情（类别、丢失数、错误、accounting gap）与 Calls & usage 的 Usage integrity 区（complete 时也明确标记）。

新增测试（tests/secure_management_surface.rs，全部进程边界）：
- `operational_write_failures_degrade_storage_without_failing_the_client` — DATA-004/005 + OPS-011/016：fail-always call_records 下两次调用客户端均 200、无调用记录、storage degraded（lost=2）、accounting gap 开放、usage incomplete、totals 不虚构。
- `storage_degradation_clears_only_after_same_category_rewrite_and_quick_check` — OPS-012/016：fail-once 首次写失败 → degraded + 开放 gap；第二次写成功 → healthy、gap 关闭但保留、usage 永久 incomplete；重启后 gap 仍持久存在。
- `failed_health_persistence_still_quarantines_the_route_in_memory` — DATA-005：fail-always route_health 下 500 路由被立即排除（下个请求直走健康路由）、operations 显示 unavailable、storage degraded（route_health 无 accounting gap）、重启后恢复 healthy 且正常服务。
- `successful_calls_without_reported_usage_are_marked_as_known_gaps` — OPS-016：无 usage 成功调用不入 totals、列为 missing_upstream_usage 点 gap、operations usage incomplete。

验收证据：`cargo test` 61 通过 0 失败（57 存量 + 4 新增）；`cargo clippy --all-targets` 0 警告；app.js 通过 `node --check`。既有 backup 测试的 schema_version 断言 7→8 随迁移同步更新。

## Comments

### code-review follow-up（2026-08-10）

Standards/Spec 双轴 review 修复：quarantine 成功写入现在也计入 route_health 同类别恢复条件（OPS-012 自动清除此前只挂在 probe-result 成功上）；`quarantine_route` 的持久化失败后路由内存排除保持（恢复靠手工 check 或重启，不发明恢复调度）。DRY：抽出 `effective_health`/`overrides_snapshot`/`usage_gap_json`，`collect_usage_gaps` 合并重复 SQL，`eligible_model_route_health` 返回 `EligibleModelRoute` 结构，gap kind 改用常量（`USAGE_GAP_KIND_*`）。UI：`usageIntegrityMarkup` 在窗口完整时也明确标记（OPS-016「明确标记所选区间是否完整」）。未采纳项：`RouteHealthOverride.state` 仍为字符串（与既有健康 TEXT 模型一致，枚举属更大重构）、运维事件类别无降级路径（运维事件持久化属 ticket 28 范围）、operations 的 `no_data|complete|incomplete` 三态（保留既有 no_data 显示语义）、gap 关闭时间用 `record.created_at_ms`（调用起点，自洽且确定）。

