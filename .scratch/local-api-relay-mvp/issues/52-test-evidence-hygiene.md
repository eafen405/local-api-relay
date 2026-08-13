# 52 — 测试证据卫生（发布构建、schema 夹具、DATA-017）

**What to build:** 测试证据与 spec 测试决策存在三处落差：其一，spec 要求进程边界测试用「实际发布构建」，当前打包测试安装 stripped debug 替身二进制；其二，schema 伪造/转储用 `ALTER TABLE ... DROP COLUMN` 之类 SQL 操作，超出「稳定测试检查器」许可；其三，DATA-017 无测试断言不存在 portable import/export 或跨机迁移入口。本 ticket 逐项对齐：要么改用发布构建证据并记录为何 debug 替身可接受，要么用稳定 schema 夹具取代 SQL 篡改，并补 DATA-017 的能力清单断言。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 进程边界测试使用的构建产物与 spec「发布构建」要求对齐，或记录有意的 debug 替身决策。
- [x] 数据库检查不再依赖对 schema 的 SQL 篡改（`ALTER TABLE` 等），改用稳定测试夹具/检查器。
- [x] DATA-017：断言管理 API、CLI 与页面不存在 portable import/export 或跨机迁移入口。
- [x] 全套现有测试保持绿。

Spec coverage: `DATA-017`, spec Testing Decisions（发布构建/稳定检查器）。

## Answer

逐项对齐三处证据落差，无生产代码改动：

1. **构建产物（记录决策）**：spec 测试决策「测试启动实际发布构建」追加记录在案的例外（ticket 52）：自动化套件在 `cargo test` 下运行 debug 测试构建并安装为打包替身——黑盒断言只观察外部 loopback 行为、持久化合同与安全不变量，不依赖优化/strip 差异（行为等价）；实际 release 构建由 `packaging/build-archive.sh` 产出并经记录式 Windows/WSL2 验收（PKG-015）验证。决策追溯表新增 ticket 52 行。
2. **稳定 schema 夹具**：删除 `downgrade_to_schema`（`DROP TABLE`/`ALTER TABLE ... DROP COLUMN` 篡改当前 schema），新增 `seed_schema_fixture`（固定 v8/v9 DDL 快照 `SCHEMA_V8_FIXTURE_SQL`/`SCHEMA_V9_FIXTURE_SQL`，转录自迁移链 v8 时代形态；保留 initialized 库的管理员行；内置一条已配置路由；重建 v3-v5 data-change triggers）；5 个迁移/恢复 drill 改为「initialize → seed → start」。`stamp_schema_version`（较新 schema）保留——未来 schema 形态不可知、任何夹具无法表示，元数据版本戳是唯一诚实模拟（restore 候选内容戳同理）。
3. **DATA-017 能力清单测试**：新增 `no_portable_import_export_or_cross_machine_migration_surface`——CLI `--help` 断言五命令在列且无 import/export/transfer；管理 API POST 10 个 portable-transfer 路径均 404；页面（`/`、app.js、app.css）无 portable-transfer admin 引用且无 Import/Export/Transfer UI 标签。

**验证**：`cargo test` 138/138 全绿（browser 14 + packaging 29 + secure 95，`/tmp/t52-full-suite-final.log`）；`cargo clippy --all-targets` 零警告。双轴 code review 通过，变更记录 `/tmp/52-change-record.md`。

## Comments

- Standards 轴：夹具 DDL 与迁移链 case 1-7 逐列核对一致；review 修复两处——admin 行插入移到 triggers 之前（`data_change_signal.writes` 保持 seed 值 0，与真实 v8 库一致）、DATA-017 页面扫描补 app.css 并把 `/admin/config` 收窄为 `/admin/config/import|export`。judgement calls：drill 样板重复（套件惯例）、`version: i64` 参数、夹具依赖表名（决策追溯表记录例外）、trigger 跨模块复制（快照设计固有成本）。
- Spec 轴：checklist 四项落实；`stamp_schema_version` 一致（版本戳非 schema 篡改）；restore 后重新 activate 合理（恢复的候选早于任何登录，admin 行为未动 bootstrap 状态，bootstrap 凭据可再登录）；「migrate」排除合理（check 的 PKG-013 前向迁移措辞）。残留风险记录：夹具为手工转录快照，转录错误不会被 drill 捕获（drill 只证明迁移接受该 DDL）——接受。
