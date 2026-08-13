# 30 — 集成 Windows 登录任务与控制台启动器

**What to build:** 让 Windows 用户登录后由 per-user scheduled task 持有长期 WSL2 中转进程，并通过桌面控制台启动器检查 ready、打开默认浏览器或给出可操作诊断；Windows 和 WSL2 客户端必须访问同一个 loopback 实例。

**Blocked by:** 29 — 打包 WSL2 用户级服务生命周期

**Status:** resolved

- [x] 安装器幂等创建登录触发的 per-user Windows scheduled task，直接持有长期 `wsl.exe` serve 调用以保持 WSL2 活跃。
- [x] 登录任务不依赖 WSL systemd、不在 Windows 登录前运行，并使用有界异常重启策略；超过策略后保持失败而非无限循环。
- [x] scheduled task、进程环境和桌面启动器均不包含管理员引导凭据或其他秘密。
- [x] 桌面控制台启动器先检查专用 ready endpoint；ready 时使用 Windows 默认浏览器打开管理页面，否则显示服务状态和固定诊断命令。
- [x] Windows 与 WSL2 客户端分别以同一中转访问密钥调用相同实例，Windows 浏览器登录同一管理面，验证 WSL localhost forwarding 而不扩大 listener（浏览器登录留记录式人工验收，见 Answer）。
- [x] 记录式验收保存 Windows/WSL 版本、构建版本、地址、安装/登录/异常重启/浏览器步骤、期望和实际结果（见 Answer）。

Spec coverage: `PKG-001`, `PKG-005`–`PKG-008`, `PKG-015`, `SEC-001`, `SEC-005`.

## Answer

实现已完成。`tests/packaging_lifecycle.rs` 新增 5 个进程边界测试（13 → 18），全套 96 个测试通过、clippy 零警告；release archive 做过端到端冒烟（解包→安装→init→start→/ready 200→启动器 exit 0→stop→status 退出码 3）。本仓库不是 git 仓库，按 issue tracker 流程以本 Answer 记录。

先行的 primary-source research：`.scratch/local-api-relay-mvp/research/windows-login-task-and-console-launcher.md`（2026-08-11，每条 claim 标注 live/doc/gap）。关键一手事实：**schtasks.exe 开关模式无法表达 per-user logon trigger**（`/SC ONLOGON` 是 any-user 触发且标准用户实测被拒 0x80070005），重启策略也无开关对应（`/RI` 是重复调度、与重启无关）——因此登录任务必须走 **XML 模板 `schtasks.exe /Create /XML /F`**（UTF-16LE+BOM、经 `\\wsl.localhost` UNC 传参），per-user `LogonTrigger` + `InteractiveToken` principal 对标准用户实测可用且不存密码。

- **PKG-005 登录任务**（`packaging/install.sh`）：生成 XML 模板并注册 per-user 登录任务——`LogonTrigger`+`UserId`（非 any-user）、`InteractiveToken`/`LeastPrivilege` principal、`MultipleInstancesPolicy IgnoreNew`、`DisallowStartIfOnBatteries`/`StopIfGoingOnBatteries false`、`ExecutionTimeLimit PT0S`（serve 长驻不被时限杀）、`RestartOnFailure Count 3 / PT1M`。`cmd.exe /c whoami` 取 `DOMAIN\user`，`$WSL_DISTRO_NAME`/`$USER` 取发行版与 WSL 用户；动作 `wsl.exe -d <distro> -u <user> -- <绝对稳定入口> serve`（绝对路径、无 `~`、`-d`/`-u` 固定不受默认发行版漂移影响；wsl.exe 长驻保持 WSL2 活跃并把 serve 退出码 0-255 精确透传为 Last Result）。幂等：重复安装 `/F` 覆盖、定义不变。测试钩子：`LOCAL_API_RELAY_WINDOWS_TASK_NAME`（默认 `local-api-relay`）、`LOCAL_API_RELAY_WINDOWS_TASK_SKIP`（=1 跳过；`run_install` 默认置 1 保持既有 Linux 侧测试 hermetic）。
- **PKG-006 有界重启**：XML `<RestartOnFailure>` Count 3 / PT1M（Count 是 unsignedByte，机制天然有界；`ExecutionTimeLimit PT0S` + 有限 Count 组合下没有无限重启路径）；`InteractiveToken` 只在用户实际登录会话运行，天然满足"不在 Windows 登录前运行"；任务直接持 wsl.exe、完全不依赖 WSL systemd。research 实测：三种创建路径下失败任务 4-4.5 分钟内都不重跑、保持 Last Result failed 不循环——因此验收断言**有界配置 + 失败后保持不循环**，不断言具体重跑次数（触发条件无权威文档，见 research §10 gap 2）。
- **SEC-005**：InteractiveToken 任务 XML 不含任何密码/凭据（research 实测 SID 形式写回）；`windows_login_task_and_launcher_carry_no_credential` 测试把导出任务 XML、生成的启动器、安装脚本按 bootstrap 凭据字节扫描；既有 `bootstrap_credential_never_enters_scripts_env_or_logs` 目标列表新增启动器文件。
- **PKG-008 桌面控制台启动器**：`install.sh` 生成 `~/.local/bin/local-api-relay-launcher`（0700，bash，任务名烘焙进诊断命令）。先短探测 `/ready`（/dev/tcp，2s 超时，不等待启动）；ready → `cmd.exe /c start "" "http://127.0.0.1:<port>/"` 以 Windows 默认浏览器打开管理页（`LOCAL_API_RELAY_LAUNCHER_NO_BROWSER` 测试钩子抑制真实弹窗），否则退出码 1 并展示 `local-api-relay-service status`、`schtasks.exe /Query /TN <name> /V /FO LIST`、`wsl.exe --status` 诊断。端口与 service 脚本同规则从 service.json 读取。设计决策：启动器是 WSL 侧 bash（与既有 0700 安装树一致、进程边界可测、浏览器经 cmd start 打开），不是 Windows .cmd；后续要改任务设置不得用 `schtasks /Change`（实测会提示输密码），应 `Set-ScheduledTask -InputObject`。
- **Windows↔WSL 同实例**（PKG-001/015 可自动化部分）：`windows_loopback_reaches_the_relay_without_widening_the_listener` 用 `/mnt/c/Windows/System32/curl.exe`（本机 PATH 上 curl 是 Anaconda 的，必须显式 System32）从 Windows 打 `/ready` → 200，并以同一中转访问密钥完成一次真实 chat 调用 → 200，WSL 监听仍只绑 127.0.0.1（PKG-009 不扩宽）。
- **测试基建修复**：staging_archive 原实现每测试复制一次 101 MB debug 镜像再 strip，18 个并行测试把 /tmp tmpfs 撑爆（ENOSPC）；改为共享一次 strip 的缓存二进制 + 硬链接进各环境（16 MB，install.sh 的 `cp -l` 同款手法），默认并行度下全绿。
- **记录式验收**（spec 测试决策允许的"真实系统边界记录式人工验收"；WSL 内无法闭环的部分留人工步骤）：
  - 环境：Windows 11 Home（中文），`NT 10.0.26200.0` build 26200，主机 `LAPTOP-45RC44AF`，标准用户 `laptop-45rc44af\user`（UAC deny-only、Medium integrity）；WSL2 默认发行版 Ubuntu（`wsl --list --verbose` 标 `*`）；构建版本 `0.1.0`（`CARGO_PKG_VERSION`，release archive `dist/local-api-relay-0.1.0.tar.gz` 4.05 MB）。
  - 提交的测试断言（实际=期望）：任务 XML 合同（per-user LogonTrigger、InteractiveToken、RestartOnFailure 3/PT1M、无 Password、wsl.exe 动作、重复安装幂等）、启动器 ready/not-ready 两路径与 0700 权限、任务 XML 与启动器无凭据、Windows System32 curl `/ready` 200 + 同一访问密钥真实 chat 调用（listener 仍只绑 127.0.0.1）。
  - research 阶段实测（非提交测试，供记录）：`schtasks /Run` 成功 Last Result 0、失败 1；失败任务 4-4.5 分钟保持 failed 不循环；`wsl.exe` 长驻（`sleep 2` 实测 2.13s）与退出码 0-255 透传。
  - doc-backed + 人工步骤：默认浏览器真实弹出未在测试中执行（research gap 5），验收在人工步骤观察。
  - 待人工验收步骤：安装 release archive（真实 HOME，创建 `local-api-relay` 任务）→ `schtasks.exe /Query /TN local-api-relay /V /FO LIST`（计划类型"登陆时"、登录状态"只使用交互方式"、Last Result）→ Windows 注销/登录 → 验证 `http://127.0.0.1:8787/ready` 200 与 `local-api-relay-service status` running → 运行 `local-api-relay-launcher` 验证默认浏览器打开管理页并登录 → 强杀 serve 观察有界重启后保持失败。期望：登录即起、浏览器打开、≤3 次重启后保持失败；任何偏离记录实际结果与本构建版本。

下一个 frontier：ticket 31（版本升级与回退，PKG-013/014），已被本 ticket 解锁。
