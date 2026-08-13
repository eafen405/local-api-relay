# 47 — Linux 可运行的 loopback/远端拒绝测试证据（SEC-001/PKG-009）

**What to build:** 验收矩阵 SEC-001–SEC-003 要求 socket 绑定检查与远端接口连接拒绝证据，PKG-009 要求默认 `127.0.0.1:8787` 稳定监听、不扩大监听地址。当前唯一的 loopback 绑定测试在非 Windows 主机上跳过（Windows localhost forwarding 场景），Linux 上没有可运行的绑定/拒绝证据。本 ticket 增加在 Linux 上可运行的进程边界测试：服务只监听 loopback、从非 loopback 地址连接被拒绝、显式端口配置不扩大监听地址。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] Linux 可运行的测试断言服务监听于 loopback，非 loopback 接口连接被拒绝。
- [x] 显式端口配置测试断言监听地址仍限 loopback，不静默换端口或扩大监听。
- [x] 测试在 Linux CI/本机全绿。

Spec coverage: `SEC-001`–`SEC-003`, `PKG-009`.

## Comments

## Answer

Linux 可运行的 loopback 绑定/拒绝证据已补齐（纯证据收集，无生产代码改动），落在 `tests/packaging_lifecycle.rs` Slice 4b：

- `service_listens_only_on_loopback_and_refuses_non_loopback_connections` — 真实安装 + `local-api-relay-service start` 后，用进程级 socket 观察（`/proc/<pid>/fd` 的 `socket:[inode]` 交叉引用 `/proc/net/tcp` + `/proc/net/tcp6` 的 state `0A` 行）断言 relay 进程全部 IPv4/IPv6 LISTEN 恰为 `127.0.0.1`（零 IPv6 监听，覆盖 SEC-001「仅绑定 127.0.0.1」完整字面合同）；从每个非 loopback 接口 IPv4（`getifaddrs`，按 `ifa_name=="lo"` 过滤，本机 eth0 `172.22.142.44`）原始 TCP connect 必须 `ECONNREFUSED`（实测确认），且 `/ready` 探针得不到 relay 200。宿主无任何非 loopback IPv4 时跳过拒绝半段（eprintln 说明），loopback 监听断言照跑。
- `explicit_port_keeps_the_listener_loopback_only_without_switching` — service.json 显式端口（≠8787）下，断言 LISTEN 恰在 `127.0.0.1:<configured_port>`、全部监听为 `127.0.0.1`、且进程无默认 8787 监听；`wait_ready(configured_port)` 的传递性证明不静默换端口。

实现要点坑：`/proc/<pid>/net/tcp` 是 namespace 级表而非进程级表（本环境有其他进程监听 `0.0.0.0:4000`），必须用 fd 的 socket inode 交叉引用才能精确观察到 relay 自己的监听。双轴 review 通过；修复包括 getifaddrs 失败 panic（不冒充「无地址」跳过）、拒绝断言收紧为 `ECONNREFUSED`（暴露并修复了 getifaddrs 字节序写反的 bug——`to_be_bytes` 把 `127.0.0.1` 转成 `1.0.0.127`，宽松断言用超时掩盖了它；正确为 `to_ne_bytes`）、监听枚举补 `/proc/net/tcp6`、`/proc` 存在性守卫对齐文件 skip 惯例。验证：packaging 29（27 旧 + 2 新）+ secure 88 = 117 全绿、exit 0、clippy 零警告。细节见 `/tmp/47-change-record.md`。
