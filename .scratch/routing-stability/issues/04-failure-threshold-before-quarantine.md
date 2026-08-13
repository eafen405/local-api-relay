# 04 — 连续失败阈值后才隔离（flap 治理）

**What to build:** REL-004。`model_route_health` 新增 `consecutive_failures`；可归因失败计数 +1，达阈值（默认 2，可配 1–5）才隔离；任何真实成功/隔离/恢复/连接编辑清零；Fallback 行为不变（单次失败仍立即转交下一候选）。

**Blocked by:** 03（同表迁移，可合并提交）。

**Status:** resolved

- [ ] store：列迁移 + 阈值设置字段 + 计数逻辑。
- [ ] server：quarantine 路径改为计数判定。
- [ ] 测试：1 次失败不隔离、连续 2 次隔离、成功清零、阈值 1 兼容旧行为。

Spec coverage: `REL-004`。

