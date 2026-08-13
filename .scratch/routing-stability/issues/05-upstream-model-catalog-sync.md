# 05 — 上游模型清单同步（缓存 + 触发 + diff 端点）

**What to build:** REL-006。`upstream_model_cache` 持久化每个供应商的上游模型清单；触发：供应商创建/编辑连接信息后、进程启动后（后台任务）、手动 POST 刷新、周期（默认 24h，可配，0=关）；管理 API 提供 `GET /admin/providers/:id/models` 与 `POST /admin/providers/:id/models/refresh`；抓取复用轻验证的 HTTP 层。

**Blocked by:** 02（缓存表与抓取函数）。

**Status:** resolved

- [ ] store：缓存表 CRUD + 周期设置字段。
- [ ] server：同步任务 + 两个端点 + 触发接线。
- [ ] 测试：创建供应商即抓取、手动刷新、周期、抓取失败不破坏路由配置。

Spec coverage: `REL-006`。

