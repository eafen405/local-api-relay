# Local API Relay MVP 规范

Status: ready-for-agent

## Problem Statement

使用 Codex、KimiCode 等 agent 工具的单机用户，可能同时持有官方或第三方 OpenAI 兼容 API。当前用户必须自行比较价格、切换凭据、处理上游故障，并承担不同工具配置方式带来的重复工作。现有参考产品又混入了工具配置管理、多租户、商业计费或复杂调度能力，不适合作为一个精简、可预测的本地中转。

用户需要一个常驻本地、单人管理的中转服务：客户端只需选择一个明确的发布模型并携带中转访问密钥，本地中转就在该发布模型内部，从获准使用且当前可用的模型路由中选择成本倍率最低者；发生可归因于上游的故障时，在不破坏响应一致性的前提下 Fallback。整个过程必须保留 OpenAI 兼容协议、严格隔离秘密和内容、提供足够的本地运维证据，并且不能擅自改变客户端所请求的模型身份。

## Solution

交付一个运行在 WSL2 中的 Linux x86_64 单进程本地中转。它只监听 loopback，在同一进程内提供 `/v1/models`、`/v1/chat/completions`、`/v1/responses`、管理 API 和嵌入式 Web 管理页面。Windows 与 WSL2 中的客户端都通过稳定本地地址访问同一个服务。

管理员通过 Operations 控制台配置上游供应商、发布模型、显式模型映射、协议、成本倍率、中转访问密钥和可用模型路由集。客户端明确选择发布模型；本地中转仅在同一发布模型、同一原生协议和当前密钥允许的模型路由中进行健康过滤、成本排序与 Fallback。SQLite 保存配置、凭据、调用统计和运维状态；启动检测、恢复检测、备份、迁移、恢复、保留与脱敏规则共同保证服务可诊断且不会记录请求或响应内容。

MVP 的成功边界是：在受支持的 Windows + WSL2 环境中，用户能够完成安装和管理员初始化，从 Windows 与 WSL2 调用同一个 loopback OpenAI 兼容端点；正常请求由成本倍率最低的可用模型路由承接；可归因故障会隔离到单条模型路由并在提交响应前 Fallback；恢复检测可使路由重新参与成本排序；管理页面能完成配置、状态检查、调用与用量查看以及本地备份恢复；所有这些行为都有可重复的外部证据。

## User Stories

1. 作为本地管理员，我希望安装一个自包含的中转程序，从而无需部署数据库、容器或前端运行时。
2. 作为本地管理员，我希望服务在 Windows 登录后自动启动，从而无需每天手工保持 WSL2 进程运行。
3. 作为本地管理员，我希望使用稳定的 loopback 地址，从而可以一次配置客户端并持续使用。
4. 作为 Windows 客户端用户，我希望从 Windows 访问 WSL2 中的本地中转，从而让 Windows 原生 agent 使用同一套上游配置。
5. 作为 WSL2 客户端用户，我希望从 WSL2 访问同一中转，从而避免维护第二个实例。
6. 作为本地管理员，我希望通过一次性引导凭据完成首次登录并立即更换凭据，从而安全建立管理权限。
7. 作为本地管理员，我希望管理面与调用面使用不同凭据，从而让客户端密钥不能修改服务配置。
8. 作为本地管理员，我希望新增包含名称、Base URL 和上游 API key 的上游供应商，从而连接官方或第三方服务。
9. 作为本地管理员，我希望允许不同上游供应商使用相同 Base URL，从而分别管理不同价格或凭据的连接。
10. 作为本地管理员，我希望为发布模型显式填写上游模型名和协议，从而消除隐式映射和错误猜测。
11. 作为本地管理员，我希望分别配置 Chat Completions 与 Responses 模型路由，从而让两种协议独立选择和隔离健康状态。
12. 作为本地管理员，我希望填写正数成本倍率，从而让同一发布模型的模型路由按我的实际相对成本排序。
13. 作为本地管理员，我希望创建中转访问密钥并只看到一次完整秘密，从而能安全交给客户端且无法从存储中再次取回。
14. 作为本地管理员，我希望为每个中转访问密钥选择可用模型路由集，从而限制该客户端实际能够使用的上游连接。
15. 作为本地管理员，我希望撤销中转访问密钥，从而立即停止遗失或不再使用的客户端凭据。
16. 作为本地管理员，我希望不完整或无有效路由的配置不能变为可调用状态，从而在客户端调用前发现问题。
17. 作为首次使用者，我希望看到与真实操作相连的设置清单，从而按顺序完成供应商、映射、倍率、密钥和启用配置。
18. 作为日常管理员，我希望默认看到紧凑的 Operations 控制台，从而快速扫描路由、存储、备份和用量状态。
19. 作为日常管理员，我希望新增或编辑操作在聚焦面板中完成并返回原列表，从而保留工作上下文。
20. 作为客户端调用者，我希望使用 Bearer 中转访问密钥调用 `/v1/*`，从而不暴露上游 API key。
21. 作为客户端调用者，我希望 `/v1/models` 返回我当前可调用的发布模型，从而使用标准 OpenAI SDK 发现模型。
22. 作为客户端调用者，我希望明确请求发布模型名，从而由我或 harness 决定主任务、subagent 或 review 使用哪个模型。
23. 作为客户端调用者，我希望未知发布模型立即失败，从而不会被静默替换为另一个模型。
24. 作为客户端调用者，我希望请求中的未知字段原样转发，从而新工具能力不必等待中转升级。
25. 作为客户端调用者，我希望成功响应中的未知字段原样返回，从而保留上游协议的完整能力。
26. 作为 Chat Completions 客户端，我希望非流式和流式响应保持原生格式，从而继续使用现有 SDK 和工具调用能力。
27. 作为 Responses 客户端，我希望非流式和具名 SSE 事件保持原生格式，从而继续使用 reasoning、tools 和生命周期事件。
28. 作为流式客户端，我希望本地中转在首个有效协议事件前仍可 Fallback，从而避免收到失败路由的残缺输出。
29. 作为流式客户端，我希望中转在已经提交输出后不拼接另一条路由的生成，从而避免重复 token、冲突 ID 和错误顺序。
30. 作为取消请求的客户端，我希望取消被及时传递到上游且不触发 Fallback，从而停止不再需要的工作。
31. 作为客户端调用者，我希望无可用路由、超时和上游协议错误使用一致的 OpenAI 风格错误，从而可以稳定处理失败。
32. 作为成本敏感的用户，我希望请求优先使用倍率最低的健康模型路由，从而降低同一模型身份下的调用成本。
33. 作为成本敏感的用户，我希望相同倍率使用确定性顺序，从而获得可重复的路由结果。
34. 作为可靠性敏感的用户，我希望故障只隔离具体模型路由，从而不影响同一上游供应商的其他模型或协议。
35. 作为可靠性敏感的用户，我希望可归因于上游的单次故障立即触发隔离，从而避免继续把请求发送到已知故障路由。
36. 作为本地管理员，我希望所有模型路由在每次启动时重新检测，从而不沿用过期健康状态。
37. 作为本地管理员，我希望健康路由不接受周期性探测，从而不产生无意义的 token 消耗。
38. 作为本地管理员，我希望暂不可用路由按有上限的倍增间隔恢复检测，从而平衡恢复速度和上游压力。
39. 作为本地管理员，我希望手工触发暂不可用路由的恢复检测，从而在修正配置或上游恢复后及时验证。
40. 作为本地管理员，我希望恢复检测使用该路由的目标模型和原生协议，从而真正验证端点、凭据、协议与模型。
41. 作为本地管理员，我希望查看每条路由的当前状态、状态年龄、最近故障和下次检测，从而理解为何某条路由未被使用。
42. 作为本地管理员，我希望查看一次客户端调用对应的一条调用记录，从而不会把 Fallback 尝试误认为多次用户调用。
43. 作为本地管理员，我希望展开查看按顺序排列的模型路由尝试链，从而诊断故障与转交过程。
44. 作为注重隐私的用户，我希望调用记录和日志永不保存 prompt、响应、工具参数、原始错误正文或秘密，从而降低本地敏感数据暴露风险。
45. 作为本地管理员，我希望查看成功路由报告的 token、缓存 token、估算费用和延迟，从而了解使用情况。
46. 作为本地管理员，我希望失败调用的未知指标显示为未知而非零，从而不会把缺失数据误读为零消耗。
47. 作为本地管理员，我希望按六个固定时间窗口查看模型与上游的 token 占比，从而快速比较近期和长期使用。
48. 作为本地管理员，我希望用量缺口被明确标记且不被估算回填，从而知道统计数据何时不完整。
49. 作为本地管理员，我希望配置写入要么完整提交要么完全不生效，从而避免数据库与运行时状态不一致。
50. 作为客户端调用者，我希望统计持久化失败不推翻已经成功的响应，从而让遥测故障不影响正常调用。
51. 作为本地管理员，我希望存储降级立即出现在控制台，从而能区分上游故障与本地数据风险。
52. 作为本地管理员，我希望系统自动保留经过验证的本地备份，从而可以从数据库损坏或迁移失败中恢复。
53. 作为本地管理员，我希望迁移前必须成功备份且迁移整体事务化，从而避免半迁移状态。
54. 作为本地管理员，我希望恢复前验证候选备份并由我明确确认，从而不会自动覆盖当前数据。
55. 作为本地管理员，我希望升级保留上一程序版本和迁移前备份，从而拥有明确的回退路径。
56. 作为本地管理员，我希望服务在端口冲突、数据库损坏或不支持的 schema 下拒绝 ready，从而不会以错误或空白状态继续运行。
57. 作为本地管理员，我希望停止服务时给在途请求有限完成时间，从而兼顾正常收尾与可预测退出。
58. 作为维护者，我希望每项规范行为都有稳定需求 ID 和外部验收证据，从而实现工作无需重新打开产品决策。

## Implementation Decisions

以下决策是 MVP 的规范性合同。`MUST` 表示必须满足，`MUST NOT` 表示禁止，`SHOULD` 表示只有在有记录且不破坏合同的理由下才能偏离。

### 系统边界与领域不变量

- **SYS-001**：MVP MUST 是单用户、本地运行的本地中转；调用者选择发布模型，本地中转 MUST NOT 管理 Codex、KimiCode 或其他 harness 的配置。
- **SYS-002**：生产运行时 MUST 只有一个独立 Rust 进程；该进程同时提供 OpenAI 兼容 API、管理 API 与嵌入式 Web 管理资产。
- **SYS-003**：实现基础 MUST 使用 Axum、Tokio、Reqwest 与 bundled Rusqlite；若 Reqwest 无法满足已验证兼容案例，才可在不改变外部合同的前提下引入更低层 Hyper 代码。
- **SYS-004**：上游供应商表示一份可调用连接配置；发布模型表示客户端可见的逻辑模型身份；模型路由表示某上游供应商、上游模型名与原生协议的组合；模型映射 MUST 是模型路由上的显式字段而非独立对象。
- **SYS-005**：模型选择的用途属于调用方；本地中转 MUST NOT 根据 prompt、请求形状、密钥标签或 main/subagent/review 角色推断或替换发布模型。
- **SYS-006**：成本排序与 Fallback MUST 始终限制在同一发布模型身份内；系统 MUST NOT 推断不同上游模型名语义等价。

### OpenAI 兼容 API 合同

- **API-001**：公开调用面 MUST 仅支持 `GET /v1/models`、`POST /v1/chat/completions` 与 `POST /v1/responses`；后台 Responses 取消端点不属于 MVP。
- **API-002**：`GET /v1/models` MUST 返回 `object: "list"` 和确定性排序的 `data[]`；每项 MUST 至少包含发布模型 `id`、`object: "model"`、`created` 与 `owned_by: "local-api-relay"`。
- **API-003**：模型列表 MUST 只包含调用密钥拥有至少一条完整、协议可用且当前 Available 的可用模型路由的发布模型；Checking 或暂不可用路由不得使仍有其他 Available 路由的发布模型消失。
- **API-004**：Chat Completions 请求 MUST 是 JSON 对象，包含非空字符串 `model` 和数组 `messages`；`stream` 缺省为 `false`。
- **API-005**：Responses 请求 MUST 是 JSON 对象，包含非空字符串 `model` 和上游可接受的 `input`；`stream` 缺省为 `false`。
- **API-006**：转发层 MUST 只解析验证、映射和路由所需的 `model` 与 `stream`，并原样保留其他已知及未知请求字段。
- **API-007**：转发到上游时 MUST 将发布模型名替换为模型路由的显式上游模型名，并以选中上游供应商的 API key 替换调用方凭据。
- **API-008**：成功响应 MUST 保留上游状态、对象结构、字段与顺序语义；如响应中包含模型标识，客户端边界 MUST 呈现原发布模型名。
- **API-009**：非流式 Chat Completions MUST 在提交下游前完整读取并验证 Chat Completion JSON，且不得把消息内容或 tool calls 折叠为纯文本。
- **API-010**：流式 Chat Completions MUST 返回 `text/event-stream`，保持合法 `data:` JSON chunk 的顺序并保留终止 `data: [DONE]`。
- **API-011**：非流式 Responses MUST 保留完整 Response 对象；HTTP 2xx 但 `status` 为 `failed`/`cancelled` 或 `error` 非空时 MUST 视为可归因语义失败。
- **API-012**：流式 Responses MUST 返回 `text/event-stream` 并保持具名事件顺序；`response.completed`、`response.failed` 或 `response.incomplete` 类型化终止事件是完成判据，MUST NOT 要求 `[DONE]`，也不得转换为 Chat chunk。
- **API-013**：中转生成的所有 API 错误 MUST 使用 `{"error":{"message","type","param","code"}}` 结构；`param` 与 `code` 可为 `null`，消息必须有界且不包含秘密或原始上游正文。
- **API-014**：上游错误在候选耗尽且尚未提交下游时，SHOULD 保留最终安全的上游 HTTP 状态与 OpenAI 风格 JSON 错误；非 JSON 或不安全正文 MUST 规范化。
- **API-015**：本地中转传输错误 MUST 使用网关语义：破损或非法上游响应返回 `502`，无可用路由返回 `503`，上游超时返回 `504`。
- **API-016**：非法 JSON、缺失或未知发布模型、非法客户端字段、请求体超限以及调用认证失败 MUST 在不尝试其他上游、不改变路由健康的情况下立即返回。
- **API-017**：下游断开或取消 MUST 及时取消当前上游工作，MUST NOT 启动 Fallback，且 MUST 保持模型路由健康中性。
- **API-018**：Chat Completions 与 Responses MUST 按各自原生协议端到端转发；MUST NOT 在两种协议间转换请求、响应或流事件。

### 安全与凭据边界

- **SEC-001**：公开 API、管理 API 与 Web 管理页面 MUST 仅绑定 `127.0.0.1`；MVP MUST NOT 支持 wildcard、LAN 或远程监听。
- **SEC-002**：所有 `/v1/*` 请求 MUST 使用 Bearer 中转访问密钥认证；未认证或已撤销密钥 MUST 被拒绝。
- **SEC-003**：管理面 MUST 使用独立的单管理员凭据和浏览器会话；中转访问密钥 MUST NOT 授予任何管理能力。
- **SEC-004**：管理员初始化 MUST 由显式 CLI 命令生成并仅显示一次引导凭据；首次浏览器登录 MUST 强制替换该凭据。
- **SEC-005**：管理员引导凭据 MUST NOT 出现在登录任务、进程环境、普通日志或桌面控制台启动器中。
- **SEC-006**：中转访问密钥 MUST 只持久化可识别非秘密前缀、秘密哈希、标签、创建时间和可选撤销时间；完整秘密只在创建成功后显示一次。
- **SEC-007**：上游 API key MUST 以明文保存在仅当前操作系统用户可读写的本地数据文件中，并在管理界面以外全部遮蔽。
- **SEC-008**：普通 API 响应、管理响应、调用记录和日志 MUST NOT 暴露上游 API key、中转访问密钥、管理员凭据、Authorization header 或备份内容。
- **SEC-009**：MVP MUST NOT 添加操作系统 keychain、加密 vault、TLS 终止、用户/角色/组织或远程登录模型；未来远程访问不得反向改变此 MVP 合同。

### 配置、模型目录与路由数据

- **CFG-001**：路由与访问控制的核心持久化图 MUST 使用六类逻辑记录：上游供应商、发布模型、模型路由、中转访问密钥、路由资格和模型路由健康；管理员认证、调用/usage 和运维记录属于其他已明确的数据类别，不得混入该路由配置图。
- **CFG-002**：上游供应商 MUST 有稳定 ID、显示名、Base URL 和一个上游 API key；Base URL MUST NOT 要求唯一。
- **CFG-003**：MVP 的本地发布模型目录 MUST 只包含 `gpt-5.6-sol`、`gpt-5.6-terra` 和 `deepseek-v4-flash`，并允许管理员本地维护各自价格。
- **CFG-004**：三个发布模型的初始 RMB/百万 token 价格 MUST 分别为：`gpt-5.6-sol` 输入 5、输出 30、缓存输入 0.5；`gpt-5.6-terra` 输入 2、输出 12、缓存输入 0.2；`deepseek-v4-flash` 输入 1、输出 2、缓存输入 0.02。
- **CFG-005**：发布模型 MUST 有稳定 ID、唯一客户端可见名称以及输入、输出、缓存输入基础价格；发布模型名代表逻辑模型身份。
- **CFG-006**：模型路由 MUST 有稳定 ID、发布模型 ID、上游供应商 ID、非空上游模型名、协议和正数定点成本倍率。
- **CFG-007**：模型路由协议 MUST 仅为 `chat_completions` 或 `responses`；相同上游模型的两种协议 MUST 是健康状态独立的两条模型路由。
- **CFG-008**：模型路由身份 MUST 在 `(发布模型, 上游供应商, 上游模型名, 协议)` 上唯一；系统 MUST NOT 创建独立模型映射对象。
- **CFG-009**：路由资格 MUST 是 `(中转访问密钥, 模型路由)` 的唯一多对多关联，并直接构成该密钥对各发布模型的可用模型路由集。
- **CFG-010**：模型路由健康 MUST 是每条模型路由一条系统所有的记录；管理员可观察但 MUST NOT 作为普通配置直接编辑。
- **CFG-011**：缺失发布模型、缺失原生协议映射、不完整配置或空的获准健康候选集 MUST 明确失败；系统 MUST NOT 猜测映射、继承其他模型或选择不同发布模型。
- **CFG-012**：新增或修改供应商、模型路由、发布模型价格和密钥资格时，管理面 MUST 在提交前验证必填字段、唯一性、协议、正数倍率和至少一条有效资格关联。
- **CFG-013**：倍率调整只改变同一发布模型候选模型路由的相对顺序；MUST NOT 作为绝对账单、套餐价格或跨模型优先级解释。

### 路由、Fallback 与健康状态机

- **ROUTE-001**：每次请求 MUST 按请求发布模型与原生协议选路，与调用密钥的路由资格求交集，移除非 Available 路由，再按成本倍率升序排列。
- **ROUTE-002**：成本倍率相同的候选 MUST 使用稳定模型路由 ID 作为确定性 tie-breaker。
- **ROUTE-003**：每条协议特定模型路由 MUST 且只能处于 Checking、Available 或暂不可用之一。
- **ROUTE-004**：每次进程启动 MUST 忽略持久化健康状态对候选选择的影响，将所有已配置模型路由置为 Checking，并并发执行启动检测。
- **ROUTE-005**：服务 ready MUST NOT 等待启动检测；Checking 路由不得进入候选集，检测成功进入 Available，失败进入暂不可用。
- **ROUTE-006**：Available 路由发生一次可归因于该路由的失败后 MUST 立即进入暂不可用；MUST NOT 使用连续失败计数、错误率窗口或阈值。
- **ROUTE-007**：可归因失败 MUST 包括 DNS、TLS、连接、转发、响应读取、上游流故障，连接/响应/流空闲超时，以及上游 `401`、`403`、`404`、`429`、`5xx`。
- **ROUTE-008**：格式错误、截断、协议非法的上游响应以及明确可归因上游的 Responses 语义失败 MUST 触发隔离。
- **ROUTE-009**：非法客户端请求、认证失败、请求体超限、下游取消及 allowlist 以外的上游 `4xx` MUST 健康中性且不得触发 Fallback。
- **ROUTE-010**：进入暂不可用后，旧在途请求的成功 MUST NOT 恢复路由；在当前隔离周期内，只有该周期的专用恢复检测成功才能恢复。连接相关配置编辑使既有隔离证据失效并结束当前隔离周期：编辑供应商连接（Base URL 或 API key）或模型路由的连接类字段（供应商、上游模型名、协议）后，受影响路由 MUST 重新进入 Checking 并由系统所有权重检以同一原生探测决定新健康（等同启动检测语义，见 ROUTE-016），不视为对 ROUTE-010 的违反；非连接类编辑（显示名、价格、成本倍率、密钥资格）MUST 保持健康状态与隔离周期不变。
- **ROUTE-011**：在任何下游响应被提交前，每次可归因失败 MUST 隔离当前模型路由并尝试原始有序候选集中的下一条 Available 路由。
- **ROUTE-012**：非流式请求 MUST 在提交下游前读取并验证完整上游响应，因此状态错误、读取失败、非法正文或 Responses 语义失败可触发 Fallback。
- **ROUTE-013**：流式请求 MUST 在提交下游 header 或 body 前读取并验证首个原生协议事件；首事件阶段失败可 Fallback，成功首事件必须无损转发。「无损」界定为语义无损：当上游模型名与发布模型名一致时 MUST 逐字节透传原始事件；需要模型名替换时（API-008），重写路径 MUST 保留对象结构、全部已知及未知字段与字段顺序，仅替换 `model` 值，且编码变换仅限语义等价形式（SSE 多行 `data:` 按规范以换行折叠、JSON 字符串转义规范化），数值在 `serde_json` 可表示范围内精确往返（`i64`/`u64` 精确、`f64` 最短往返）；字面精度超出可表示范围的数值四舍五入到最近可表示值，属已记录的可接受边界。
- **ROUTE-014**：流式响应一旦提交，后续可归因失败 MUST 隔离路由并终止下游流；MUST NOT 拼接其他模型路由的生成，也不得在同一次调用中重试。
- **ROUTE-015**：所有候选失败后 MUST 返回最终规范化错误；已经提交的流只能终止，不能改写为新的 JSON 错误响应。
- **ROUTE-016**：启动与恢复检测 MUST 使用模型路由的原生协议、配置的上游模型、最小有效输入和最小支持输出量，并使用非流式、完整、协议有效的成功作为唯一通过条件。
- **ROUTE-017**：每次检测请求 MUST 以最小化 token 为目标且总请求少于 100 token；MVP MUST NOT 提供元数据-only 或不消耗 token 的检测选项。
- **ROUTE-018**：Available 路由 MUST NOT 被周期性检测；每条暂不可用路由最多同时运行一个恢复检测。
- **ROUTE-019**：恢复检测的全局基础间隔 `B` MUST 默认为 30 秒；倍增上限 `N` MUST 默认为 5，并允许零或正整数。
- **ROUTE-020**：首次恢复检测在 `B` 后执行；第 `k` 次失败后，下次间隔 MUST 为 `B * 2^min(k,N)`，达到上限后持续使用上限间隔；`N=0` 时始终使用 `B`。
- **ROUTE-021**：恢复检测成功 MUST 将路由转为 Available、清除失败检测索引并重新参与倍率排序；之后的新故障 MUST 从基础间隔重新开始。
- **ROUTE-022**：管理员触发的恢复操作 MUST 使用同一原生协议检测语义，只能报告允许的安全元数据，且不得允许任意 prompt 或任意目标模型。

### 持久化、迁移、备份与恢复

- **DATA-001**：bundled SQLite MUST 是唯一持久化存储，并启用 foreign keys、WAL 与 full durable commit；单进程内 MUST 串行化写入，读取只能观察一致快照。
- **DATA-002**：上游供应商、发布模型及价格、模型路由、中转访问密钥及资格、管理员认证和恢复设置的每次变更 MUST 在一个事务中应用全部相关记录。
- **DATA-003**：运行时配置 MUST 只在数据库事务提交后发布，管理客户端也只能在提交后收到成功；事务失败 MUST 同时保留旧数据库状态和旧运行时配置。
- **DATA-004**：调用用量、估算费用、模型路由健康历史与运维事件属于可降级的运维记录；可靠事实可用时应事务化持久化，但写入失败 MUST NOT 使已经成功的中转响应失败或失效。
- **DATA-005**：健康状态转换 MUST 立即在内存中生效；相关持久化失败 MUST 产生可见 Storage Degraded 信号，且系统 MUST NOT 为中断流或缺失 usage 发明统计。
- **DATA-006**：数据库 MUST 保存单一整数 schema 版本；二进制 MUST 携带有序、仅前向迁移，MUST NOT 提供 downgrade migration 或迁移 UI。
- **DATA-007**：打开旧 schema 时，服务 MUST 先创建并验证一致完整备份，再在一个事务中运行全部迁移和 schema 版本更新，最后验证通过后才进入 ready。
- **DATA-008**：迁移前备份失败 MUST 阻止迁移；迁移或验证失败 MUST 回滚、保留旧数据库并阻止 ready；高于二进制支持版本的 schema MUST 拒绝写入且不得自动降级。
- **DATA-009**：备份 MUST 使用 SQLite online backup/snapshot API，而不是复制 live database 文件；备份 MUST 包含配置、usage、中转密钥哈希、管理员状态、上游 API key 及已存健康历史。
- **DATA-010**：备份目录和备份文件 MUST 仅当前操作系统用户可访问；全部备份均视为含秘密，普通日志与管理响应只能呈现安全元数据。
- **DATA-011**：持久数据自上次计划快照后有变化时，系统 MUST 在任意 24 小时内最多创建一次自动备份；迁移前、显式恢复前 MUST 创建备份，并允许管理员手工创建。
- **DATA-012**：新快照创建并验证成功后，系统 MUST 将托管集合轮换为最近 10 个备份；验证失败不得删除已有可用备份。
- **DATA-013**：数据库损坏时系统 MUST 保留失败数据库作为恢复证据、保持 not ready 并要求管理员显式选择备份；MUST NOT 自动恢复或静默创建空数据库。
- **DATA-014**：恢复 MUST 在隔离候选数据库中验证 SQLite 完整性、应用身份和 schema 兼容性；较新 schema 必须拒绝，较旧 schema 必须按同一备份门控迁移合同升级。
- **DATA-015**：恢复切换前 MUST 保留当前数据库；只有候选通过全部检查才能替换，切换前任一失败 MUST 保持当前数据库为选中状态。
- **DATA-016**：恢复成功 MUST 保留备份中的配置、秘密、密钥哈希和 usage 历史，但 MUST 丢弃恢复健康状态对路由的影响，并让所有模型路由重新进入 Checking。
- **DATA-017**：MVP MUST NOT 提供可移植配置导入导出或跨机器迁移格式；完整本地备份和显式恢复是唯一数据转移机制。

### 调用记录、用量与运维诊断

- **OPS-001**：系统 MUST 为每次下游客户端调用保存至多一条 metadata-only 调用记录，即使发生 Fallback 或全部候选失败也不得拆成多条调用记录。
- **OPS-002**：成功调用记录 MUST 包含调用时间、发布模型、最终成功上游供应商、该成功模型路由报告的总 token 及缓存 token、估算费用、完成耗时，流式调用另含首字耗时。
- **OPS-003**：Fallback MUST 以调用记录下可展开的有序模型路由尝试链呈现；每次尝试只包含顺序、模型路由和上游供应商 ID、开始时间、耗时、安全 HTTP 状态、规范化故障类别、下游提交阶段及 Fallback/流终止结果。
- **OPS-004**：token 与费用 MUST 只使用最终成功模型路由可靠报告的 usage；此前失败尝试 MUST NOT 纳入，系统也不得假定其免费或估算缺失用量。
- **OPS-005**：全部候选失败时，调用记录 MUST 标记无成功上游，并将 token、费用、首字耗时和完成耗时显示为 `-` 而非零；失败调用 MUST 排除在 token 与费用聚合之外。
- **OPS-006**：费用估算 MUST 使用 `(未缓存输入 * 输入价 + 缓存输入 * 缓存输入价 + 输出 * 输出价) / 1,000,000 * 成本倍率`；上游未报告缓存输入时按零处理。
- **OPS-007**：输入总量 MUST 等于未缓存输入加缓存输入；缓存命中率 MUST 等于缓存输入除以输入总量；估算费用与缓存命中率 MUST NOT 影响路由选择。
- **OPS-008**：Calls & usage MUST 支持 `1h`、`5h`、`24h`、`7d`、`14d` 和 all-time 六个窗口，并展示发布模型 token 占比及单个发布模型内的上游供应商 token 占比；费用仅作为信息并列显示。
- **OPS-009**：逐调用记录、模型路由尝试和运维事件 MUST 保留 14 天；按发布模型与上游供应商汇总的每日 token 和估算费用 MUST 永久保留且不得含逐调用标识。
- **OPS-010**：Operations 控制台 MUST 持久展示六个独立状态区：Storage、模型路由、备份、迁移与恢复、usage 完整性、恢复设置；每个异常状态 MUST 可进入其 14 天事件历史。恢复设置状态区为常驻展示区，展示基础间隔 `B` 与倍增上限 `N`（ROUTE-019）并可进入恢复设置面板（DATA-002）。
- **OPS-011**：Storage 状态 MUST 呈现 Healthy、Degraded 或 Not ready、状态开始时间、受影响记录类别、规范化持久化错误、已知丢失记录数或 unknown，以及相关 accounting gap 的起止。
- **OPS-012**：相同记录类别重新成功持久化且 SQLite 轻量完整性检查通过后，当前 Storage Degraded 状态才可自动清除；历史事件和既有 usage gap MUST 保留。
- **OPS-013**：模型路由状态 MUST 展示三态数量；路由行 MUST 展示状态年龄、最近检测或可归因故障时间与类别、安全 HTTP 状态、下次恢复检测时间和当前倍增间隔。
- **OPS-014**：备份状态 MUST 展示最近验证备份的时间、触发方式、schema 版本和大小、下次自动备份时间、保留数量，以及最近失败阶段与规范化原因。
- **OPS-015**：迁移/恢复状态 MUST 展示运行与支持 schema 版本、前置备份结果、当前或最近阶段、验证结果、完成时间和 not-ready 的可操作原因；恢复出的健康历史不得显示为当前健康。
- **OPS-016**：Usage 完整性 MUST 明确标记所选区间是否完整，并列出缺失上游 usage 或本地持久化失败造成的已知 gap；MUST NOT 估算或回填 gap。
- **OPS-017**：结构化诊断 MUST 写到标准错误并由安装启动器捕获；范围包括进程与 ready、路由转换与检测、Fallback 和异常调用、存储降级/恢复、备份、迁移、恢复及日志轮换失败。
- **OPS-018**：普通成功调用 MUST NOT 额外生成结构化日志事件；每个必要事件 MUST 含时间、severity、稳定 event code、进程版本、相关本地 correlation ID 和 allowlist 中的安全标识与状态字段。
- **OPS-019**：捕获日志 MUST 在本地日历日边界或达到 20 MiB 时轮换，以先到者为准；不得保留超过 14 天的文件，托管日志总量 MUST 不超过 200 MiB，超限时先删最旧文件。
- **OPS-020**：诊断面——调用记录、运维事件、日志与状态区——MUST 使用字段 allowlist，MUST NOT 存储或渲染请求/响应正文、prompt、tool 参数、原始上游错误正文、原始 header、query string、完整 Base URL、任何秘密或备份内容。管理聚焦编辑面板是保留完整 Base URL 的唯一例外（CFG-002 编辑上游供应商所需）；管理列表/只读表面 MUST NOT 渲染完整 Base URL，只允许遮蔽或截断形式。
- **OPS-021**：上游故障 MUST 归一化为稳定类别和安全的本地描述；上游只能通过非秘密本地 ID 与显示名识别。

### Web 管理工作流

- **UI-001**：Web 管理面的默认视图 MUST 是 Operations；第二个持久主视图 MUST 是 Calls & usage，其他配置和数据安全能力应作为聚焦面板或对话框进入。
- **UI-002**：Operations MUST 使用按发布模型分组的紧凑模型路由表，展示上游供应商、上游模型、协议、倍率、健康、最近故障/检测和下次检测。
- **UI-003**：Operations MUST 同时提供上游供应商、发布模型/模型路由和中转访问密钥的新增、编辑、检查及撤销入口，但不得引入 Sub2API 的 Accounts、Groups 或 Channels 领域对象。
- **UI-004**：空或不完整配置 MUST 展示与真实控件相连的引导清单：添加上游供应商、选择发布模型、建立显式上游模型/协议映射、设置正倍率、为中转访问密钥分配路由资格、验证并使配置可调用。
- **UI-005**：新增或编辑供应商、模型路由和中转访问密钥 MUST 在聚焦面板中完成，保存或取消后回到原 Operations 上下文。
- **UI-006**：不完整映射、非法倍率或没有有效路由资格的密钥 MUST 在管理面给出字段级、可操作反馈，并不得变为可调用配置。
- **UI-007**：模型路由行 MUST 将健康显示为系统所有状态，并在适用时提供配置修正或恢复检测；配置修正本身 MUST 健康中性——编辑动作 MUST NOT 直接设置健康，健康只能由系统所有权重检决定（连接相关编辑后的 Checking 重检、启动检测或专用恢复检测）。
- **UI-008**：恢复检测交互 MUST 有 disabled/loading/success/error/retry 状态，并只展示规范允许的安全结果。
- **UI-009**：中转访问密钥列表 MUST 可搜索、显示标签/前缀/状态/范围；创建时显示一次完整秘密，撤销必须显式确认，之后不得提供复制完整秘密功能。
- **UI-010**：Calls & usage MUST 将固定时间窗汇总、token 分布和可分页调用表置于同一工作面；调用行 MUST 以发布模型为主标签、成功上游供应商为次标签。
- **UI-011**：调用行进入详情时 MUST 在原视图内展示 metadata-only 模型路由尝试链，而不是跳转到原始请求、响应或错误内容。
- **UI-012**：数据安全面板 MUST 从 Operations 状态区进入，展示安全备份元数据、手工备份、显式恢复选择、阶段进度和可操作故障；MUST NOT 提供云备份、下载、任意删除或可编辑调度。
- **UI-013**：Sub2API 仅作为高密度表格、状态、聚焦操作和 summary-to-detail 的交互证据；MUST NOT 复制其品牌、视觉样式、页面清单、多租户或商业能力。

### 打包、启动与生命周期

- **PKG-001**：MVP MUST 支持单一主拓扑：Linux x86_64 中转运行在 Windows 主机的 WSL2 内，Windows 原生客户端、WSL2 客户端和 Windows 默认浏览器访问同一 loopback 服务。
- **PKG-002**：MVP MUST 发布包含 Rust 二进制和幂等安装/生命周期脚本的自包含版本化 Linux x86_64 archive；生产环境不得要求包仓库、root 系统目录、容器、Node.js 或桌面 shell。
- **PKG-003**：版本化程序文件 MUST 并排安装，并通过稳定的用户级可执行入口选择当前版本；数据、进程配置及运行状态/日志 MUST 分别遵循 XDG user data、config 和 state 目录，应用目录名为 `local-api-relay`。
- **PKG-004**：所有本地应用目录及含秘密文件 MUST 仅当前操作系统用户可访问；管理前端 MUST 在发布前构建并嵌入 Rust 二进制。
- **PKG-005**：安装器 MUST 创建按 Windows 用户登录触发的 per-user scheduled task，直接持有长期 `wsl.exe` 调用以运行 `local-api-relay serve` 并保持 WSL2 活跃。
- **PKG-006**：登录任务 MUST 使用有界异常退出重启策略，不依赖 WSL systemd，也不得在 Windows 登录前运行；超过策略后 MUST 保持失败而不是无限重启。
- **PKG-007**：生命周期接口 MUST 提供固定的 `start`、`stop`、`restart`、`status` 命令；浏览器 MUST NOT 管理进程生命周期或配置任意 shell hook。
- **PKG-008**：桌面控制台启动器 MUST 先检查专用本地 ready endpoint；ready 时以 Windows 默认浏览器打开管理页面，否则展示服务状态和可操作诊断命令。
- **PKG-009**：默认监听 MUST 稳定为 `127.0.0.1:8787`；显式配置可选择另一稳定端口，但进程 MUST NOT 扫描空闲端口、静默换端口或扩大监听地址。
- **PKG-010**：服务 MUST 在持久存储与配置成功打开、迁移、验证且 loopback listener 成功绑定后进入 ready；MUST NOT 等待上游启动检测。
- **PKG-011**：数据库损坏、较新且不支持的 schema、备份门控迁移失败、非法进程配置或端口冲突 MUST 阻止 ready 并以非零状态退出。
- **PKG-012**：停止或重启时，服务 MUST 停止接受新调用并最多等待 30 秒让在途调用完成；超时后取消剩余请求、关闭持久资源并退出。
- **PKG-013**：升级 MUST 将新版本并排解包并验证，遵守迁移前备份合同，原子切换稳定可执行入口并重启登录任务；MUST 保留上一程序版本。
- **PKG-014**：未提交 schema 迁移的升级失败可直接切回上一版本；前向迁移已提交后，回退 MUST 使用上一二进制配合显式恢复迁移前备份，MUST NOT 降级 live database。
- **PKG-015**：发布验收 MUST 从 Windows 和 WSL2 分别调用中转，并从 Windows 打开管理页面，以验证 WSL localhost forwarding、浏览器启动与同实例访问。

## Testing Decisions

### 测试原则与接缝

- 唯一主要自动化测试接缝是**真实本地中转进程的 loopback HTTP 边界**。测试启动实际发布构建，使用隔离的 XDG 目录和 SQLite 数据库，并连接可编排的 OpenAI 兼容上游；通过公开 API、管理 API、Web 页面和生命周期命令观察结果。记录在案的例外（ticket 52）：自动化套件在 `cargo test` 下运行 debug 测试构建并安装为打包替身，因为黑盒断言只观察外部 loopback 行为、持久化合同与安全不变量，不依赖优化或 strip 差异（行为等价）；实际 release 构建由 `packaging/build-archive.sh` 产出并经记录式 Windows/WSL2 验收（PKG-015）验证。
- 可编排上游必须能确定性产生非流式成功、Chat SSE、Responses SSE、各类 HTTP 状态、非法 JSON、截断正文、首事件前失败、提交后失败、空闲超时、取消和 usage 变体。测试不得依赖 Rust 私有函数、内部 struct 布局或数据库表名。
- 数据库检查仅用于验证外部不可直接观察的持久化不变量，例如 secret 未明文存储、事务原子性、schema 版本和保留结果；此类检查应通过稳定的测试检查器读取隔离数据，而不是断言实现查询顺序。
- Web 管理流程使用浏览器自动化从真实服务进入，验证用户可见状态、校验、创建/撤销、探测和 drill-down。视觉参考不是像素克隆要求。
- Windows/WSL2 安装、Windows localhost forwarding、默认浏览器、scheduled task、升级与恢复演练属于真实系统边界，允许记录式人工验收；每次证据必须包含环境、步骤、期望、实际结果和构建版本。
- 好测试只断言外部行为、持久化合同和安全不变量，不断言路由函数拆分、task 数量、SQL 语句形状或前端组件树。

### 被测能力与既有证据

- OpenAI 兼容入口、管理入口、路由内核、健康与恢复调度、SQLite 数据保护、调用/用量/日志投影、Web 管理流程、安装和生命周期均通过上述进程边界测试。
- 当前项目尚无产品实现测试。CC Switch 的 circuit breaker、首事件 priming、取消和错误策略测试可作为场景 prior art，但其 provider/application 领域模型不是规范。Sub2API 只提供管理交互 prior art，不提供行为、数据或隐私断言。

### 验收与证据矩阵

| Requirement IDs | Repeatable evidence |
| --- | --- |
| `SYS-001`–`SYS-002` | 构建产物检查和进程验收证明只有一个服务进程同时提供调用、管理和静态页面；客户端配置文件在调用前后保持不变。 |
| `SYS-003` | 依赖清单与发布构建检查；若引入 Hyper 路径，附对应 Reqwest 兼容失败案例及相同 HTTP 边界回归。 |
| `SYS-004`–`SYS-006` | 通过管理 API 创建同 URL 多供应商、同发布模型多路由和不同发布模型；调用断言只在显式同身份组内选路且不推断用途或名称等价。 |
| `API-001`–`API-003` | 带不同资格/健康状态的密钥请求模型列表；断言唯一路径集合、标准 `data[]` 结构、确定顺序和按可调用性过滤。 |
| `API-004`–`API-005` | 对两个 POST 端点分别提交最小有效、缺字段、错误类型和默认 `stream` 请求；记录上游是否被调用及下游错误。 |
| `API-006`–`API-008` | 在请求与响应中加入未知、tools、reasoning、多模态和 metadata 字段；脚本上游捕获转发体，客户端断言字段保留、凭据/模型替换及公开模型名恢复。 |
| `API-009`–`API-010` | 非流式与 Chat SSE 合约测试验证完整 JSON、tool calls、chunk 顺序、内容类型和 `[DONE]`。 |
| `API-011`–`API-012` | Responses 非流式和具名 SSE 合约测试覆盖完整对象、2xx semantic failure、事件顺序及三种类型化终止，不接受 Chat 转换。 |
| `API-013`–`API-016` | 错误表驱动测试覆盖安全上游 JSON、HTML/纯文本错误、broken response、无路由、超时、非法客户端请求和超限体；断言 envelope、状态码、无 fallback 与无秘密。 |
| `API-017`–`API-018` | 真实客户端中断流并观察上游取消、无新尝试和健康不变；分别捕获 Chat/Responses 上游请求证明无跨协议转换。 |
| `SEC-001`–`SEC-003` | socket 绑定检查、远端接口连接拒绝、调用/管理凭据交叉使用矩阵和撤销密钥请求，证明 loopback 与权限隔离。 |
| `SEC-004`–`SEC-006` | 初始化 CLI 与首次浏览器登录流程；检查只显示一次、强制替换、哈希存储及后续页面无法恢复完整中转密钥。 |
| `SEC-007`–`SEC-009` | 文件权限和隔离数据库检查，加上 API/页面/日志秘密扫描；发布依赖与功能清单确认无 keychain、vault、TLS、远程或多用户能力。 |
| `CFG-001`–`CFG-002` | 通过管理面建立最小配置和两个共享 Base URL 的上游供应商；重启后读取相同行为，稳定检查器验证六类逻辑记录关系。 |
| `CFG-003`–`CFG-005` | 空库初始化和价格编辑/重启测试断言三个模型目录、精确初始 RMB 价格、唯一名称与持久化更新。 |
| `CFG-006`–`CFG-010` | 配置验证矩阵覆盖空模型名、非法协议、非正倍率、重复路由、双协议独立健康、重复资格和禁止手改健康。 |
| `CFG-011`–`CFG-013` | 缺失模型/映射/资格/健康候选调用测试和倍率调整测试，断言明确失败、无替代模型及仅同模型顺序变化。 |
| `ROUTE-001`–`ROUTE-002` | 多密钥、多倍率、同倍率及混合健康候选表驱动测试，捕获每次命中的上游并验证交集、排序和稳定 tie-breaker。 |
| `ROUTE-003`–`ROUTE-005` | 预置旧健康记录后重启；延迟脚本探测并并发访问 ready、控制台和调用面，断言 Checking 排除、服务先 ready 及检测转换。 |
| `ROUTE-006`–`ROUTE-009` | 每种 transport、timeout、allowlist 状态、非法协议、客户端错误、取消和其他 `4xx` 的故障分类表，断言单次隔离、fallback 与健康中性。 |
| `ROUTE-010`–`ROUTE-011` | 制造同一路由并发请求，其中旧请求成功而新请求触发隔离；断言旧成功不恢复，提交前故障按原候选序列转交。连接相关配置编辑触发系统所有权重检（路由重新进入 Checking 并由探测决定），非连接类编辑保持健康与隔离周期不变。 |
| `ROUTE-012`–`ROUTE-015` | 非流式完整读取、流式首事件 priming、提交前和提交后失败、候选耗尽场景；断言无泄漏首字节、首事件无损、后提交不拼接。SSE 重写路径以语义无损为合同：多行 `data:` 折叠、unicode 转义规范化与数值往返边界由测试钉住，模型名替换时结构与字段顺序保留、未知字段保留，无需替换时逐字节透传。 |
| `ROUTE-016`–`ROUTE-018` | 捕获启动/恢复 probe 的协议、模型、stream、输入和 token 上界；保持 Available 路由空闲确认无周期 probe，并检测单路由并发上限。 |
| `ROUTE-019`–`ROUTE-021` | 使用可控时钟验证默认与 `N=0` 的完整间隔序列、cap 重复、成功清零和再次故障从 `B` 开始。 |
| `ROUTE-022` | 浏览器触发恢复检测并捕获上游请求；断言固定原生 probe、完整 UI 状态、安全结果且无法输入任意 prompt/模型。 |
| `DATA-001`–`DATA-003` | 在真实进程边界提交多记录配置并注入 commit 失败；重启和并发读取断言 foreign keys、原子提交、无中间态及旧运行时保持。 |
| `DATA-004`–`DATA-005` | 使 operational-record 写入失败而上游成功；断言客户端仍成功、内存健康立即变化、Storage Degraded 和 gap 出现且无虚构 usage。 |
| `DATA-006`–`DATA-008` | 用旧、当前、较新、迁移失败和前置备份失败数据库启动；断言前向链、事务回滚、ready/exit 和原库保留。 |
| `DATA-009`–`DATA-012` | WAL 写入并触发自动/手工/迁移前备份；对每个快照做完整性与内容检查，验证权限、24 小时门槛、失败保护及最近 10 个轮换。 |
| `DATA-013`–`DATA-016` | 损坏数据库和多类候选备份的恢复演练；断言无自动空库、隔离验证、切换前保护、失败不切换、成功保留数据并重做 Checking。 |
| `DATA-017` | 管理 API、CLI 和页面能力清单检查，确认不存在 portable import/export 或跨机迁移入口。 |
| `OPS-001`–`OPS-005` | 成功、一次 fallback、多次 fallback、全失败和中断流调用；检查调用表和详情只生成一条记录、尝试链字段、成功 usage 归属和未知值/聚合排除。 |
| `OPS-006`–`OPS-008` | 以精确 usage fixture 覆盖缓存存在/缺失/零输入，断言费用公式、命中率、六窗口和两级 token 分布，且更改价格不改变路由命中。 |
| `OPS-009` | 使用可控时钟运行 14 天边界清理，断言逐调用/事件删除而永久每日聚合仍可查询且无调用 ID。 |
| `OPS-010`–`OPS-016` | 浏览器和管理 API 场景覆盖六区状态、各字段、异常历史、恢复设置区（基础间隔与倍增上限）、存储恢复条件、备份/迁移/恢复阶段及 usage gap 不回填。 |
| `OPS-017`–`OPS-019` | 触发生命周期、路由、存储、数据保护和轮换事件并解析 stderr；断言字段、普通成功静默、日期/大小轮换、14 天与 200 MiB 上限。 |
| `OPS-020`–`OPS-021` | 向请求、响应、header、query、Base URL 和上游错误植入唯一 canary；扫描数据库、页面、API、日志和备份元数据，断言只出现规范化类别和安全本地标识；管理聚焦编辑面板加载端点是完整 Base URL 的唯一合法出现处。 |
| `UI-001`–`UI-003` | 浏览器从登录进入，断言 Operations 默认、Calls & usage 次级导航、路由分组表和聚焦管理入口，不出现被排除领域对象。 |
| `UI-004`–`UI-006` | 空库完成引导流程，并逐项提交不完整/非法表单；断言清单进度、上下文返回、字段错误和不可调用状态。 |
| `UI-007`–`UI-009` | 浏览器覆盖三态路由、配置修正、probe 状态及密钥创建/搜索/一次显示/撤销确认，断言系统健康不可直接编辑；配置修正健康中性：编辑不直接设置健康，连接相关编辑进入 Checking 并由系统重检决定。 |
| `UI-010`–`UI-012` | 用成功、fallback、失败、gap 和备份 fixture 验证 Calls & usage 表、详情 modal、数据安全操作及明确排除的云/下载/删除控件。 |
| `UI-013` | 产品表面审查以领域对象和能力清单为准，确认没有继承 Sub2API 品牌、页面结构或商业/多租户功能。 |
| `PKG-001`–`PKG-004` | 发布 archive 在干净 WSL2 用户环境安装；检查单 Linux 进程、自包含依赖、版本并排、稳定入口、XDG 布局、权限和嵌入资产。 |
| `PKG-005`–`PKG-008` | Windows/WSL2 记录式验收创建登录任务，验证登录启动、有界重启、四个生命周期命令、ready 检查及默认浏览器行为。 |
| `PKG-009`–`PKG-011` | 自动化启动矩阵覆盖默认/显式端口、端口冲突、非法配置、损坏/较新 schema 和延迟上游；断言固定 bind、ready 边界和非零退出。 |
| `PKG-012` | 运行短请求与超过 30 秒请求后 stop/restart；断言停止接收、短请求完成、长请求取消和资源关闭。 |
| `PKG-013`–`PKG-014` | 记录式升级演练覆盖无迁移失败、迁移成功后应用失败及显式旧版本+备份回退，验证原子入口和无 live downgrade。 |
| `PKG-015` | 在记录环境中从 Windows 和 WSL2 分别调用同一构建和实例，并从 Windows 打开控制台；保存地址、版本、期望与实际结果。 |

## Out of Scope

- 编辑、接管或切换 Codex、KimiCode 或其他 harness 的配置；管理 prompt、MCP、skill、session 或 agent 工作流。
- Tauri、系统托盘、原生 Windows relay 二进制、原生 macOS/Linux 桌面应用，以及 Linux x86_64 WSL2 以外的生产拓扑。
- wildcard、LAN、远程或公网访问，以及为这些暴露方式设计 TLS、反向代理、远程身份认证或多人权限。
- Anthropic、Gemini 或其他非 OpenAI 兼容的下游/上游协议；Chat Completions 与 Responses 之间的协议转换。
- 根据 prompt 或 main/subagent/review 用途推断模型；用途别名、模型降级、跨发布模型 Fallback 或不同上游模型名的语义等价推断。
- 自动供应商/模型发现、外部价格服务、自动报价、余额或订阅查询、套餐管理、绝对账单与支付结算。
- 多租户用户、组织、角色、分组、配额、rate limit、并发控制、session stickiness、随机/加权调度或复杂 scheduler。
- 对 Available 路由做周期性健康检测、独立本地互联网故障检测，或无需目标模型和原生协议的网络连通性检测。
- 在流式响应提交后自动重试或拼接第二次生成；Responses 后台任务及单独的 API 级取消端点。
- 请求/响应正文、prompt、tool 参数、原始 header、原始上游错误或客户端 IP/UA 的审计、搜索、导出或呈现。
- 可移植配置导入导出、跨机器迁移、云对象存储备份、任意备份计划、备份下载或手工删除。
- 操作系统 keychain、加密 vault 或数据库内字段级秘密加密。
- 直接 fork、依赖或抽取 CC Switch 的 relay 模块；复制 Sub2API 的页面清单、领域模型、品牌或视觉设计。

## Further Notes

### 决策追溯

| Resolved decision | Normative coverage |
| --- | --- |
| [Identify the Reusable CC Switch Relay Kernel](issues/01-identify-reusable-cc-switch-relay-kernel.md) | `SYS-003`, `ROUTE-006`–`ROUTE-018` 及测试 prior art 边界 |
| [Establish the OpenAI-Compatible MVP Contract](issues/02-establish-openai-compatible-mvp-contract.md) | `API-001`–`API-018`, `ROUTE-011`–`ROUTE-015` |
| [Define the Provider and Model Route Data Model](issues/03-define-provider-and-model-route-data-model.md) | `SYS-004`–`SYS-006`, `CFG-001`–`CFG-013`, `ROUTE-001`–`ROUTE-005` |
| [Define the Route Failure and Recovery State Machine](issues/04-define-route-failure-and-recovery-state-machine.md) | `ROUTE-003`–`ROUTE-022` |
| [Choose the Implementation Foundation](issues/05-choose-the-implementation-foundation.md) | `SYS-002`–`SYS-003`, `DATA-001`, `PKG-002`, `PKG-004` |
| [Choose the Local Service Security Boundary](issues/06-choose-the-local-service-security-boundary.md) | `SEC-001`–`SEC-009`, `PKG-009` |
| [Define Purpose-Specific Model Selection](issues/07-define-purpose-specific-model-selection.md) | `SYS-005`–`SYS-006`, `CFG-011`, `ROUTE-001` |
| [Validate the Local Management Workflow](issues/08-validate-the-local-management-workflow.md) | `UI-001`–`UI-012`, `OPS-010`–`OPS-016` |
| [Define the Model Catalog and Cost Accounting Boundary](issues/09-define-model-catalog-and-cost-accounting.md) | `CFG-003`–`CFG-004`, `OPS-002`, `OPS-004`–`OPS-008` |
| [Define the Persistence, Backup, and Migration Contract](issues/10-define-persistence-backup-and-migration-contract.md) | `DATA-001`–`DATA-017` |
| [Define Packaging and Startup Experience](issues/11-define-packaging-and-startup-experience.md) | `PKG-001`–`PKG-015`, `SEC-004`–`SEC-005` |
| [Define Operational Diagnostics and Retention](issues/12-define-operational-diagnostics-and-retention.md) | `OPS-001`–`OPS-021` |
| [Map Sub2API Management Web Capabilities to the Relay MVP](issues/13-map-sub2api-management-web-capabilities.md) | `UI-001`–`UI-013` 及明确排除的产品范围 |
| [Choose the MVP Specification Assembly and Handoff Contract](issues/14-choose-mvp-specification-assembly-and-handoff-contract.md) | 稳定需求 ID、外部行为验收矩阵、追溯规则和 handoff 边界 |
| [Bound the Base URL Rendering Surface](issues/41-bound-base-url-rendering.md) | OPS-020 措辞修订：allowlist 界定到诊断面（调用记录/运维事件/日志/状态区），管理聚焦编辑面板为保留完整 Base URL 的唯一例外（CFG-002），管理列表/只读表面仅允许遮蔽或截断形式 |
| [Clarify Config-Edit Health Semantics](issues/42-clarify-config-edit-health-semantics.md) | ROUTE-010 措辞修订 + UI-007 澄清：连接相关配置编辑（供应商连接或路由的供应商/上游模型名/协议）使既有隔离证据失效并结束隔离周期，受影响路由重新进入 Checking 并由系统所有权重检以同一原生探测决定新健康，不违反 ROUTE-010；非连接类编辑（显示名、价格、倍率、资格）保持健康与隔离周期不变。配置修正健康中性指编辑不直接设置健康 |
| [Define SSE Rewrite Lossless Semantics](issues/43-define-sse-rewrite-lossless-semantics.md) | ROUTE-013 解释：「成功首事件无损转发」= 语义无损；无需模型替换时逐字节透传原始事件，需要替换时（API-008）保留对象结构、全部字段与字段顺序并仅替换 `model` 值；多行 `data:` 折叠与 unicode 转义规范化为语义等价编码变换，数值在 `serde_json` 可表示范围内精确往返，超出精度边界为已记录的可接受边界 |
| [Revise the OPS-010 Status-Area Count](issues/53-ops-status-area-count-revision.md) | OPS-010 措辞修订：六个独立状态区（Storage、模型路由、备份、迁移与恢复、usage 完整性、恢复设置）；恢复设置状态区为常驻展示区，展示基础间隔 `B` 与倍增上限 `N`（ROUTE-019）并可进入恢复设置面板（DATA-002）；14 天事件历史条款继续限定异常状态 |
| [Test Evidence Hygiene](issues/52-test-evidence-hygiene.md) | 测试决策记录：自动化套件运行 debug 测试构建（行为等价黑盒断言，不依赖优化/strip），实际 release 构建由 `build-archive.sh` 产出并经记录式验收验证——「测试启动实际发布构建」的例外；DATA-017 能力清单检查（管理 API/CLI/页面无 portable import/export 或跨机迁移入口）；迁移/恢复 drill 改用固定 v8/v9 schema 夹具而非 `ALTER TABLE` 篡改当前 schema；较新 schema 与 restore 候选内容模拟保留元数据戳（未来形态不可知，无法夹具化） |

### 非规范性参考与实现自由度

- `CONTEXT.md` 的领域词汇是本规范的命名来源；若实现需要新增核心领域概念，应先更新领域决策，而不是在代码中引入同义词。
- CC Switch 仅作为 MIT 许可下的行为和测试场景证据。本 MVP 不继承其 application/provider 健康粒度、请求触发恢复、Tauri 耦合或工具配置能力。未来若要复用源代码，必须单独完成范围、依赖、来源与许可证决定。
- Sub2API 仅证明 Operations-first、高密度表格、聚焦编辑和 summary-to-detail 等交互模式可行。其本地快照目录标为 `0.1.173`，内嵌 server metadata 为 `0.1.172`，且没有 commit metadata；它不是本规范行为的权威来源。
- 本规范中的 OpenAI 兼容子集是 MVP 的固定合同。未来 OpenAI API 新增字段应通过透明 pass-through 获益，但不会静默扩大受支持 endpoint 或增加跨协议转换。
- 实现 ticket 可自由决定 Rust module/file 组织、内部 trait 与函数边界、SQLite 表和索引细节、前端构建框架、管理 API 的内部资源路径、有限 timeout 数值和视觉细节，只要它们保持所有需求 ID 的外部行为、安全边界、数据不变量和验收证据。
- 若 ticketing 或实现发现需要改变项目目标、公开协议、核心数据关系、模块/架构接缝、新依赖、安全边界或不可逆迁移，必须先修订本规范并重新确认该决策；不得以实现便利覆盖本合同。
- 规范、planning map 和 resolved tickets 的优先级为：本规范提供 build contract；resolved tickets 提供理由与证据；planning map 仅用于导航。参考资料不得添加未写入需求 ID 的隐含行为。
