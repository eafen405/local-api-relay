# 51 — 恢复调度可控时钟测试（ROUTE-019..021）

**What to build:** ROUTE-019..021 要求验证默认与 `N=0` 的完整间隔序列、cap 重复、成功清零和再次故障从 `B` 开始。当前间隔序列测试用 50ms 墙钟 tick + 容差断言，依赖真实经过时间，与 spec「可控时钟」的证据要求有落差（ticket 22 曾以环境 tick 钩子实现）。本 ticket 用可控时钟语义补强恢复调度的时序测试，或在验证实现确实使用可注入时钟的前提下修订 spec 测试决策以记录既有证据方式；目标是间隔序列断言不再依赖脆弱的墙钟容差。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 恢复间隔序列断言基于可注入时钟而非墙钟容差（或记录为何不可行的 spec 变更）。
- [x] 覆盖默认 `B`/`N`、`N=0`、达到上限后的 cap 重复、成功清零、新故障从 `B` 重启。
- [x] 全套现有测试保持绿。

Spec coverage: `ROUTE-019`–`ROUTE-021`.

## Answer

给恢复调度引入可注入时钟（`LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE`：测试持有的时钟文件，进程边界可推进），并把四个间隔序列测试从墙钟容差改为时钟域精确断言：

- **生产（最小接缝）**：`src/timeutil.rs` 新增 `recovery_clock_now_ms()`（文件权威；未设置回退既有时钟；畸形文件冻结为 `i64::MIN`）；`src/store.rs` 三处 schedule anchor（`update_recovery_settings`/`record_probe_result`/`quarantine_route`）与 `src/server.rs` 调度器 due 检查接入。`checked_at` 等展示时间戳仍用系统时钟。未设置环境变量时行为与之前完全一致。
- **测试**：移除 `assert_gap` 与 `timing_http_upstream` 的 `Instant` 打点；新增 `write_recovery_clock`/`route_row`/`await_route_field`；改写三个既有测试（N=2 cap 重复、N=0 恒 B、成功清零 + 新故障从 B 重启）并新增默认配置测试（B=30s/N=5 完整序列至 32B cap 重复）——全部通过 Operations 公开字段（`next_probe_at_ms`/`current_interval_ms`/`failed_probe_count`）做精确时钟域断言，负向窗口证明 probe 只在时钟越过 due 后触发。

**验证**：`cargo test` 137/137 全绿（browser 14 + packaging 29 + secure 94，`/tmp/t51-full-suite.log`）；`cargo clippy --all-targets` 零警告。双轴 code review 通过，变更记录 `/tmp/51-change-record.md`。

## Comments

- Standards 轴：无违规；judgement calls 为 `i64::MIN` 冻结哨兵（有文档、四调用点一致）、测试 setup/驱动循环重复（套件 inline-setup 惯例背书）——均不改。
- Spec 轴：checklist 三项落实；`next_probe_at_ms` 序列精确建模 `B*2^min(k,N)`；生产时钟注入是 spec「使用可控时钟验证」的必要接缝，非 scope creep。默认测试的 2s 负向窗口本身无法区分墙钟/时钟（B=30s），但精确 anchor 断言兜底；冻结语义无测试覆盖，接受。
