# 50 — Responses 取消传播测试（API-018）

**What to build:** API-018 要求真实客户端中断流后上游取消、无新尝试、健康不变，并分别捕获 Chat/Responses 上游请求证明无跨协议转换。当前取消传播只在 Chat 协议上测试，Responses 侧无对等测试。本 ticket 补 Responses 原生流式调用的取消传播进程边界测试。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] Responses 流式客户端中断后，上游请求被取消，无新候选尝试。
- [x] 取消不改变路由健康状态（健康中性）。
- [x] 测试捕获上游请求，证明是 Responses 原生协议、无 Chat 转换。
- [x] 全套现有测试保持绿。

Spec coverage: `API-018`, `ROUTE-009`.

## Answer

新增 Responses 协议版本的取消传播进程边界测试（与既有 Chat 版完全对称），无生产代码改动：

- `tests/secure_management_surface.rs` 新增 `cancellable_responses_sse_upstream()`（`cancellable_sse_upstream` 的 Responses 孪生：先响应非流式 probe，再返回 `text/event-stream` + `Connection: keep-alive` 的 Responses 原生 SSE 流，首事件 `response.created`；随后单字节读等待 relay 取消，非阻塞 accept 300ms 窗口探测新上游尝试）。
- 新增测试 `downstream_responses_cancellation_closes_the_upstream_without_changing_route_health`：真实 TCP 客户端对 relay 发 `POST /v1/responses`（`stream: true`、`input`）流式调用，读到已提交并转发的首事件 `response.created` 后断开；断言（1）上游流被取消（read 返回 0/aborted/reset）、（2）300ms 内无新上游尝试、（3）路由健康保持 `available`（ROUTE-009 健康中性）、（4）捕获的上游请求是 Responses 原生（`request_line == "POST /v1/responses"`、body 保留 `stream: true` 且有 `input`、无 Chat `messages`）——API-018 无跨协议转换在 Responses 侧的缺口闭合。

**验证**：`cargo test` 136/136 全绿（browser 14 + packaging 29 + secure 93，第二轮 `/tmp/t50-full-suite2.log`；第一轮唯一失败 `restore_reports_in_flight_stage_progress_at_the_process_boundary` 为 handoff 已记录的 WSL2 环境波动，隔离重跑通过）；`cargo clippy --all-targets` 零警告。双轴 code review 通过，变更记录 `/tmp/50-change-record.md`。

## Comments

- Standards 轴：无 documented-standard 违规；唯一 baseline smell 为 Duplicated Code（与 Chat 孪生逐行镜像），被套件既有平行-helper 惯例背书，判定为 repo 惯例覆盖 baseline，不改。
- Spec 轴：`upstream_cancelled` 非空断言、健康中性、Requests-native 断言均真实有效；probe 捕获陷阱（route-creation probe 与流式请求共享通道）处理正确。judgement-call：单候选下 300ms 无新尝试窗口只能捕获隔离重探测（fallback 结构上不可能）——与已接受的 Chat 孪生设计一致，保留。
- 弱探测与双胞胎同步维护成本记录在变更记录「双轴 code review」段。
