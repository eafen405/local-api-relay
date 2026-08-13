# 24 — 计算费用并聚合用量

**What to build:** 让管理员基于成功模型路由报告的 usage 查看缓存命中率、倍率调整后的 RMB 估算费用、六个固定时间窗口和模型/上游 token 分布，同时让 all-time 每日聚合在逐调用记录过期后继续存在。

**Blocked by:** 23 — 记录调用与模型路由尝试链

**Status:** resolved

- [x] 使用规范公式分别计算未缓存输入、缓存输入和输出费用后乘模型路由倍率；上游缺失缓存 usage 时按零，不从费用反推缓存。
- [x] 输入总量和缓存命中率按规范定义计算；费用、价格和缓存命中率都不改变路由候选或顺序。
- [x] Calls & usage 支持 `1h`、`5h`、`24h`、`7d`、`14d`、all-time，并展示发布模型 token 占比及单模型内上游供应商占比。
- [x] 费用与 token 汇总只使用可靠成功 usage；失败调用和失败路由尝试不进入聚合，缺失值不估算。
- [x] 每日聚合按发布模型和上游供应商永久保存 token 与费用，但不保留调用 ID 或尝试详情。
- [x] 精确 usage fixtures 覆盖有缓存、无缓存字段、零输入、多模型、多上游、失败与价格编辑，并证明路由命中不受 accounting 影响。

## Answer

实现（均从零构建，schema v6→v7）：

- **费用计算 OPS-006**：`compute_call_cost`（store.rs）在 `record_call` 事务内按成功尝试的路由倍率与发布模型价格计算 `(未缓存*输入价 + 缓存*缓存价 + 输出*输出价)/1e6 * 倍率`，缺失缓存按零；缺失任何输入（无 usage/无成功尝试/路由或模型已删）时保留 unknown，不反推。
- **每日聚合 OPS-009**：新表 `daily_usage(day, model, provider, tokens, cost)`，`record_call` 同事务 upsert；按 UTC 日（`civil_from_days`）分组，永不存调用 ID/尝试详情。
- **六窗口聚合 OPS-008**：`usage_totals(window, now_ms)` — `1h`..`14d` 读 `call_records`（succeeded=1 且 usage 非空），`all` 读 `daily_usage`；返回 totals（input/cached/output/cost/cache_hit_rate）与 `models[].providers[]` 两级分布。
- **14 天保留 OPS-009**：新后台任务 `spawn_call_retention_task`（server.rs）按 `created_at_ms` 删除过期调用记录（attempts 级联），每日聚合不受影响；tick 可用 `LOCAL_API_RELAY_TEST_RETENTION_TICK_MS` 加速。
- **测试时钟**：server.rs `timestamp()/timestamp_ms()` 支持 `LOCAL_API_RELAY_TEST_CLOCK_EPOCH`（与 backup.rs 同一变量），使窗口边界与保留可在进程边界观察。
- **API/UI**：`/admin/calls-usage?window=` 新增 `window` 回显与计算后 `totals`；app.js 增加窗口选择器、缓存命中率、两级 token 分布渲染（app.css 相应样式）。

新增测试（tests/secure_management_surface.rs，全部进程边界）：
- `usage_totals_and_cost_follow_the_spec_formula` — OPS-006/007：gpt-5.6-sol 1x、usage {1M, 100k, 200k} → 10.55 RMB、命中率 0.1。
- `missing_cache_usage_counts_as_zero_and_zero_input_is_safe` — 缺缓存字段按零、零输入命中率 0.0 且费用 0.02 RMB。
- `usage_distribution_breaks_down_by_model_and_provider` — 双模型/双上游分布（gpt-5.6-sol 10.0 + deepseek-v4-flash 2.0 = 12.0 RMB）。
- `six_windows_aggregate_usage_by_recency` — 测试时钟 +2 天：1h/24h 只含新调用、7d/14d/all 含两条、非法窗口回落 24h。
- `price_edits_change_future_costs_without_changing_routing` — 改价后新调用用新价、历史记录保留旧价、便宜路由（倍率 1）命中不变。
- `failed_calls_are_excluded_from_usage_aggregation` — fallback 与全失败调用均不入聚合，仅成功 usage 进入 totals。
- `daily_aggregation_outlives_per_call_records` — 时钟跳 15 天后调用记录被清理（total=0），all-time 汇总仍保留 1000 token 与费用。

验收证据：`cargo test` 56 通过 0 失败（49 存量 + 7 新增）；`cargo clippy --all-targets` 无警告；app.js 通过 `node --check`。

## Comments

### code-review follow-up（2026-08-10）

Standards/Spec 双轴 review 修复：v6→v7 迁移现在从既有成功调用记录回填 `daily_usage`（OPS-009/DATA-007，all-time 汇总不因升级丢失）；`six_windows_aggregate_usage_by_recency` 补上 5h 窗口断言；app.js 窗口选择器消费服务端 `windows` 列表，分布渲染增加模型/供应商 token 占比列（OPS-008「占比」呈现）。未采纳项：provider_name 参与 GROUP BY（快照语义合理）、f64 费用累加（信息性展示）、calls_usage 双锁读（微秒级窗口）、测试时钟与 backup.rs 复用同一变量（注释已说明耦合）。
