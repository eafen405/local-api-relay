# 46 — 数据安全面板恢复阶段进度（UI-012）

**What to build:** UI-012 要求数据安全面板展示「阶段进度和可操作故障」。当前显式恢复流程只显示一个「Restoring…」标签，没有区分验证候选备份、切换数据库、重置路由重新检测等阶段。本 ticket 让恢复过程展示阶段进度（当前阶段 + 已完成阶段），失败时指出失败阶段与可操作原因。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 显式恢复期间展示分阶段进度（验证 → 切换 → 重检），不只一个静态标签。
- [x] 恢复失败时展示失败阶段与可操作原因，可回到原状态继续操作。
- [x] 页面级或浏览器测试覆盖阶段进度展示。
- [x] 全套现有测试保持绿。

Spec coverage: `UI-012`, `DATA-013`–`DATA-016`.

## Comments

## Answer

UI-012/OPS-015 的恢复阶段进度已实现。恢复 handler 通过 `spawn_blocking` 跑同步恢复，期间 `GET /admin/restore/progress`（会话绑定）上报粗粒度三阶段（verify → switch → recheck，对应 DATA-014/015 验证候选+保留当前库、原子切换、DATA-016 路由重置为 Checking）；完成/失败后阶段序列保留为 `recent` 状态 10 秒（OPS-015 current or recent stage）。前端数据安全面板在恢复中显示三阶段进度视图（当前阶段高亮 + 已完成标记，250ms 轮询），失败时展示细粒度失败阶段（映射回粗粒度阶段）+ 可操作原因 + 返回继续操作入口。测试：进程边界测试断言进行中 `restoring` 线状态 + 完成后 `recent` 完整序列 + 会话绑定 401；失败路径断言 `recent` 含失败前阶段、`last_failed_stage`/`last_failed_reason` 与表面仍可操作；静态脚本 marker 覆盖面板渲染。全套 115 测试绿、clippy 零警告。细节见 `/tmp/46-change-record.md`。
