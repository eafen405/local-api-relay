# 15 — 启动安全的本地管理面

**What to build:** 让本地管理员能够启动一个只监听 loopback 的单进程本地中转，完成一次性管理员初始化、首次登录和凭据更换，并在浏览器中看到可工作的 Operations 空状态。该 slice 同时建立后续 tickets 共用的真实进程 HTTP 测试接缝、隔离 SQLite 状态和嵌入式 Web 资产交付方式。

**Blocked by:** None — can start immediately

**Status:** resolved

- [x] 单个 Rust 进程使用规定的运行基础，同时提供 ready endpoint、管理 API 和嵌入式 Web 页面，并只绑定默认 loopback 地址。
- [x] 服务仅在 SQLite、配置和 listener 成功打开并验证后 ready；端口冲突、非法进程配置或基础存储打开失败会安全地非零退出。
- [x] 进程遵循隔离的 XDG data/config/state 布局，创建的应用目录和含秘密文件仅当前操作系统用户可访问。
- [x] 显式 CLI 初始化只显示一次管理员引导凭据；首次浏览器登录必须更换凭据，浏览器会话与后续调用面凭据类型隔离。
- [x] 引导凭据不进入普通日志、进程环境、页面资产或 ready/status 响应。
- [x] 登录后的默认页面是 Operations 空状态，并可进入空的 Calls & usage 视图；不出现多用户、商业或工具配置管理能力。
- [x] 自动化测试从真实 loopback 进程和浏览器边界覆盖启动、ready、登录、强制改密、会话保护及失败启动。

Spec coverage: `SYS-001`–`SYS-003`, `SEC-001`, `SEC-003`–`SEC-005`, `DATA-001`, `UI-001`, `PKG-009`–`PKG-011`.

## Answer

Implemented the initial Rust service with Axum, Tokio, Reqwest, and bundled Rusqlite. The process creates private XDG state, binds only `127.0.0.1`, serves `/ready`, management endpoints, and embedded Operations / Calls & usage assets. `init-admin` creates an Argon2-hashed one-time bootstrap credential; browser sessions are opaque hashed cookies and must rotate the bootstrap credential before management access.

The real-process integration suite starts isolated loopback instances and covers ready behavior, startup failure, one-time initialization, credential rotation, session and relay-key separation, static management delivery, and local secret/file-permission boundaries. `cargo test --offline` and `cargo clippy --offline --all-targets -- -D warnings` pass.
