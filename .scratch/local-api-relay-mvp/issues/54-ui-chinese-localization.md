# 54 — 管理界面中文化（仅静态文案）

**What to build:** 浏览器管理界面（`src/web/index.html` + `src/web/app.js`）的用户可见静态文案当前为英文。本 ticket 将所有静态文案中文化（登录、导航、状态卡、表格标题、按钮、六步清单、聚焦面板、Calls & usage、数据安全/恢复面板、事件历史、时间与间隔格式化单位等）；**动态数据值保留英文**（路由健康状态 `available/unavailable/checking`、协议值、backup trigger、severity、event_code、attempt 的 failure_category/commit_phase/outcome、stage 值如 `backup current` 等），**服务端错误消息保留英文**（文本来自 admin API，中文化需改动 server/store 与大量测试断言，属范围外）。界面行为合同（UI-001..013）不变，浏览器测试断言同步更新为中文。

**Blocked by:** None — can start immediately.

**Status:** resolved

- [x] `app.js` 所有用户可见静态文案中文化。
- [x] `index.html` `lang="zh-CN"` + 中文 title。
- [x] 浏览器测试（`tests/browser/driver.js` / `tests/browser_surface.rs`）断言同步为中文。
- [x] 全套现有测试保持绿。

Spec coverage: `UI-001`–`UI-013`（行为合同不变，仅呈现语言；spec「实现自由度」覆盖视觉细节）。

## Answer

管理界面静态文案中文化（用户确认范围：仅界面静态文案；动态数据值与服务端错误消息保留英文）：

- `src/web/app.js`：全部用户可见静态文案中文化（约 150 处）——登录/强制改密、导航（操作台/调用与用量/退出登录）、状态卡、发布模型目录、上游供应商、模型路由表、中转访问密钥、六步配置清单、Calls & usage（Token 分布/调用表/分页/用量完整性/尝试链）、聚焦面板、数据安全/恢复面板、恢复检测、运维事件历史、时间与间隔格式化单位（秒/分/小时/毫秒/已逾期/N秒后）。
- `src/web/index.html`：`lang="zh-CN"` + `<title>本地 API 中转</title>`。
- 浏览器测试断言同步为中文（driver.js 与 browser_surface.rs 的 h1/面板标题/按钮/清单 label/面板文本/确认文案等）。
- 连带修复：`packaging/install.sh` 升级 preflight 的管理页标识标记从 `*"Local API Relay"*` 改为 `*"本地 API 中转"*`（title 中文化后原标记失效）；secure 套件的 `<title>` 断言同步。
- 边界修订（实现时落实）：时间/间隔格式化单位归入界面文案（中文化）；保留英文的仅为服务端数据值（health/protocol/trigger/severity/event_code/attempt 字段/stage 值）与服务端错误消息。品牌名「Local API Relay」、协议名 Chat Completions/Responses、Base URL、RMB、KiB/B 等技术名词保留。

**验证**：`cargo test` 138/138 全绿（browser 14 + packaging 29 + secure 95，`/tmp/t54-final.log`）；`cargo clippy --all-targets` 零警告；`node --check` app.js/driver.js 通过。双轴 code review 通过，变更记录 `/tmp/54-change-record.md`。

## Comments

- Standards 轴：中文一致性逐字节核对通过；review 修复 `key-status` class 不再从显示文本反推（直接基于 `key.revoked_at`）；pagination 重复与 `statusCard` 死代码为既有代码，未动。
- Spec 轴：行为合同（UI-001..013）不依赖语言全部保持；install.sh 标记改动的连带性已记录（升级总是使用新 archive 自带 install.sh）；`dist/` 旧 archive 内部自洽，待下次 build-archive.sh 更新。
