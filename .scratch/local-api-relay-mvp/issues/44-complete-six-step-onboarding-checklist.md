# 44 — 完整六步引导清单（UI-004）

**What to build:** UI-004 要求「空或不完整配置 MUST 展示与真实控件相连的引导清单」，且清单步骤为：添加上游供应商、选择发布模型、建立显式上游模型/协议映射、设置正倍率、为中转访问密钥分配路由资格、验证并使配置可调用。当前清单只有三项（供应商、模型路由、查看健康结果），且仅在完全没有模型路由时出现——有路由但密钥尚无可用资格等「不完整但非空」的配置看不到清单。本 ticket 补齐六步清单，每步与真实控件（打开聚焦面板、编辑资格、检查路由）相连，并按完成度显示勾选状态；清单在不完整配置时可见，完整后消失。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] 引导清单包含全部六步，每步有对应的真实操作入口（面板/按钮/检查动作），不只展示文字。
- [x] 清单按完成度标记已完成的步骤；不完整配置（含非空但缺密钥资格、缺正倍率等状态）可见，完整后消失。
- [x] 浏览器或页面级测试断言六步内容与完成度流转。
- [x] 全套现有测试保持绿。

Spec coverage: `UI-004`.

## Comments

- 2026-08-12: Claimed for implementation by the implement skill（与 45 一并实现、一并 review）。
- 2026-08-12: Implementation completed. 前端 `checklistState`/`checklistMarkup` 将清单扩为 UI-004 六步，每步连接真实控件（provider/route/relay-key 聚焦面板、data-edit-route/data-edit-key、data-check-route 检查路由）；完成度按六条件计算，`operationsMarkup` 以 `checklistComplete ? "" : checklistMarkup(...)` 作为可见性门（不完整——含非空但缺密钥资格/缺正倍率/无可调用路由——可见，完整后消失）。无后端改动。新增进程边界测试 `onboarding_checklist_covers_six_steps_and_tracks_callable_completion`。
- 2026-08-12: Code review (dual-axis，非 git 场景经 `/tmp/tickets44-45-change-record.md` + 实际文件审查) 完成。**Standards 轴**：通过——无文档化标准违规（repo 无编码标准文件，clippy/rustfmt 工具强制）；baseline smells 均为 judgement call（Data Clumps：`(value, label, field)` 三元组未抽 FieldSpec——调用点可读性可接受、避免过度抽象；create/update 字段错误返回重复沿用既有成对结构；downcast 脆弱性以 FieldError 文档注明根因约束）。**Spec 轴**：通过——六步与 UI-004 一一对应、每步有真实控件、可见性门为"完整才隐藏"而非"有路由就隐藏"；两处 implemented-but-wrong 已修复：(1) callableReady 原为 `eligibilityReady && 任一路由 Available`，把"仅对不可用路由有资格但另有一条无关 Available 路由"误判为完整——改为未撤销密钥的资格集与 Available 路由求交集；(2) 撤销密钥（SEC-002 在调用面拒绝）仍计入资格——eligibilityReady/callableReady 及 Edit access key 控件改为只统计未撤销密钥。字段错误放置与变更记录不符（`closest("label, fieldset")` 命中 checkbox label 而非 fieldset）已一并修正。测试缺口（中间态流转无浏览器级断言）为 repo 无浏览器自动化接缝下的既有模式限制，清单消费的三项快照契约已钉在进程边界。验证：全套 cargo exit 0、113 测试全绿、clippy 零警告。

## Answer

实现完成（实现与 45 共享同一批文件）。清单完成度与可见性：

- 六步（UI-004 原文逐条映射）：① 添加上游供应商（Add provider，`data-open-panel="provider"`）② 选择发布模型 ③ 建立显式上游模型/协议映射 ④ 设置正倍率（②③④ 共用 Add/Edit model route 控件，`data-open-panel="route"`/`data-edit-route`，路由表单含对应字段）⑤ 为中转访问密钥分配路由资格（Create/Edit access key，`data-open-panel="relay-key"`/`data-edit-key`）⑥ 验证并使配置可调用（Check route，`data-check-route` 复用既有 per-route 检查）。
- 完成度：providerReady（providers>0）；routeReady（routes>0，含②③）；multiplierReady（routes 且全部 `cost_multiplier > 0`——覆盖"缺正倍率"的非空不完整态）；eligibilityReady（未撤销密钥任一 `model_route_ids` 非空）；callableReady（未撤销密钥的资格集与 Available 路由求交集）。
- 可见性：`operationsMarkup` 以 `checklistComplete ? "" : checklistMarkup(...)` 门控——任何一步未完成（含非空但缺资格/缺正倍率/无可调用路由）即显示，六步全完成才消失（完整后清单隐藏，路由表/密钥列表等常规管理面不受影响）。
- 测试：`onboarding_checklist_covers_six_steps_and_tracks_callable_completion` 断言嵌入 app.js 的六步文案、六类真实控件标记、`checklistState` 与可见性门标记、Done 标记；并在进程边界走查清单消费的三个快照契约（空库 providers/routes/keys → 健康路由 health `available` + cost_multiplier "1" → 密钥 model_route_ids 非空），防止 wire 字段改名静默破坏完成度流转。
- 全套测试绿（113 = secure 86 + packaging 27），clippy 零警告。
