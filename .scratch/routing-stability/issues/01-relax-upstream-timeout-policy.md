# 01 — 上游超时策略可配置并与直连对齐

**What to build:** REL-001。新增"路由设置"持久化字段（首事件截止 ms、流中空闲 ms、非流式总超时 ms，默认 120000/30000/120000），管理面可编辑；`relay_precommit_fallback_loop` 与流循环消费这些值；已提交流的空闲超时改为健康中性（不隔离、不 Fallback，504 终止）。复用现有 `/admin/recovery-settings` GET/PATCH 端点（字段扩展，保持既有 B/N 字段）。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] store：settings 表新增三列 + 读写 + 迁移（schema v11 起点）。
- [ ] server：超时常量替换为运行时设置；空闲超时后提交路径健康中性。
- [ ] 管理面：路由设置面板三字段。
- [ ] 测试：慢首字/长停顿不误杀；空闲截止不隔离；配置生效；旧断言同步修订。

Spec coverage: `REL-001`。

## Answer

REL-001 已实现（schema v11→v12）：`recovery_settings` 新增 `first_event_timeout_ms`(默认120000)/`stream_idle_timeout_ms`(默认30000)/`nonstream_timeout_ms`(默认120000) 三列；`send_upstream_request` 以设置值做首事件/响应头截止，非流式响应体读取用 `nonstream_timeout_ms` 截止，流中空闲用 `stream_idle_timeout_ms`；已提交流的空闲超时健康中性（不隔离、不 Fallback，504 终止）。管理面"路由设置"面板新增三字段；PATCH 兼容旧字段（新字段可选）。`UPSTREAM_RESPONSE_HEAD_TIMEOUT`/`UPSTREAM_STREAM_IDLE_TIMEOUT` 常量移除，改为 1h 硬上限 `UPSTREAM_HARD_TIMEOUT`。

测试：`relay_times_out_when_an_sse_upstream_is_idle_before_its_first_event` 改为经 `set_relay_timeouts` 设置 1s 空闲后断言 >=1s；新增 `post_commit_stream_idle_timeout_is_health_neutral`（首事件后停顿 → 流终止但路由保持 available）；`relay_allows_a_long_stream...` 在新默认值下保持绿。schema 断言 11→12 全部同步。

