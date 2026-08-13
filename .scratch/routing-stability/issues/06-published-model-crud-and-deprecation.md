# 06 — 发布模型 CRUD/弃用 + 缓存清单驱动表单

**What to build:** REL-007。发布模型支持创建（含价格）与弃用（软删除：新路由不可引用，已有路由保留）；模型路由表单上游模型名提供该供应商缓存清单下拉建议（保留手输）；Operations 展示同步 diff（新增/消失模型）并支持一键创建发布模型。

**Blocked by:** 05。

**Status:** resolved

- [ ] store：`deprecated_at` 列迁移 + create/deprecate 方法 + 引用校验。
- [ ] server：`POST /admin/published-models`、`POST /admin/published-models/:id/deprecate`。
- [ ] web：目录面板（创建/弃用）、路由表单 datalist 建议、diff 卡片。
- [ ] 测试：创建/弃用/引用拦截/表单建议。

Spec coverage: `REL-007`。

