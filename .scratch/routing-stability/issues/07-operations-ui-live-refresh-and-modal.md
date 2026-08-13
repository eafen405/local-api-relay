# 07 — Operations 实时局部刷新 + 真 modal 聚焦面板

**What to build:** REL-008。5s 轮询 `/admin/operations`，仅重渲染状态区/路由表/清单，保持滚动与打开面板；聚焦面板改为真 modal（overlay、焦点陷阱、Esc、打开即滚动到可见）。

**Blocked by:** None（前端独立）——但建议在 04 后端稳定后开始，避免联调抖动。

**Status:** resolved

- [ ] app.js：轮询 + 按区重渲染 + modal 行为。
- [ ] app.css：overlay/焦点样式。
- [ ] 浏览器测试：刷新保持滚动/面板；modal 焦点与 Esc。

Spec coverage: `REL-008`。

