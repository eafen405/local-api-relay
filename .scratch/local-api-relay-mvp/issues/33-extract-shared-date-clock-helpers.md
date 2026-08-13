# 33 — 抽取共享日期与时钟工具

**What to build:** 纯重构。把当前分散在多个模块里的日期与时钟工具（公历日期转换、日期键、epoch 毫秒、时钟测试钩子）收拢为一个共享实现，消除逐字重复；对外行为完全不变，全套现有测试保持绿色。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 公历日期转换（含 `civil_from_days`）在代码库中只有一处实现，重复实现被移除。
- [x] 日期键与 epoch 时钟辅助在共享模块中只有一处实现。
- [x] 时钟测试钩子使用单一共享常量，不再出现重复字面量。
- [x] 全套现有测试通过，无任何行为变化。

Spec coverage: None — pure refactor.

## Comments

- 2026-08-12: Implementation completed. New shared module `src/timeutil.rs`; the duplicated civil-date conversion, date keys, epoch clock helpers, and the test-clock environment-variable literal were consolidated into it. Behavior preserved exactly: the storage layer's session/probe/key bookkeeping stays on the system clock (matching `session_expiry`'s real-time basis), while usage windows, log day boundaries, and the backup boundary keep honoring the test clock. Full suite (78 + 27 = 105 tests) green, clippy clean.

## Answer

实现完成，纯重构。新增共享模块 `src/timeutil.rs`，把散落在 `log.rs`、`store.rs`、`server.rs`、`backup.rs` 里的日期与时钟工具收拢为单一实现，逐字重复全部消除；对外行为完全不变，全套现有测试（78 + 27 = 105）通过、clippy 零警告。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **共享模块 `src/timeutil.rs`**：
  - `civil_from_days` / `days_from_civil`（Howard Hinnant 公历转换及逆变换）——原 `log.rs` 与 `store.rs` 各一份逐字重复，现为单份。
  - `date_key(epoch_ms)`（UTC 日键 `YYYY-MM-DD`）与 `MILLIS_PER_DAY` 常量——原 `log::date_key` 与 `store::day_key` 同体、`86_400_000` 字面量在 `log.rs` 与 `date_key` 中重复，现为单份。
  - 测试时钟钩子：`TEST_CLOCK_EPOCH_VARIABLE` 常量（`LOCAL_API_RELAY_TEST_CLOCK_EPOCH`）在代码库中唯一出现，`test_clock_epoch()` 单点读取；原 `backup.rs`、`log.rs` 各自声明同值常量，`server.rs` 又内联字面量，全部消除。
  - epoch 时钟辅助：`now_epoch` / `now_epoch_ms`（遵从测试时钟，替代原 `backup::now_epoch`、`log::now_epoch_ms`、`server::timestamp` / `timestamp_ms` 的三份同体重复）与 `system_epoch_seconds` / `system_epoch_millis`（纯系统时钟，替代原 `store::timestamp` / `timestamp_ms`；`server::session_expiry` 也改用 `system_epoch_seconds() + SESSION_SECONDS`，消除最后一处 epoch 秒内联实现，语义仍为真实时间）。
- **行为保持的取舍**：store 层的会话/探针/密钥簿记时间戳保留"纯系统时钟"语义（`system_epoch_*`），与 `session_expiry` 的真实时间基准一致——若改为遵从测试时钟，固定时钟下的测试会话会被误判过期；usage 窗口、日志日界、备份边界继续经由 `now_epoch` / `now_epoch_ms` 遵从测试时钟。此取舍已由全套进程边界测试验证（含自动备份 24 小时边界、日志日界旋转、usage 窗口聚合等用例）。
