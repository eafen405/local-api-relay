# 26 — 创建并轮换受保护本地备份

**What to build:** 让管理员从 Operations 数据安全面板创建和检查受保护的完整 SQLite 备份，同时让服务在数据发生变化时最多每 24 小时自动备份，并只保留最近 10 个经过验证的快照。

**Blocked by:** 16 — 配置第一条可检测模型路由

**Status:** resolved

- [x] 备份使用 SQLite online backup/snapshot API 从 WAL 数据库创建一致完整 artifact，而不是复制 live database 文件。
- [x] 备份包含配置、秘密、认证、密钥哈希、已存 usage 与健康历史，并以 owner-only 目录和 owner read/write 文件权限保护。
- [x] 数据有变化时任意 24 小时内最多自动创建一次；管理员可手工创建，创建后必须验证 SQLite 完整性和应用身份。
- [x] 只有新快照创建并验证成功后才轮换为最近 10 个；创建、验证或轮换失败不删除已有可用备份。
- [x] Operations 备份状态和数据安全面板只展示时间、触发、schema、大小、下次计划、数量与规范化失败阶段，不暴露内容或秘密。
- [x] 页面不提供云对象存储、下载、任意调度、手工删除或 portable export/import 控件。
- [x] WAL、可控时钟、权限和失败注入测试覆盖手工/自动触发、无变化、24 小时边界、验证失败和轮换保护。

Spec coverage: `DATA-009`–`DATA-012`, `DATA-017`, `OPS-014`, `UI-012`.

## Comments

- 2026-08-10: Implementation started. The test seam is the real loopback process boundary per the MVP Testing Decisions, with the existing `LOCAL_API_RELAY_TEST_*` env-var injection pattern extended by a controllable backup clock and a create/verify/rotate failure injection. The build toolchain is reused from the prior sessions at `/tmp/local-api-relay-rustup` + `/tmp/local-api-relay-cargo` (complete with all cached crates); the workspace has no git metadata, so verification and review are the completion record.
- 2026-08-10: Implemented the protected rotating backup subsystem. Backups use SQLite's online backup API from the live WAL connection into a single artifact that carries application identity and provenance (`backup_metadata`), then verify integrity, identity, and schema before rotation keeps the most recent 10. Automatic backups are gated by a persisted write counter (`data_change_signal` + per-table triggers, sessions excluded so login churn does not count as data change) and a 24-hour window driven by a controllable clock. Backup-set bookkeeping lives in an owner-only sidecar JSON so the backup flow never writes the source database.
- 2026-08-10: Process-boundary coverage passed for manual content/permissions/status, automatic trigger, the 24-hour boundary across restarts, no-change skip, verification-failure protection, rotation to 10, and rotation-failure protection. `rustfmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `node --check src/web/app.js` pass; the full suite passes with 29 tests.
- 2026-08-10: Standards/Spec review (two parallel axes) surfaced and resolved: artifact cleanup on post-verify permission/metadata failures, removal of a middle-man directory helper and its fragile error-string coupling, deduplication of the failure-recording block, a ticker that reports the real failure instead of swallowing it, and session-churn no longer counting as durable data change. The commit cannot be produced because the workspace has no `.git` (empty read-only mount, `git rev-parse HEAD` fails); the local Markdown tracker is the completion record, per the established handoff constraint.

## Answer

Implemented protected local backup creation and rotation. Backups are created through SQLite's online backup API from the WAL-backed live connection into a single complete artifact that embeds application identity (`local-api-relay`) and provenance (time, trigger, source schema), then verify SQLite integrity, identity, and schema compatibility before the managed set rotates to the most recent 10. The backup directory is owner-only (0700) and each artifact is owner read/write (0600); the full snapshot contains configuration, upstream secrets, administrator state, relay-key hashes, and stored health history. Automatic backups run at most once per any 24-hour window when durable data changed since the last snapshot — tracked by a persisted write counter (`data_change_signal` with per-table triggers, sessions excluded) and a controllable test clock — while administrators can create a verified snapshot on demand. Failures at the create, verify, or rotate stage never delete existing valid backups; each failure is recorded as a normalized stage and reason.

The Operations backups status card (OPS-014: last verified time, trigger, schema, size, next automatic backup, retained count, last failed stage/reason) opens a Data security panel (UI-012) that lists backup metadata and offers a manual "Create backup" action with busy state; it exposes no cloud storage, download, arbitrary scheduling, manual deletion, or portable export/import controls (DATA-017). Schema v4 adds `backup_metadata` (identity/provenance) and `data_change_signal`; backup-set bookkeeping lives in an owner-only sidecar JSON so the backup flow never writes the source database.

Process-boundary tests cover manual creation with artifact content/secret/permission verification against the real WAL database, automatic triggering, the 24-hour boundary across restarts with a fixed clock, no-change skip, verification-failure protection, rotation to the ten most recent snapshots, and rotation-failure protection. Verification passed: `rustfmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `node --check src/web/app.js`, and the full 29-test suite.
