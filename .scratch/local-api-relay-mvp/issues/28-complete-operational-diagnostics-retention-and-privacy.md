# 28 — 完成运维诊断、保留与隐私边界

**What to build:** 让管理员从 Operations 的五区状态和 14 天历史区分路由故障、存储风险、备份、迁移/恢复与 usage 完整性，同时让结构化日志和持久记录共享严格 metadata allowlist、确定保留策略和可验证的秘密/内容隔离。

**Blocked by:** 23 — 记录调用与模型路由尝试链; 25 — 暴露存储降级与 usage 缺口; 27 — 执行备份门控迁移与显式恢复

**Status:** resolved

- [x] Operations 持久展示 Storage、模型路由、备份、迁移/恢复和 usage 完整性五个独立状态区，每个异常可进入 14 天运维事件历史。
- [x] 结构化 stderr 事件覆盖进程/ready、路由转换/检测、Fallback/异常调用、存储降级/恢复、备份、迁移、恢复和日志轮换；普通成功调用不额外记录。
- [x] 每个事件只有时间、severity、稳定 event code、进程版本、correlation ID 和安全本地标识/状态；上游错误规范化且不记录完整 Base URL。
- [x] 逐调用记录、尝试和运维事件保留 14 天；当前状态、托管备份元数据和永久每日聚合不随诊断历史删除。
- [x] 捕获日志在本地日界或 20 MiB 时轮换，不保留超过 14 天的文件，总量上限 200 MiB，超限先删除最旧文件。
- [x] canary 扫描证明数据库、API、页面、事件和日志均不含请求/响应正文、prompt、tool 参数、原始错误/header/query、秘密、完整 Base URL 或备份内容。
- [x] 管理表面保持 Operations/Calls & usage/聚焦面板结构，不复制 Sub2API 品牌、页面清单、多租户、商业或原始诊断能力。
- [x] 可控时钟、大小边界、事件触发和 canary 测试覆盖保留、轮换、状态 drill-down、普通成功静默和全部隐私禁止项。

Spec coverage: `SEC-008`–`SEC-009`, `OPS-009`–`OPS-021`, `UI-001`, `UI-003`, `UI-010`–`UI-013`.

## Answer

实现已完成并通过 78 个进程边界测试（含 5 个新增 ticket-28 测试）。

- **事件持久化**：schema v10 新增 `operational_events` 表（section/severity/event_code/version/correlation_id/payload_json），由 `Store::record_operational_event` / `record_event` 写入，`GET /admin/operations/events?section=…` 分页读取（section 走 allowlist 校验），保留任务与调用记录同走 14 天清理。
- **结构化 stderr**：新模块 `src/log.rs` 每事件一行 JSON，字段固定为 `ts/severity/event/version/section/correlation/payload`；覆盖 process.start/ready、routes.check/quarantined、call.fallback/failed/stream_terminated/cancelled、storage.degraded/recovered/not_ready、backup.created/failed、migration.fresh/completed/failed、restore.completed/failed、usage.gap_opened/closed 及 logs.rotation_failed；普通成功调用静默（CallRecorder::finalize 仅在异常时发事件，correlation_id 逐调用生成）。迁移失败（前置备份失败或迁移/验证回滚）在 `Store::open` 的退出路径向 stderr 发 `migration.failed`，因为此时数据库仍是旧 schema，无法写入 v10 事件历史。
- **托管日志轮换**：`$XDG_STATE_HOME/local-api-relay/logs/` 下 `relay.log` 当日界或 20 MiB（测试可用 `LOCAL_API_RELAY_TEST_LOG_SIZE_LIMIT` 缩小）先到者轮换为 `relay.log.<date>[.N]`，按名内日期保留 14 天、总量上限 200 MiB（`LOCAL_API_RELAY_TEST_LOG_SIZE_CAP`），超限先删最旧；权限 0700/0600。
- **隐私边界**：canary 测试把唯一标记植入 prompt/tool 参数/响应正文/上游错误/header/query/上游 key/完整 Base URL/relay secret，扫描数据库（排除 SEC-007/CFG-002 故意明文的两列）、全部管理 API、页面、事件与日志，断言只出现规范化类别和安全本地标识；日志每行还断言键集合恰为 allowlist。

相关旧测试因 schema v10 更新：`downgrade_to_schema` 现只删比目标版本新的表（v9 保留 data_operations），迁移/恢复/新 schema 拒绝测试改为 v9→v10 与 v11 拒绝。
