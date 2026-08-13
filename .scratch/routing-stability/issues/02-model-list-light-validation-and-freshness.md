# 02 — 轻验证（GET /v1/models）作为健康第一信号 + Available 周期保鲜

**What to build:** REL-002/REL-003。实现 `GET {base_url}/v1/models` 轻验证（带供应商 key，2xx 且清单含目标上游模型名）；启动检测先轻验证、失败才原生探测；恢复检测先轻验证、通过需原生探测确认、失败继续按倍增调度；Available 路由按可配周期（默认 10min，0=关）错峰轻验证，失败 → Checking → 原生探测裁决。

**Blocked by:** 01（共享 settings 表与迁移）。

**Status:** resolved

- [ ] store：settings 新增 freshness 周期字段；`upstream_model_cache` 建表（REL-006 共用）。
- [ ] server：`upstream_model_list()` 抓取 + `light_validate_route()`；启动/恢复/保鲜三处接入。
- [ ] 测试：轻验证通过/失败路径、模型缺失、保鲜触发与关闭、无跨供应商泄漏。

Spec coverage: `REL-002`–`REL-003`。

