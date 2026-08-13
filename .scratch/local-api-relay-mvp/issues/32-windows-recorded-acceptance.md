# 32 — Windows 记录式人工验收（ticket 30 剩余步骤）

**What to build:** 在真实 Windows 桌面完成 ticket 30 的记录式人工验收：登录任务在真实 Windows 登录时触发并拉起 serve、默认浏览器真实弹出、登录会话下进程生命周期、强杀后的有界重启。WSL 内无法闭环的部分（真实登录触发、浏览器目视与管理页登录）按 spec 测试决策（spec.md 第 253 行"属于真实系统边界，允许记录式人工验收"）留人工执行；agent 可达的机械证据已于阶段 A 完成并记录于下。

**Blocked by:** 30 — 集成 Windows 登录任务与控制台启动器（已 resolved → 本票 unblocked）

**Status:** resolved

- [x] 安装 release archive 到真实 HOME（创建正式 `local-api-relay` 登录任务）——阶段 A 已完成。
- [x] `schtasks.exe /Query /TN local-api-relay /V /FO LIST`：计划类型"登陆时"、登录状态"只使用交互方式"——阶段 A 已完成（见证据）。
- [x] Windows 注销后重新登录，验证登录任务真实触发——阶段 B 已完成（2026-08-11 21:20:59 任务触发、serve 21:21:08 起来、`/ready` 200；见阶段 B 证据）。
- [x] 运行 `local-api-relay-launcher`：Windows 默认浏览器打开管理页并登录——阶段 B 已完成（`init-admin` 凭据登录成功，进入管理界面；见阶段 B 证据）。
- [x] 强杀 serve 进程，观察登录任务在 ≤3 次有界重启后保持失败、不无限循环——阶段 A 已完成（`schtasks /Run` 真实任务路径，见证据）。
- [x] 证据已按环境、步骤、期望、实际结果与构建版本记录（阶段 A + 阶段 B，见下）。

Spec coverage: `PKG-005`–`PKG-008`, `PKG-015`, `SEC-005`（记录式人工验收侧）。

## Agent-run evidence（阶段 A，2026-08-11 21:00–21:20）

- 环境：Windows 11 Home（中文）`NT 10.0.26200.0` build 26200，主机 `LAPTOP-45RC44AF`，标准用户 `laptop-45rc44af\user`（UAC deny-only）；WSL2 默认发行版 Ubuntu，WSL 用户 `eafen405`，真实 HOME `/home/eafen405`。构建版本 `0.1.0`（`dist/local-api-relay-0.1.0.tar.gz`，4,052,858 字节）。
- 步骤与结果（期望 = 实际，除非注明）：
  1. 真实安装：解包 archive → `bash install.sh`，exit 0，输出"Windows login task: local-api-relay (per-user logon, bounded restart)"。安装树：稳定入口 `~/.local/bin/local-api-relay` → `~/.local/opt/local-api-relay/0.1.0/bin/local-api-relay`（0700），launcher/service 0700。
  2. 任务查询 `schtasks /Query /TN local-api-relay /V /FO LIST`：计划类型"登陆时"、登录状态"只使用交互方式"、状态已启用、作为用户运行 `user`、要运行的任务 `wsl.exe -d Ubuntu -u eafen405 -- /home/eafen405/.local/bin/local-api-relay serve`、初始 Last Result 267011（未运行）。
  3. 任务 XML 导出：`<LogonTrigger>` + `<UserId>LAPTOP-45RC44AF\user</UserId>`（principal 写回 SID `S-1-5-21-4053945493-3504556832-829198439-1001`）、`<LogonType>InteractiveToken</LogonType>`、`<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>`、`<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>`、`<RestartOnFailure><Count>3</Count><Interval>PT1M</Interval></RestartOnFailure>`、`<Command>wsl.exe</Command>` + `<Arguments>-d Ubuntu -u eafen405 -- /home/eafen405/.local/bin/local-api-relay serve</Arguments>`；无 `<Password`（SEC-005）。
  4. 服务启动：`local-api-relay-service start` exit 0（pid 650603）；Windows 侧 System32 curl `http://127.0.0.1:8787/ready` = 200；`status` 显示 running。
  5. 启动器：`local-api-relay-launcher` exit 0，输出 "ready — opened http://127.0.0.1:8787/ in the default browser"；浏览器进程 msedge 21 → 23（新增 2，符合默认浏览器新开页）。目视确认窗口与登录留阶段 B。
  6. **有界重启（真实任务路径）**：`schtasks.exe /Run /TN local-api-relay` → ~2s 内 `/ready` = 200；任务状态 267009（运行中）；serve pid 650948。`kill -9 650948` → 任务 Last Result 更新为 `9`（失败）；随后 **5.5 分钟轮询（11×30s）serve 计数恒为 0、任务保持已启用、Last Run Time 不再变化——无重启、不无限循环**，符合"有界、超过策略保持失败"终态（与 research 实测一致：交互任务重启不实际触发；验收以有界配置 + 保持失败为准）。
  7. 恢复：`local-api-relay-service start` exit 0（pid 651249）→ Windows 侧 `/ready` 200、根页面 200、管理页 HTML 正常（`<title>Local API Relay</title>`）。
- 备注：步骤 4-5 的浏览器窗口与"登录即起"只覆盖手动启动与 `/Run` 路径；**真实 Windows 注销/登录触发**必须由真人在锁屏/会话完成（agent 无法认证），登录管理页需要 `init-admin` 引导凭据（仅在 stdout 出现一次，建议真人执行）。

## Phase B evidence（真人，2026-08-11 21:20–21:25）

- 环境：同阶段 A；操作者：用户本人（Windows 桌面 + WSL 终端）。
- 步骤与结果（期望 = 实际，除非注明）：
  1. **登录自动启动**：Windows 注销 → 重新登录 → 未手动启动任何服务。任务 `local-api-relay` 上次运行时间 `2026/8/11 21:20:59`（登录时刻）、Last Result 267009（运行中）；serve 进程于 `21:21:08` 由任务启动（pid 456，父进程 wsl.exe 侧 452）；浏览器访问 `http://127.0.0.1:8787/ready` 返回 `{"status":"ready"}`。**登录任务真实触发、登录即起 ✅**。
  2. **`local-api-relay-service status` 盲点（发现，已修复）**：任务启动的 serve 不经 service 脚本、不写 pidfile，故 `status` 报 `stopped`，而 relay 实际在跑（监听 8787、/ready 200）。**修复**：`packaging/local-api-relay-service` 的 `status` 在无 live pidfile 时用单次 `serving_now` 探测（精确匹配 HTTP 200，stop 后保持即时返回）；任务启动的 relay 报 `running ... (login task)`、exit 0。配套测试 `status_reports_running_for_a_directly_started_serve_without_pidfile`（先验证无修复时红、修复后绿；直接启动 serve 无 pidfile → status 0/running，停止后回 3/stopped）。全套 97 测试通过、clippy 零警告；release archive 已重建（含修复脚本）并幂等重装到真实机器。
  3. **init-admin**：`local-api-relay init-admin` 输出引导凭据（仅 stdout 一次；用户已用于登录，首次登录强制改密完成）。
  4. **浏览器 + 管理面**：`local-api-relay-launcher` 打开 Windows 默认浏览器至 `http://127.0.0.1:8787/`，用引导凭据登录成功，进入管理界面（运营 / 呼叫与使用 / 已发布型号目录可见，含 深寻-V4-闪电 / GPT-5.6-SOL / GPT-5.6-地球）。**默认浏览器 + 管理面登录 ✅**。
- 偏离记录：`status` 报 stopped 与 /ready 200 不一致（见第 2 步盲点），其余期望全部达成。
- 收尾：本票全部验收项完成，`Status: resolved`。

## 执行模板（阶段 B，真人）

- 环境：Windows 11 Home（中文）`NT 10.0.26200.0` build 26200，主机 `LAPTOP-45RC44AF`；WSL2 Ubuntu；构建 `0.1.0`（已安装于真实 HOME，任务已注册）。
- 步骤：
  1. Windows 注销 → 重新登录 → **不手动启动任何服务** → 等 ~30s → `http://127.0.0.1:8787/ready` 应返回 200，`~/.local/bin/local-api-relay-service status` 应显示 running（pid 应为登录后新值）→ 记录。这是"登录任务真实触发"的直接证据。
  2. `~/.local/bin/local-api-relay init-admin` 记录引导凭据（仅 stdout 一次）→ `~/.local/bin/local-api-relay-launcher` → 目视默认浏览器打开管理页 → 用引导凭据登录（首次强制改密）→ 记录。
- 期望：登录即起（/ready 200、status running 新 pid）；启动器打开默认浏览器并成功登录管理面。
- 实际：按真实观察记录；任何偏离与期望一并记录。
- 完成后：勾掉剩余两项，本票置 `Status: resolved` 并向 `map.md` 追加决策指针。
