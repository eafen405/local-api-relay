# 29 — 打包 WSL2 用户级服务生命周期

**What to build:** 让用户在干净 WSL2 环境中安装一个自包含、版本化的 Linux x86_64 archive，通过稳定用户级入口执行 start/stop/restart/status，并让同一进程从 XDG 目录加载数据、提供嵌入页面、捕获诊断和执行有界优雅停止。

**Blocked by:** 15 — 启动安全的本地管理面; 28 — 完成运维诊断、保留与隐私边界

**Status:** resolved

- [x] 发布 archive 包含自包含 Rust 二进制和幂等安装/生命周期脚本，不要求 package repository、root 目录、容器、Node.js、单独前端或桌面 shell。
- [x] 版本化程序并排安装，并通过稳定用户级可执行入口选择当前版本；data/config/state/log 使用规范 XDG 位置和 owner-only 权限。
- [x] 管理前端预构建并嵌入二进制，运行时不存在可能与后端漂移的独立前端目录或服务。
- [x] start、stop、restart、status 行为固定且可脚本化；浏览器不能管理进程或配置任意 shell hook。
- [x] 启动器捕获结构化 stderr 到 state 日志并遵守既定轮换；引导凭据和其他秘密不写入脚本、环境或日志。
- [x] stop/restart 先停止接受新调用，最多等待 30 秒让在途调用完成，之后取消剩余调用、关闭持久资源并退出。
- [x] 干净 WSL2 安装验收检查单进程、稳定入口、重复安装、默认/显式端口、权限、内嵌资产、生产依赖缺失和优雅停止。

Spec coverage: `PKG-002`–`PKG-004`, `PKG-007`, `PKG-009`–`PKG-012`, `SEC-005`.

## Answer

实现已完成，新增 13 个进程边界测试（`tests/packaging_lifecycle.rs`），全套 91 个测试通过、clippy 零警告；在真实 WSL2 环境对 release archive 做了端到端冒烟（解包→安装→start→/ready 200→控制台 HTML→status→restart 换 pid→stop→status 退出码 3）。本仓库不是 git 仓库（handoff 明确"无需 git"），因此按 issue tracker 流程以本 Answer 记录。

- **打包（PKG-002/003/004）**：`packaging/build-archive.sh` 产出 `dist/local-api-relay-<version>.tar.gz`（3.9 MB，仅含 release 二进制 + `install.sh` + `local-api-relay-service`）。`install.sh` 幂等：版本化程序并排装入 `~/.local/opt/local-api-relay/<version>/bin/`（0700），稳定入口 `~/.local/bin/local-api-relay` 为符号链接并原子切换，生命周期脚本装到 `~/.local/bin/local-api-relay-service`；XDG data/config/state 子目录 `local-api-relay` 全部 0700，二进制 0700；ldd 证明只依赖 glibc 基础运行时（无 libnode/libsqlite3/libssl，rusqlite bundled、rustls）。前端由 `include_str!` 嵌入（ticket 15 起），安装树无 assets 目录。
- **生命周期（PKG-007）**：`local-api-relay-service {start|stop|restart|status}`，退出码 0 运行/1 未 ready 或启动失败/2 用法/3 未运行。start 幂等（live pid 即视为已启动，即使仍在 coming up，修复 code-review 指出的启动竞态）；启动器把 serve 的 stderr 捕获到 `$XDG_STATE_HOME/local-api-relay/logs/serve.log`（0600），在 start/stop 边界按 20 MiB/日界轮换为 `serve.log.<date>[.N]`、14 天严格保留（按秒比较，修复 off-by-one）、总量 200 MiB 先删最旧（`LOCAL_API_RELAY_SERVICE_LOG_SIZE_LIMIT/_CAP` 可缩小以便进程边界测试）。浏览器面没有任何生命周期端点或 shell hook。
- **端口与 ready（PKG-009/010/011）**：默认 `127.0.0.1:8787`；`service.json` 或 `serve --port` 选稳定端口，不扫空闲端口、不静默换端口、不扩大监听。store 打开/迁移/验证 + listener 绑定后即 ready，不等上游检测。端口冲突、非法端口、非 JSON 配置、损坏库均阻塞 ready 并以非零退出（测试覆盖默认/显式端口、非法配置、端口冲突路径）。
- **有界优雅停止（PKG-012）**：`serve` 现同时处理 SIGTERM（生命周期 stop/restart 路径）与可捕获时的 SIGINT（保留交互 Ctrl+C 行为）；进程启动时若继承 SIGINT=IGN（后台作业/登录任务启动），只等 SIGTERM，避免 tokio 对 ignored 信号立即 resolve 导致启动即 drain 的 bug。停止信号到达后 axum 停止接受新连接，drain guard 在**同一时刻**起算 30 秒（`LOCAL_API_RELAY_TEST_SHUTDOWN_GRACE_MS` 可缩小）；在途调用在期限内完成则正常退出（事件 `process.stopping`/`process.stopped`），超时则取消剩余调用、关闭持久资源、仍以状态 0 退出（事件 `process.drain_expired`）。注意：30 秒是硬期限，任何在期限瞬间仍未完全拆除的连接都会被取消——这是"最多等待 30 秒"合同的固有语义。
- **启动器捕获 vs 托管轮转日志（设计决策）**：`serve.log` 的轮转发生在 start/stop 边界，因为在 shell 重定向 fd 仍打开时重命名文件会丢失后续写入（POSIX rename-while-open），唯一能对捕获流做**实时** OPS-019 轮转的是持有 fd 的进程本身——即二进制：log.rs（ticket 28）把每条 stderr 事件镜像进 `relay.log` 托管轮转日志（实时日界/20 MiB/14 天/200 MiB）。因此合同意图（stderr 进入 state 日志、有界、可轮转、owner-only）由两层共同满足：`serve.log` 为边界有界崩溃捕获镜像，`relay.log` 为实时轮转权威日志。按 AGENTS.md 的实践优先原则记录此变通，不引入 copytruncate 守护进程等屎山。
- **SEC-005**：`init-admin` 只在 stdout 打印一次引导凭据；测试扫描安装脚本、service.json、捕获日志、托管日志与运行中服务进程的 `/proc/<pid>/environ`，断言凭据不出现。
- **测试**：新增 13 个——SIGTERM 在途完成/期限取消（含 drain 事件断言）、SIGINT 优雅停止（按 `/proc/<pid>/status` SigIgn 自适应）、安装布局/幂等重复安装/自包含与 owner-only/无独立前端目录、生命周期四命令与单进程、stderr 捕获与轮转/cap、默认与显式端口、非法配置与端口冲突阻塞、凭据秘密扫描。所有测试走真实安装脚本与真实进程 loopback 边界。
- **code-review 处理**：Standards 轴（无硬违规；测试 harness 与既有文件的重复属独立 test crate 固有限制，未抽取共享 crate 以免 Speculative Generality）；Spec 轴指出的 start 幂等竞态、保留 off-by-one、SIGINT 未测已修复；`serve.log` 边界轮转与 drain 硬期限语义按上述记录为设计决策。

下一个 frontier：ticket 30（Windows 登录任务 + 控制台启动器，PKG-005/006/008），可直接复用本 ticket 的稳定入口 `~/.local/bin/local-api-relay-service` 与 `status` 退出码约定。
