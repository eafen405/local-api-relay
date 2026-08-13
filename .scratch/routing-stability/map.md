# Map: Routing Stability & Dynamic Catalog

## Destination

把本地中转打磨成"与直连几乎无差别"的多供应商模型聚合站：动态模型库、与真实可用性一致的路由健康、可配置的直连式超时、flap 治理、实时管理面 UI，并砍掉个人场景的过度设计（密钥可再显示）。

## Decisions so far

- [01 — 上游超时策略可配置并与直连对齐](issues/01-relax-upstream-timeout-policy.md) — 首事件/流中空闲/非流式截止默认 120s/30s/120s 可配；已提交流空闲超时健康中性。
- [02 — 轻验证 + 周期保鲜](issues/02-model-list-light-validation-and-freshness.md) — GET /v1/models 零成本轻验证作为启动/恢复第一信号；Available 路由默认 10 分钟错峰保鲜。
- [03 — 隔离纪元](issues/03-quarantine-epoch-guard.md) — 旧探测结果不得恢复新隔离。
- [04 — 失败阈值](issues/04-failure-threshold-before-quarantine.md) — 连续可归因失败默认 2 次才隔离（可配 1–5）。
- [05 — 上游模型清单同步](issues/05-upstream-model-catalog-sync.md) — 缓存 + 启动/周期/保鲜/手动刷新；实践修订：不在创建/编辑处理器内异步抓取（竞态与重复）。
- [06 — 发布模型 CRUD/弃用](issues/06-published-model-crud-and-deprecation.md) — 创建/弃用 + 缓存清单驱动的表单建议与一键发布。
- [07 — UI 实时刷新 + 真 modal](issues/07-operations-ui-live-refresh-and-modal.md) — 5s 局部轮询 + overlay 焦点陷阱 modal。
- [08 — 状态面/表单助手/密钥覆盖](issues/08-ui-states-consolidation-and-key-coverage.md) — 错误重试视图、submitPanelForm、防抖搜索、按模型覆盖计数与警告。
- [09 — 密钥明文可再显示](issues/09-simplify-personal-relay-key-handling.md) — SEC-006 修订：个人中转密钥 owner-only 明文持久化 + 管理面随时复制。

## Notes

- schema 11 → 17 共 6 个前向迁移（超时列、freshness、epoch/阈值、sync、deprecation、密钥明文），全部备份门控。
- schema 版本钩子（旧二进制模拟）兼容：所有新列读写按实际 schema 自适应。
- 本机沙箱环境无法创建 Windows 登录任务（schtasks 被拒），2 个 Windows 打包测试在此环境失败属环境限制，不影响真实机器。