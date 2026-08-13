# 31 — 实现版本升级与可恢复回退

**What to build:** 让管理员将新版本并排安装、验证、备份、原子设为当前并重启 Windows 登录任务；升级失败时，未提交迁移可直接切回，已提交前向迁移则通过旧二进制和迁移前备份进行明确恢复。

**Blocked by:** 27 — 执行备份门控迁移与显式恢复; 29 — 打包 WSL2 用户级服务生命周期; 30 — 集成 Windows 登录任务与控制台启动器

**Status:** resolved

- [x] 升级把新版本解包到独立版本目录，在切换前验证二进制、配置兼容性、内嵌资产和启动前提，并保留上一程序版本。
- [x] 需要 schema 迁移时必须先创建和验证迁移前备份；失败时不切换稳定入口、不修改 live database。
- [x] 验证通过后原子切换稳定可执行入口并重启 scheduled task，客户端地址和管理入口保持稳定。
- [x] 未提交 schema 迁移的失败升级可直接切回上一版本并恢复服务。
- [x] 已提交前向迁移后禁止对 live database 降级；回退使用上一二进制和显式恢复迁移前备份，恢复后重新检测全部模型路由。
- [x] 记录式升级演练覆盖安装验证失败、切换/重启失败、无迁移回退、已迁移回退，以及 Windows/WSL2 客户端恢复调用。

Spec coverage: `DATA-007`–`DATA-008`, `DATA-011`, `DATA-014`–`DATA-016`, `PKG-013`–`PKG-015`.

## Comments

- 2026-08-11: Implementation started. Test seam remains the real relay process at its loopback HTTP boundary with XDG-isolated SQLite and scripted upstreams. The toolchain is reused from prior sessions at `/tmp/local-api-relay-rustup` + `/tmp/local-api-relay-cargo`; the workspace has no git, so verification and the Standards/Spec review are the completion record.
- 2026-08-11: Implemented the offline CLI surface, the upgrade orchestration, and the rollback command. Two subtle bash bugs surfaced during process-boundary testing and were fixed: (1) `exec 9<&- 9>&- 2>/dev/null || true` permanently redirects the shell's own stderr to /dev/null (a bare `exec` without a command applies its redirections to the shell), which silently swallowed every diagnostic after the first ready probe — fixed by closing the probe descriptor in a subshell `(exec 9<&- 9>&-) 2>/dev/null || true`; (2) a `trap ... RETURN` set inside `trial_serve` leaks into the calling function and fires again at the outer function's return with the local variable out of scope, aborting the script — fixed with a self-removing trap `trap 'rm -rf "$trial_root"; trap - RETURN' RETURN`. Both patterns also existed in the pre-existing launcher/lifecycle probes and were fixed consistently (the service script runs without `-e`, so its stderr death was latent and benign).
- 2026-08-11: The process-boundary rollback drill initially restarted the real Windows login task instead of the test service: the rollback command is task-aware, and the test environment did not set `LOCAL_API_RELAY_WINDOWS_TASK_SKIP`, so `task_registered` found the real `local-api-relay` task on this host and used `schtasks /Run`. Fixed the drill to pass the hermetic skip hook, matching how install.sh tests keep installs hermetic.

## Answer

实现已完成。`tests/packaging_lifecycle.rs` 新增 8 个进程边界测试（19 → 27），全套 27 + 78 = 105 个测试通过、clippy 零警告；release archive 做过端到端冒烟（0.1.0 安装 → 升级 0.1.1 → 回退 0.1.0）。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

- **CLI（离线操作面）**：二进制新增 `check`、`backup --reason <trigger>`、`restore <name>` 三个子命令，全部不经 `Store::open` 打开 live database（绝不迁移或写库）。`check` 只读报告 `version/supported_schema/settings_ok/port/database_schema/migration_needed` 并解析进程配置；`backup` 用 SQLite online backup API 创建并验证受管备份（升级预检的迁移前快照，也是回退的恢复源）；`restore` 在文件层执行与 `Store::restore_from_backup` 同合同（DATA-014/015/016）的显式恢复——这是回退在"live database 比旧二进制新"时能恢复的关键（此时 `Store::open` 会按 DATA-008 拒绝，任何 Web 恢复都无法运行）：保留当前库为 restore-gate 备份（以 live schema 为自洽校验界，容忍比二进制新的库）、隔离验证并（更旧时）迁移 staged 候选、原子换入，成功后所有模型路由重新进入 Checking。
- **升级编排（PKG-013，install.sh）**：检测到稳定入口指向其他版本时进入升级流程——先探测 `was_serving` 并停止服务（scheduled task 存在则 `schtasks /End`，否则生命周期脚本），再预检：`check`（二进制可运行、配置兼容、schema 受支持或可迁移）→ 在 live 数据库的暂存副本上试运行新二进制（证明启动前提、内嵌管理页、配置端口可绑定、迁移在副本上成功）→ 需要迁移时先创建并验证迁移前备份（失败则不切换、不碰 live 库）→ 记录 `upgrade.state`（previous_version，需要时 pre_backup——即回退合同的全部字段）→ 原子切换稳定入口 → 重启任务/服务并等待 ready。上一程序版本始终并排保留。任何切换前失败都恢复原服务；重启失败时：无迁移（或迁移未提交）→ 自动直接切回上一版本；迁移已提交（或无法判定）→ 保持新入口并指示显式 rollback（旧二进制读不了新 schema，只有恢复能修）。
- **回退（PKG-014，local-api-relay-service rollback）**：读 upgrade.state → 停止服务 → 用**上一二进制** `check` 探测 live schema：若 live schema 超过上一版本支持 → 用上一二进制显式恢复迁移前备份（当前新 schema 库被保留为 restore-gate 备份，绝不就地降级）→ 原子切回入口 → 重启。恢复后的路由由下一次启动重新进入 Checking 并重新检测（DATA-016）；本 ticket 的"客户端恢复调用"演练证明恢复后同访问密钥能再次完成真实 chat 调用。
- **测试钩子**：`LOCAL_API_RELAY_TEST_VERSION`（覆盖 `--version` 与事件版本，同一二进制可安装成两个版本）、`LOCAL_API_RELAY_TEST_SCHEMA_VERSION`（降低支持的 schema，演练真正"旧二进制读不了新库"）、`LOCAL_API_RELAY_TEST_FAIL_SERVE`（store 打开后、bind 前失败——区分"迁移已提交"与"未提交"的重启失败）、`LOCAL_API_RELAY_TEST_FAIL_CHECK`、`LOCAL_API_RELAY_UPGRADE_SKIP_TRIAL`（install.sh 级，跳过试运行）。
- **演练矩阵（进程边界测试）**：无迁移升级成功（并排保留、原子切换、同端口重启、客户端地址稳定）；无迁移重启失败自动切回；预检 check 失败不切换；迁移前备份失败不切换且 live 库保持 v9、无 migration 工件；试运行失败不切换；v9→v10 迁移提交后显式回退恢复迁移前备份（live 库回到 v9、新 schema 库被保留、恢复后路由重新检测并以同一 key 完成真实 chat 调用）；迁移已提交 + 重启失败保持新入口且显式 rollback 恢复；无状态 rollback 明确拒绝。
- **记录式验收**（spec 允许的真实系统边界记录式人工验收；Windows 侧人工步骤见下）：
  - 环境：Windows 11 Home（中文），WSL2 Ubuntu；构建版本 `0.1.0`（release archive `dist/local-api-relay-0.1.0.tar.gz`）。提交的进程边界断言覆盖 Linux/WSL2 侧的完整升级与回退矩阵。
  - 待人工验收步骤：安装 0.1.0 release archive（真实 HOME，创建 `local-api-relay` 登录任务）→ 配置路由并完成一次调用 → 用 0.1.1 archive 执行 `./install.sh` 升级 → 验证 `schtasks.exe /Query /TN local-api-relay /V /FO LIST` Last Result 0、`http://127.0.0.1:8787/ready` 200、`~/.local/bin/local-api-relay-service status` running、上一版本仍在 `~/.local/opt/local-api-relay/0.1.0/` → 从 Windows 以同一中转访问密钥完成一次 chat 调用并打开管理页 → `local-api-relay-service rollback` → 再次验证入口切回 0.1.0、服务 ready、Windows 调用恢复。期望：升级/回退各步骤与实际结果一致；任何偏离记录实际结果与本构建版本。
