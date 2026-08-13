# 03 — 隔离纪元：旧探测不得恢复新隔离

**What to build:** REL-005。`model_route_health` 新增 `quarantine_epoch`；隔离时纪元 +1；`record_probe_result` 仅当请求携带的纪元与当前一致才生效，否则丢弃。

**Blocked by:** 02。

**Status:** resolved

- [ ] store：列迁移 + 隔离/记录路径带纪元校验。
- [ ] server：探测配置携带纪元，回写校验。
- [ ] 测试：隔离后到达的旧成功探测被丢弃；新纪元探测生效。

Spec coverage: `REL-005`。

