# 18 — 在可用模型路由集内确定性成本选路

**What to build:** 让管理员为同一发布模型配置多条模型路由并为不同中转访问密钥分配不同资格；每次 Chat 调用只在该密钥获准、协议匹配且 Available 的模型路由中按成本倍率和稳定 ID 确定性选择，不得跨模型身份替换。

**Blocked by:** 17 — 签发中转访问密钥并完成首次 Chat 调用

**Status:** resolved

- [x] Operations 支持新增、编辑和检查多个上游供应商与模型路由，包括共享 Base URL、不同上游 API key 和同发布模型多路由。
- [x] 密钥编辑流程可为每个发布模型选择精确模型路由资格，并原子验证重复关联、空资格和已失效路由。
- [x] 候选选择依次应用发布模型、原生协议、密钥资格、Available 状态、倍率升序和稳定模型路由 ID tie-breaker。
- [x] 相同输入和状态产生稳定命中；倍率编辑只改变同一发布模型内顺序，不影响其他发布模型或把费用当作绝对账单。
- [x] 缺失映射、空的获准健康候选集和未知发布模型明确失败，且不会推断用途、继承映射、比较不同上游模型名语义或跨发布模型 Fallback。
- [x] `/v1/models` 按密钥资格和当前可调用性过滤；某一路由暂不可用时，只要同发布模型仍有另一条获准 Available 路由，发布模型仍存在。
- [x] 进程边界测试覆盖多密钥、多倍率、同倍率、多模型、同 URL 供应商以及管理端事务失败后的旧配置继续生效。

Spec coverage: `SYS-005`–`SYS-006`, `API-002`–`API-003`, `CFG-002`, `CFG-005`–`CFG-013`, `ROUTE-001`–`ROUTE-002`, `UI-002`–`UI-006`.

## Comments

- 2026-08-10: Implementation started. The approved test seam is the real relay process at its loopback HTTP boundary.
- 2026-08-10: Implemented deterministic eligible routing and Operations editing. `cargo check`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `node --check src/web/app.js`, and `cargo test --all-targets --no-run` pass. The real-process deterministic-routing and administration-editing scenarios pass. The final full runtime suite and the new rollback scenario could not be retried because loopback-test escalation was rejected after the approval reviewer returned HTTP 429; all test targets compile.
- 2026-08-10: `code-review` cannot obtain a Git fixed point because `.git` is an empty read-only mount and `git rev-parse HEAD` fails. A local Standards/Spec review of the changed store, HTTP and Web workflows found no remaining issues. Commit creation is unavailable for the same reason.

## Answer

Implemented deterministic routing within each relay access key's eligible model-route set. Chat candidates now intersect the requested published model, native protocol, key eligibility and Available health state, then sort by fixed-point cost multiplier and stable route ID. Model discovery preserves only currently callable, key-authorized published models.

Operations now supports editing protected provider connections, model routes and relay-key eligibility sets, plus manual native route checks. Provider connection changes atomically mark affected routes Checking before rechecking each route; mapping/protocol changes do the same, while a multiplier-only edit preserves health and only changes that published model's candidate order. Eligibility replacement rejects empty, duplicate and nonexistent route IDs in the same transaction, preserving the previous scope on a failed commit.
