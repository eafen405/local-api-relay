# 22 — 启动检测并恢复暂不可用模型路由

**What to build:** 每次进程启动都从 Checking 重建所有模型路由的当前健康，并让暂不可用路由通过原生最小恢复检测按 capped-doubling 计划重返成本排序；管理员可从 Operations 查看完整状态并安全手工检测。

**Blocked by:** 21 — 隔离模型路由并执行提交前 Fallback

**Status:** resolved

- [x] 启动忽略持久化健康对候选选择的影响，将每条路由置为 Checking 并并发检测；服务 ready 不等待上游。
- [x] Checking 路由不进入候选集；完整有效检测进入 Available，失败进入暂不可用并开始恢复计划。
- [x] 启动与恢复检测使用路由原生协议、配置上游模型、非流式最小有效输入和最小输出，总请求少于 100 token。
- [x] Available 路由不接受周期检测；每条暂不可用路由最多一个检测在途，旧调用成功不能替代当前隔离周期检测。
- [x] 全局恢复设置支持默认 `B=30s`、`N=5` 及 `N=0`，严格执行 `B * 2^min(k,N)`、上限重复、成功清零和新故障重新开始。
- [x] Operations 显示三态数量、状态年龄、最近安全故障/检测、下次检测和当前间隔；管理员检测有 disabled/loading/success/error/retry 状态且不能输入任意 prompt 或模型。
- [x] 可控时钟和脚本上游测试覆盖重启、ready 先于检测、并发检测、完整间隔序列、手工检测、恢复重返倍率排序和无健康路由周期流量。

Spec coverage: `ROUTE-003`–`ROUTE-005`, `ROUTE-010`, `ROUTE-016`–`ROUTE-022`, `OPS-013`, `UI-007`–`UI-008`.

## Comments

- 2026-08-10: Implementation started. Test seam remains the real relay process at its loopback HTTP boundary with scripted upstreams, per the MVP Testing Decisions.
- 2026-08-10: Implemented startup recheck and capped-doubling recovery. On every start the store resets all routes to Checking synchronously before the listener binds (so persisted health can never affect candidates) and the probe task fires one native probe per route concurrently; ready never waits. Checking routes are excluded by the existing candidate SQL; a fully validated native probe moves a route to Available, any other result leaves it excluded. Startup, recovery, and manual probes all use the fixed `ping`/`max_tokens: 1` non-streaming body on the route's native protocol and configured upstream model (< 100 tokens). Available routes are never probed; the recovery scheduler fires at most one probe per unavailable route (an in-flight set shared with the admin check endpoint rejects a second one with 409). Global recovery settings (B defaults 30s, N defaults 5, N=0 allowed) are persisted and editable via `GET/PATCH /admin/recovery-settings`; a failed probe at k schedules the next at `B * 2^min(k,N)` with cap repetition, success clears the index, and a new failure restarts from B. Operations now shows three-state counts plus per-route state age, failure category, next probe time, and current interval, with a recovery settings card/panel and Check-button disabled/loading/error/retry states.
- 2026-08-10: Interpretation notes recorded after Standards/Spec review. (1) The ticket's own Operations checklist ("三态数量、状态年龄、最近安全故障/检测、下次检测和当前间隔") is fully implemented; the broader OPS-013 also lists a "安全 HTTP 状态" column, which is not part of this ticket's checklist and is left to the operations-surface work rather than adding a schema field mid-ticket. (2) "可控时钟" is realized as the settings API (B/N) plus `LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS`, the same env-tick pattern the backup tests already use; intervals are asserted against real elapsed time with a tolerance band. (3) Config edits that change a connection re-probe the route (pre-existing behavior from the configuration tickets); the recovery work itself is health-neutral on configuration corrections.
- 2026-08-10: Red-green process-boundary TDD confirmed the behavior: seven new tests plus two restart-test fixes failed against the prior binary and pass after the change (43 total). `cargo fmt -- --check`, clippy (`-D warnings`), `cargo check --all-targets`, and `node --check src/web/app.js` pass.

## Answer

Every process start discards persisted health for candidate selection: all configured routes are reset to Checking before the loopback listener binds and probed concurrently with the route's native protocol, configured upstream model, and a minimal non-streaming `ping`/`max_tokens: 1` request; the service becomes ready without waiting. A fully validated probe moves a route to Available; failure leaves it excluded and starts a recovery schedule. Available routes receive no periodic probes, and at most one recovery probe runs per unavailable route — the scheduler's in-flight set is shared with the admin's manual check, which is rejected with 409 while a probe is in flight.

Global recovery settings persist with defaults B=30s and N=5 (N=0 allowed) and are editable through `GET/PATCH /admin/recovery-settings`. After entering Temporarily unavailable the first recovery probe runs after B; the k-th failed probe schedules the next at `B * 2^min(k,N)`, the cap interval repeats, a successful probe clears the index and returns the route to multiplier ordering, and a later failure restarts from B. Operations exposes the three-state counts, per-route state age, failure category, next probe time, and current interval, plus a recovery settings card/panel; the Check interaction has disabled (Checking), loading (in-flight), success (re-rendered health), and error/retry states and only ever sends the fixed native probe.

Process-boundary tests cover restart recheck (ready before probes, concurrent probes, Checking-excluded model list), the full capped-doubling sequence and N=0 constant intervals, recovery restoring a route and re-entering multiplier ordering with a restart-from-B after a new failure, manual recovery of an unavailable route, absence of periodic traffic to Available routes, and the single-in-flight guarantee including the manual-check 409.

