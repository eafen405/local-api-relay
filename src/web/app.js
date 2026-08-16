const app = document.querySelector("#app");

async function request(path, options = {}) {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", ...(options.headers || {}) },
    ...options,
  });
  const body = response.status === 204 ? null : await response.json().catch(() => null);
  if (!response.ok) {
    const error = new Error(body?.error?.message || "请求无法完成。");
    error.fields = body?.error?.fields || null;
    throw error;
  }
  return body;
}

// Renders field-attributed validation errors (UI-006) next to the offending
// input — the enclosing fieldset for grouped inputs (e.g. route eligibility)
// or the label for plain fields — with an actionable message, and clears any
// previously rendered field errors on the next submit. Returns how many errors
// were placed so the caller can keep the general error area as a fallback for
// messages that did not map to a rendered input.
function renderFieldErrors(form, fields) {
  document.querySelectorAll(".field-error").forEach((element) => element.remove());
  if (!fields || !form) return 0;
  let rendered = 0;
  for (const [name, message] of Object.entries(fields)) {
    const input = form.querySelector(`[name="${name}"]`);
    const container = input?.closest("fieldset") || input?.closest("label");
    if (!container) continue;
    const error = document.createElement("p");
    error.className = "error field-error";
    error.textContent = message;
    container.appendChild(error);
    rendered += 1;
  }
  return rendered;
}

// REL-009: one uniform submit path for every panel form: preventDefault,
// FormData body, request(), renderShell (or a custom onSuccess) on success,
// renderFieldErrors with the #panel-error fallback on failure, and the submit
// button disabled for the request duration.
async function submitPanelForm(form, { method, path, body, onSuccess }) {
  const error = form.querySelector("#panel-error");
  const submit = form.querySelector("button[type=submit]");
  if (submit) submit.disabled = true;
  try {
    const result = await request(path, { method, body: JSON.stringify(body) });
    if (onSuccess) onSuccess(result);
    else renderShell("operations");
  } catch (requestError) {
    if (!renderFieldErrors(form, requestError.fields)) {
      if (error) error.textContent = requestError.message;
    }
    if (submit) submit.disabled = false;
  }
}

function escapeHtml(value) {
  return String(value).replace(/[&<>\"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "\"": "&quot;", "'": "&#39;" })[character]);
}

function authPanel(title, text, form) {
  app.innerHTML = `<section class="auth-shell"><article class="auth-panel"><p class="eyebrow">Local API Relay</p><h1>${title}</h1><p class="muted">${text}</p>${form}</article></section>`;
}

function renderLogin() {
  authPanel("管理员登录", "使用本地管理员凭据打开管理界面。", `
    <form id="login-form"><label>管理员凭据<input name="password" type="password" autocomplete="current-password" required autofocus></label><p class="error" id="form-error"></p><button type="submit">登录</button></form>`);
  document.querySelector("#login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const error = document.querySelector("#form-error");
    try {
      const password = new FormData(event.currentTarget).get("password");
      const session = await request("/admin/login", { method: "POST", body: JSON.stringify({ password }) });
      session.must_change_password ? renderPasswordChange() : renderShell("operations");
    } catch (requestError) { error.textContent = requestError.message; }
  });
}

function renderPasswordChange() {
  authPanel("设置新凭据", "访问管理界面之前需要先设置新的管理员凭据。", `
    <form id="password-form"><label>新管理员凭据<input name="newPassword" type="password" autocomplete="new-password" minlength="16" required autofocus></label><p class="error" id="form-error"></p><button type="submit">保存凭据</button></form>`);
  document.querySelector("#password-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const error = document.querySelector("#form-error");
    try {
      const newPassword = new FormData(event.currentTarget).get("newPassword");
      await request("/admin/change-password", { method: "POST", body: JSON.stringify({ new_password: newPassword }) });
      renderShell("operations");
    } catch (requestError) { error.textContent = requestError.message; }
  });
}

function shellMarkup(view) {
  return `<section class="shell"><aside class="sidebar"><div class="sidebar-header"><span class="brand">Local API Relay</span></div><nav class="navigation" aria-label="主导航"><button class="nav" data-view="operations" aria-current="${view === "operations" ? "page" : "false"}">操作台</button><button class="nav" data-view="usage" aria-current="${view === "usage" ? "page" : "false"}">调用与用量</button><button class="nav" data-view="settings" aria-current="${view === "settings" ? "page" : "false"}">设置</button></nav><button class="secondary account-action" id="sign-out">退出登录</button></aside><main class="content" id="content"></main></section>`;
}

// UI-010: collapsible sections keep the Operations page scannable. The
// open/collapsed set is in-memory per page load; the 5s poll re-renders
// sections from it, so a user's collapse survives every refresh.
const COLLAPSED_SECTIONS = new Set();

function collapseToggle(id, label) {
  const collapsed = COLLAPSED_SECTIONS.has(id);
  return `<button type="button" class="collapse-toggle" data-collapse-toggle="${escapeHtml(id)}" aria-expanded="${collapsed ? "false" : "true"}" title="${collapsed ? "展开" : "折叠"}${label ? ` ${escapeHtml(label)}` : ""}"><span class="collapse-chevron" aria-hidden="true">▾</span></button>`;
}

function collapseClass(id) {
  return COLLAPSED_SECTIONS.has(id) ? " collapsed" : "";
}
function statusCard(title, value, warning = false) {
  return `<article class="status${warning ? " warning" : ""}"><h3>${title}</h3><p class="value">${escapeHtml(value)}</p></article>`;
}

// REL-007: the published-model catalog panel offers create (with prices) and
// deprecate (soft delete: existing routes keep serving, new routes cannot
// reference a deprecated model). Deprecated rows show a badge instead of the
// deprecate action; an empty catalog shows an actionable empty state.
function catalogMarkup(catalog) {
  const rows = catalog.map((model) => `<div class="table-row"><strong>${escapeHtml(model.name)}</strong><span>${escapeHtml(model.input_price_rmb)}</span><span>${escapeHtml(model.output_price_rmb)}</span><span>${escapeHtml(model.cached_input_price_rmb)}</span><div class="catalog-actions">${model.deprecated ? `<span class="deprecated-badge">已弃用</span>` : `<button class="secondary compact" data-deprecate-model="${escapeHtml(model.id)}">弃用</button>`}<button class="secondary compact" data-edit-prices="${escapeHtml(model.id)}">编辑</button></div></div>`).join("");
  const table = catalog.length
    ? `<div class="data-table catalog-table"><div class="table-head"><span>模型</span><span>输入</span><span>输出</span><span>缓存输入</span><span></span></div>${rows}</div>`
    : `<div class="empty"><h2>尚未发布任何模型</h2><p>从上方发布新模型，或从上游模型清单一键发布。</p></div>`;
  return `<div class="table-heading">${collapseToggle("catalog", "发布模型目录")}<div class="table-heading-title"><h2>发布模型目录</h2><p class="muted">每百万 token 价格（RMB）</p></div><button class="secondary" data-open-panel="catalog">发布新模型</button></div><div class="collapse-body">${table}</div>`;
}

function providersMarkup(providers) {
  const rows = providers.length ? providers.map((provider) => `<div class="provider-row"><div><strong>${escapeHtml(provider.display_name)}</strong><span>API 密钥 ${escapeHtml(provider.api_key_masked)}</span></div><button class="secondary compact" data-edit-provider="${escapeHtml(provider.id)}">编辑</button></div>`).join("") : `<p class="muted">尚未配置上游供应商。</p>`;
  return `<div class="table-heading">${collapseToggle("providers", "上游供应商")}<h2 class="table-heading-title">上游供应商</h2><button class="secondary" data-open-panel="provider">添加供应商</button></div><div class="collapse-body"><div class="provider-list">${rows}</div></div>`;
}

// REL-007: the upstream-model list shows each provider cached model catalog
// (from the last successful upstream /v1/models fetch). Models already
// published to the catalog render as plain text with a check mark; the rest
// get a one-click publish action (prices 0/0/0, editable afterwards in the
// catalog). A per-provider refresh re-fetches the upstream list; empty caches
// get a hint.
function upstreamModelsMarkup(state) {
  const providers = state.providers || [];
  const catalogNames = new Set((state.catalog || []).map((model) => model.name));
  const feedback = `<p class="check-feedback" id="upstream-model-feedback" role="status"></p>`;
  if (!providers.length) {
    return `${feedback}<div class="table-heading">${collapseToggle("upstream-models", "上游模型")}<div class="table-heading-title"><h2>上游模型</h2><p class="muted">添加供应商并获取其模型清单后，可一键发布到目录</p></div></div><div class="collapse-body"><p class="muted">尚未配置上游供应商。</p></div>`;
  }
  const rows = providers.map((provider) => {
    const models = provider.cached_models || [];
    const items = models.map((model) => {
      if (catalogNames.has(model)) {
        return `<li class="upstream-model in-catalog"><span class="upstream-model-check">✓</span><span>${escapeHtml(model)}</span></li>`;
      }
      return `<li class="upstream-model"><span>${escapeHtml(model)}</span><button class="secondary compact" data-create-upstream-model="${escapeHtml(provider.id)}" data-model-name="${escapeHtml(model)}">发布</button></li>`;
    }).join("");
    const body = models.length ? `<ul class="upstream-model-list">${items}</ul>` : `<p class="muted">尚未获取此供应商的模型清单。</p>`;
    return `<div class="provider-models"><div class="provider-models-head"><strong>${escapeHtml(provider.display_name)}</strong><button class="secondary compact" data-refresh-provider-models="${escapeHtml(provider.id)}">刷新模型</button></div>${body}</div>`;
  }).join("");
  return `${feedback}<div class="table-heading">${collapseToggle("upstream-models", "上游模型")}<div class="table-heading-title"><h2>上游模型</h2><p class="muted">尚未发布到目录的模型可一键发布</p></div></div><div class="collapse-body">${rows}</div>`;
}

function routesMarkup(routes) {
  if (!routes.length) return `<div class="empty"><h2>尚未配置模型路由</h2><p>完成配置清单以创建第一条路由。</p></div>`;
  // The Operations snapshot orders routes by (published model, route id), so
  // grouping by published model preserves both the group order (model name)
  // and the within-group order (route id): deterministic and repeatable.
  const groups = new Map();
  for (const route of routes) {
    const group = groups.get(route.published_model_name) || [];
    group.push(route);
    groups.set(route.published_model_name, group);
  }
  return [...groups.entries()].map(([modelName, modelRoutes]) => {
    const rows = modelRoutes.map((route) => {
      const checking = route.health === "checking";
      const failureDetail = route.failure_category ? `<small class="route-detail">${escapeHtml(route.failure_category.replaceAll("_", " "))}</small>` : "";
      const httpDetail = route.last_http_status != null ? `<small class="route-detail">HTTP ${escapeHtml(route.last_http_status)}</small>` : "";
      const intervalDetail = route.health === "unavailable" && route.current_interval_ms != null ? `<small class="route-detail">每 ${formatInterval(route.current_interval_ms)}</small>` : "";
      const nextProbe = route.health === "unavailable" && route.next_probe_at_ms != null ? formatRelativeMs(route.next_probe_at_ms) : "—";
      return `<div class="table-row"><span>${escapeHtml(route.provider_name)}</span><span>${escapeHtml(route.upstream_model_name)}</span><span>${escapeHtml(route.protocol)}</span><span>${escapeHtml(route.cost_multiplier)}x</span><span class="health-cell"><span class="health health-${escapeHtml(route.health)}">${escapeHtml(route.health)}</span>${failureDetail}${httpDetail}${intervalDetail}</span><span>${route.state_age_seconds != null ? formatDuration(route.state_age_seconds) : "—"}</span><span>${formatTimestamp(route.last_checked_at)}</span><span>${nextProbe}</span><span class="route-actions"><button class="secondary compact" data-edit-route="${escapeHtml(route.id)}">编辑</button><button class="secondary compact" data-check-route="${escapeHtml(route.id)}" ${checking ? "disabled" : ""} title="${checking ? "启动检查进行中" : "运行原生协议检查"}">检查</button><span class="check-feedback" role="status"></span></span></div>`;
    }).join("");
    const collapseId = `route-group:${modelName}`;
    return `<section class="route-group${collapseClass(collapseId)}" data-collapse-section="${escapeHtml(collapseId)}"><div class="table-heading">${collapseToggle(collapseId, modelName)}<h3 class="route-group-title table-heading-title">${escapeHtml(modelName)}</h3></div><div class="collapse-body"><div class="data-table routes-table"><div class="table-head"><span>上游供应商</span><span>上游模型</span><span>协议</span><span>倍率</span><span>健康</span><span>状态时长</span><span>上次检查</span><span>下次探测</span><span>操作</span></div>${rows}</div></div></section>`;
  }).join("");
}

function relayAccessKeyRows(keys, routes) {
  if (!keys.length) return `<p class="muted">没有匹配的访问密钥。</p>`;
  const routeNames = new Map(routes.map((route) => [route.id, `${route.published_model_name} (${route.protocol})`]));
  return keys.map((key) => {
    const scope = key.model_route_ids.map((routeId) => routeNames.get(routeId) || "已配置的模型路由").join(", ");
    const revoked = Boolean(key.revoked_at);
    const status = revoked ? "已撤销" : "有效";
    // REL-010: the full secret is re-displayable for keys created by the
    // current version; older hash-only keys cannot be recovered.
    const secretControl = key.secret
      ? `<div class="key-secret"><label>完整密钥<input class="mono" value="${escapeHtml(key.secret)}" readonly></label><button type="button" class="secondary compact" data-copy-secret="${escapeHtml(key.secret)}">复制</button></div>`
      : `<p class="muted">旧版本创建的密钥无法恢复完整值，请重新创建。</p>`;
    return `<div class="relay-key-row"><div><strong>${escapeHtml(key.label)}</strong><span>${escapeHtml(key.prefix)}</span></div><div><span class="key-status key-status-${revoked ? "revoked" : "active"}">${status}</span><p class="muted">${escapeHtml(scope)}</p>${secretControl}</div><div class="key-actions">${key.revoked_at ? "" : `<button class="secondary compact" data-edit-key="${escapeHtml(key.id)}">编辑</button><button class="secondary compact" data-revoke-key="${escapeHtml(key.id)}">撤销</button>`}</div></div>`;
  }).join("");
}

function relayAccessKeysMarkup(keys, routes) {
  return `<section class="table-region relay-key-region${collapseClass("relay-keys")}" data-collapse-section="relay-keys"><div class="table-heading">${collapseToggle("relay-keys", "中转访问密钥")}<div class="table-heading-title"><h2>中转访问密钥</h2></div><button class="secondary" data-open-panel="relay-key">创建访问密钥</button></div><div class="collapse-body"><div class="relay-key-tools"><label>搜索访问密钥<input id="relay-key-search" type="search" autocomplete="off"></label></div><div class="relay-key-list" id="relay-key-list">${relayAccessKeyRows(keys, routes)}</div></div></section>`;
}

function formatTimestamp(timestamp) {
  return Number.isInteger(timestamp) ? new Date(timestamp * 1000).toLocaleString() : "未检查";
}

function formatDuration(seconds) {
  if (seconds == null) return "—";
  if (seconds < 60) return `${seconds}秒`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}分 ${seconds % 60}秒`;
  const hours = Math.floor(seconds / 3600);
  return `${hours}小时 ${Math.floor((seconds % 3600) / 60)}分`;
}

function formatInterval(ms) {
  if (ms == null) return "—";
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds}秒`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}分`;
  return `${(seconds / 3600).toFixed(1)}小时`;
}

function formatRelativeMs(timestampMs) {
  const deltaSeconds = Math.round((timestampMs - Date.now()) / 1000);
  if (deltaSeconds <= 0) return "已逾期";
  if (deltaSeconds < 60) return `${deltaSeconds}秒后`;
  if (deltaSeconds < 3600) return `${Math.floor(deltaSeconds / 60)}分后`;
  return `${(deltaSeconds / 3600).toFixed(1)}小时后`;
}

// UI-004: the six-step setup checklist. Each step is connected to a real
// control (focus panel, eligibility editing, route check) rather than text
// only, and the checklist stays visible for any incomplete — including
// non-empty — configuration until the full chain is callable.
function checklistState(state, relayAccessKeys) {
  const providerReady = state.providers.length > 0;
  const routeReady = state.routes.length > 0;
  const multiplierReady = routeReady && state.routes.every((route) => parseFloat(route.cost_multiplier) > 0);
  // Only active (non-revoked) keys contribute eligibility: a revoked key is
  // rejected at the relay boundary (SEC-002), so its route set is not callable.
  const activeKeys = (relayAccessKeys || []).filter((key) => !key.revoked_at);
  const eligibilityReady = activeKeys.some((key) => key.model_route_ids.length > 0);
  // Step 6 is complete only when an active key is eligible for a route that is
  // actually Available — eligibility and availability must intersect.
  const callableReady = activeKeys.some((key) =>
    key.model_route_ids.some((routeId) => state.routes.some((route) => route.id === routeId && route.health === "available")));
  return {
    providerReady,
    routeReady,
    multiplierReady,
    eligibilityReady,
    callableReady,
    complete: providerReady && routeReady && multiplierReady && eligibilityReady && callableReady,
  };
}

function checklistMarkup(state, relayAccessKeys, regionId) {
  const progress = checklistState(state, relayAccessKeys);
  const firstRoute = state.routes[0];
  const firstEligibleKey = (relayAccessKeys || []).find((key) => !key.revoked_at && key.model_route_ids.length > 0);
  const routeControl = progress.routeReady
    ? `<button class="secondary compact" data-edit-route="${escapeHtml(firstRoute.id)}">编辑模型路由</button>`
    : `<button class="secondary compact" data-open-panel="route" ${progress.providerReady ? "" : "disabled"}>添加模型路由</button>`;
  const keyControl = progress.eligibilityReady
    ? `<button class="secondary compact" data-edit-key="${escapeHtml(firstEligibleKey.id)}">编辑访问密钥</button>`
    : `<button class="secondary compact" data-open-panel="relay-key" ${progress.routeReady ? "" : "disabled"}>创建访问密钥</button>`;
  const checkControl = progress.routeReady
    ? `<button class="secondary compact" data-check-route="${escapeHtml(firstRoute.id)}">检查路由</button>`
    : `<button class="secondary compact" disabled title="请先添加模型路由">检查路由</button>`;
  const steps = [
    { done: progress.providerReady, label: "添加上游供应商",
      control: `<button class="secondary compact" data-open-panel="provider">${progress.providerReady ? "再添加" : "添加供应商"}</button>` },
    { done: progress.routeReady, label: "选择发布模型", control: routeControl },
    { done: progress.routeReady, label: "映射明确的上游模型与协议", control: routeControl },
    { done: progress.multiplierReady, label: "设置正数成本倍率", control: routeControl },
    { done: progress.eligibilityReady, label: "为模型路由授予访问密钥资格", control: keyControl },
    { done: progress.callableReady, label: "验证并让配置可调用", control: checkControl },
  ];
  return `<section class="onboarding"${regionId ? ` id="${regionId}"` : ""} aria-label="配置清单"><div><p class="eyebrow">设置</p><h2>完成首个可调用配置</h2></div><ol class="checklist">${steps.map((step, index) => `<li><span class="check ${step.done ? "complete" : ""}">${step.done ? "完成" : String(index + 1)}</span><span>${step.label}</span>${step.control}</li>`).join("")}</ol></section>`;
}

function storageStatusMarkup(storage) {
  const abnormal = storage.state !== "healthy";
  const categories = (storage.categories || []).map((category) => `${escapeHtml(category.category)}: ${category.lost_records == null ? "丢失数量未知" : `丢失 ${category.lost_records}`}${category.error ? ` — ${escapeHtml(category.error)}` : ""}`).join("; ");
  const gaps = (storage.accounting_gaps || []).map((gap) => `${escapeHtml(gap.category)} ${formatTimestampMs(gap.started_at_ms)}→${gap.ended_at_ms == null ? "未关闭" : formatTimestampMs(gap.ended_at_ms)}`).join("; ");
  const detail = abnormal ? `<small class="route-detail">自 ${formatTimestamp(storage.since)}${categories ? ` · ${categories}` : ""}</small>${gaps ? `<small class="route-detail">记账缺口：${gaps}</small>` : ""}` : "";
  const history = abnormal ? `<button class="link compact" data-open-events="storage">事件历史</button>` : "";
  return `<article class="status${abnormal ? " warning" : ""}"><h3>存储</h3><p class="value">${escapeHtml(storage.state.replace("_", " "))}</p>${detail}${history}</article>`;
}

function usageStatusMarkup(usage) {
  const gapCount = (usage.gaps || []).length;
  const warning = usage.state !== "no_data" && usage.state !== "complete";
  const history = warning ? `<button class="link compact" data-open-events="usage">事件历史</button>` : "";
  return `<article class="status${warning ? " warning" : ""}"><h3>用量</h3><p class="value">${gapCount ? `${usage.state.replace("_", " ")} · ${gapCount} 个缺口` : usage.state.replace("_", " ")}</p>${history}</article>`;
}

function migrationStatusMarkup(migration) {
  const failed = migration.last_phase !== "none" && migration.last_result === "failed";
  const details = [];
  if (migration.migration_state === "migrated" && migration.migrated_from_schema != null) {
    details.push(`从 v${migration.migrated_from_schema} 迁移`);
  }
  if (migration.last_phase === "migration" && migration.last_result === "ok") {
    details.push(`迁移成功 ${formatTimestamp(migration.last_completed_at)}`);
  }
  if (migration.last_phase === "restore") {
    details.push(`恢复 ${migration.last_result}${migration.restore_source ? ` 自 ${escapeHtml(migration.restore_source)}` : ""}`);
  }
  if (migration.last_failed_reason) {
    details.push(`<small class="route-detail">${escapeHtml(migration.last_failed_reason)}</small>`);
  }
  const detail = details.length ? `<small class="route-detail">${details.join(" · ")}</small>` : "";
  const history = failed ? `<button class="link compact" data-open-events="migration">事件历史</button>` : "";
  return `<article class="status${failed ? " warning" : ""}" data-open-backups title="迁移、恢复与数据安全"><h3>迁移与恢复</h3><p class="value">v${escapeHtml(migration.running_schema)} ${escapeHtml(migration.migration_state)}</p>${detail}${history}</article>`;
}

// REL-008: the Operations view is split into refreshable regions with stable
// ids so a 5s poll can re-render only the live areas (status, routes,
// checklist, providers, upstream models) without touching the focused panel,
// page scroll or search state.
function operationsStatusMarkup(state) {
  const routesAbnormal = state.model_routes.unavailable > 0 || state.model_routes.checking > 0;
  const routesDetail = routesAbnormal ? `<small class="route-detail">${state.model_routes.checking} 检查中</small><button class="link compact" data-open-events="routes">事件历史</button>` : "";
  const backupsAbnormal = state.backups.state !== "ok";
  return `${storageStatusMarkup(state.storage)}<article class="status${routesAbnormal ? " warning" : ""}"><h3>模型路由</h3><p class="value">${state.model_routes.available} 可用，${state.model_routes.unavailable} 不可用</p>${routesDetail}</article><article class="status${backupsAbnormal ? " warning" : ""}" data-open-backups><h3>备份</h3><p class="value">${escapeHtml(state.backups.state.replace("_", " "))}</p>${backupsAbnormal ? `<button class="link compact" data-open-events="backups">事件历史</button>` : ""}</article>${migrationStatusMarkup(state.migration)}${usageStatusMarkup(state.usage)}<article class="status" data-open-recovery><h3>恢复</h3><p class="value">B ${formatInterval(state.recovery.base_interval_ms)} · ×2<sup>${state.recovery.doubling_limit}</sup></p></article>`;
}

function routesRegionMarkup(state) {
  return `<div class="table-heading">${collapseToggle("routes", "模型路由")}<h2 class="table-heading-title">模型路由</h2><button class="secondary" data-open-panel="route" ${state.providers.length ? "" : "disabled"}>添加模型路由</button></div><div class="collapse-body">${routesMarkup(state.routes)}</div>`;
}

function operationsMarkup(state, relayAccessKeys) {
  const checklistComplete = checklistState(state, relayAccessKeys).complete;
  return `<div class="title-row"><div><p class="eyebrow">管理</p><h1>操作台</h1><p class="muted last-refresh" id="last-refresh">尚未自动刷新</p></div><button id="add-route" ${state.providers.length ? "" : "disabled"}>添加模型路由</button></div><section class="status-grid" id="ops-status" aria-label="运行状态">${operationsStatusMarkup(state)}</section>${checklistComplete ? "" : checklistMarkup(state, relayAccessKeys, "ops-checklist")}<div class="workbench-grid operations-workbench"><section class="workbench-main"><section class="table-region${collapseClass("routes")}" id="ops-routes" data-collapse-section="routes">${routesRegionMarkup(state)}</section>${relayAccessKeysMarkup(relayAccessKeys, state.routes)}</section><aside class="workbench-aside"><section class="table-region provider-region${collapseClass("providers")}" id="ops-providers" data-collapse-section="providers">${providersMarkup(state.providers)}</section><section class="table-region upstream-model-region${collapseClass("upstream-models")}" id="ops-upstream-models" data-collapse-section="upstream-models">${upstreamModelsMarkup(state)}</section><section class="table-region catalog-region${collapseClass("catalog")}" id="ops-catalog" data-collapse-section="catalog">${catalogMarkup(state.catalog)}</section></aside></div><div id="focused-panel"></div>`;
}

const DEFAULT_CALLS_PAGE_SIZE = 25;
const USAGE_WINDOWS = ["1h", "5h", "24h", "7d", "14d", "all"];

function formatMs(ms) {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms}毫秒`;
  return `${(ms / 1000).toFixed(1)}秒`;
}

function formatTimestampMs(timestampMs) {
  return new Date(timestampMs).toLocaleString();
}

function modelRouteAttemptChainMarkup(attempts) {
  if (!attempts.length) return "";
  const rows = attempts.map((attempt) => `
      <div class="model-route-attempt-row"><span>${attempt.sequence + 1}</span><span class="mono">${escapeHtml(attempt.route_id)}</span><span>${escapeHtml(attempt.provider_name)} <small class="mono">${escapeHtml(attempt.provider_id)}</small></span><span>${formatTimestampMs(attempt.started_at_ms)}</span><span>${formatMs(attempt.duration_ms)}</span><span>${attempt.http_status != null ? attempt.http_status : "—"}</span><span>${attempt.failure_category ? escapeHtml(attempt.failure_category.replaceAll("_", " ")) : "—"}</span><span>${escapeHtml(attempt.commit_phase.replaceAll("_", " "))}</span><span>${escapeHtml(attempt.outcome.replaceAll("_", " "))}</span></div>`).join("");
  return `<div class="model-route-attempt-chain" role="region" aria-label="模型路由尝试链"><div class="model-route-attempt-head"><span>顺序</span><span>模型路由</span><span>供应商</span><span>开始</span><span>耗时</span><span>HTTP</span><span>失败</span><span>阶段</span><span>结果</span></div>${rows}</div>`;
}

function callEntryMarkup(call) {
  const failed = !call.succeeded;
  const tokens = failed ? "—" : `${call.input_tokens ?? "—"} 入 · ${call.cached_input_tokens ?? "—"} 缓存 · ${call.output_tokens ?? "—"} 出`;
  const provider = call.success_provider_name ? escapeHtml(call.success_provider_name) : "—";
  const cost = call.estimated_cost_rmb != null ? `RMB ${call.estimated_cost_rmb}` : "—";
  const completion = failed ? "—" : formatMs(call.completion_ms);
  const firstToken = failed || !call.streamed ? "—" : formatMs(call.first_token_ms);
  const chain = modelRouteAttemptChainMarkup(call.attempts);
  return `<div class="call-entry" data-call-entry><div class="table-row call-row"><span>${formatTimestampMs(call.created_at_ms)}</span><strong>${escapeHtml(call.published_model_name)}</strong><span>${provider}</span><span>${tokens}</span><span>${cost}</span><span>${completion}</span><span>${firstToken}</span><button class="secondary compact" data-call-toggle aria-expanded="false">模型路由尝试</button></div>${chain ? `<div class="model-route-attempt-chain-wrap" hidden>${chain}</div>` : ""}</div>`;
}

function callsTableMarkup(state) {
  const calls = state.calls || [];
  if (!calls.length) return `<div class="empty"><h2>尚无调用记录</h2><p>每个完成的客户端调用都会连同其模型路由尝试链显示在这里。</p></div>`;
  const rows = calls.map(callEntryMarkup).join("");
  const total = state.total || 0;
  const pageSize = state.page_size || DEFAULT_CALLS_PAGE_SIZE;
  const page = state.page || 0;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  return `<div class="data-table calls-table"><div class="table-head"><span>时间</span><span>发布模型</span><span>成功供应商</span><span>Token</span><span>预估费用</span><span>耗时</span><span>首 Token</span><span></span></div>${rows}</div><div class="pagination"><button class="secondary compact" data-calls-page="${page - 1}" ${page <= 0 ? "disabled" : ""}>上一页</button><span>第 ${page + 1} 页，共 ${totalPages} 页</span><button class="secondary compact" data-calls-page="${page + 1}" ${page + 1 >= totalPages ? "disabled" : ""}>下一页</button></div>`;
}

function usageIntegrityMarkup(integrity) {
  if (!integrity) return "";
  if (!integrity.gaps || !integrity.gaps.length) {
    return `<section class="table-region"><div class="table-heading"><h2>用量完整性</h2><p class="muted">此窗口数据完整。</p></div></section>`;
  }
  const rows = integrity.gaps.map((gap) => `<div class="table-row"><span>${escapeHtml(gap.kind.replaceAll("_", " "))}</span><span>${escapeHtml(gap.category)}</span><span>${formatTimestampMs(gap.started_at_ms)}</span><span>${gap.ended_at_ms == null ? "未关闭" : formatTimestampMs(gap.ended_at_ms)}</span><span>${gap.lost_records ?? "—"}</span></div>`).join("");
  return `<section class="table-region${collapseClass("usage-integrity")}" data-collapse-section="usage-integrity"><div class="table-heading">${collapseToggle("usage-integrity", "用量完整性")}<div class="table-heading-title"><h2>用量完整性</h2><p class="muted">此窗口数据不完整——存在 ${integrity.gaps.length} 个已知缺口；不会估算或回填。</p></div></div><div class="collapse-body"><div class="data-table usage-integrity-table"><div class="table-head"><span>类型</span><span>类别</span><span>开始</span><span>结束</span><span>丢失记录数</span></div>${rows}</div></div></section>`;
}

function usageMarkup(state) {
  const metric = (value) => value == null ? "—" : String(value);
  const totals = state.totals || {};
  const window = state.window || "24h";
  const windows = state.windows && state.windows.length ? state.windows : USAGE_WINDOWS;
  const hitRate = totals.cache_hit_rate == null ? "—" : `${(totals.cache_hit_rate * 100).toFixed(2)}%`;
  const share = (part, whole) => whole > 0 ? `${((part / whole) * 100).toFixed(1)}%` : "—";
  const totalInput = totals.input_tokens || 0;
  const distribution = (totals.models || []).map((model) => {
    const modelShare = share(model.input_tokens, totalInput);
    const providers = (model.providers || []).map((provider) =>
      `<div class="table-row"><strong>${escapeHtml(provider.provider_name)}</strong><span>${share(provider.input_tokens, model.input_tokens)}</span><span>${metric(provider.input_tokens)}</span><span>${metric(provider.cached_input_tokens)}</span><span>${metric(provider.output_tokens)}</span><span>RMB ${metric(provider.estimated_cost_rmb)}</span></div>`).join("");
    const collapseId = `usage-model:${model.published_model_name}`;
    return `<article class="usage-model${collapseClass(collapseId)}" data-collapse-section="${escapeHtml(collapseId)}"><div class="usage-model-head">${collapseToggle(collapseId, model.published_model_name)}<h3 class="table-heading-title">${escapeHtml(model.published_model_name)} <small>占输入 ${modelShare}</small></h3></div><div class="collapse-body"><div class="data-table usage-table"><div class="table-head"><span>上游供应商</span><span>输入占比</span><span>输入 Token</span><span>缓存输入</span><span>输出 Token</span><span>预估费用</span></div>${providers || `<div class="empty"><p>暂无用量记录</p></div>`}</div></div></article>`;
  }).join("");
  return `<div class="title-row"><div><p class="eyebrow">管理</p><h1>调用与用量</h1></div></div><div class="workbench-grid usage-workbench"><section class="workbench-main"><section class="window-selector" aria-label="用量窗口">${windows.map((w) => `<button class="secondary compact${w === window ? " selected" : ""}" data-usage-window="${w}">${w}</button>`).join("")}</section><section class="usage-grid" aria-label="用量汇总"><article class="metric"><h3>输入 Token</h3><p class="number">${metric(totals.input_tokens)}</p></article><article class="metric"><h3>缓存输入</h3><p class="number">${metric(totals.cached_input_tokens)}</p></article><article class="metric"><h3>输出 Token</h3><p class="number">${metric(totals.output_tokens)}</p></article><article class="metric"><h3>缓存命中率</h3><p class="number">${hitRate}</p></article><article class="metric"><h3>预估费用</h3><p class="number">RMB ${metric(totals.estimated_cost_rmb)}</p></article></section><section class="table-region${collapseClass("usage-calls")}" data-collapse-section="usage-calls"><div class="table-heading">${collapseToggle("usage-calls", "调用")}<h2 class="table-heading-title">调用</h2></div><div class="collapse-body">${callsTableMarkup(state)}</div></section></section><aside class="workbench-aside">${usageIntegrityMarkup(state.usage_integrity)}<section class="table-region${collapseClass("usage-distribution")}" data-collapse-section="usage-distribution"><div class="table-heading">${collapseToggle("usage-distribution", "Token 分布")}<h2 class="table-heading-title">Token 分布</h2></div><div class="collapse-body">${distribution || `<div class="empty"><h2>暂无用量记录</h2><p>上报了用量的成功调用按发布模型和上游供应商显示在这里。</p></div>`}</div></section></aside></div>`;
}

function settingsMarkup() {
  return `<div class="title-row"><div><p class="eyebrow">管理</p><h1>设置</h1><p class="muted">备份、迁移与恢复，以及路由行为参数。</p></div></div><section class="settings-grid" aria-label="设置"><article class="settings-card"><h2>数据安全</h2><p class="muted">查看备份、迁移与恢复状态，创建手动备份或从备份恢复。</p><button class="secondary" data-open-backups>打开数据安全</button></article><article class="settings-card"><h2>路由设置</h2><p class="muted">调整恢复探测间隔、倍增上限、超时与上游模型同步周期。</p><button class="secondary" data-open-recovery>打开路由设置</button></article></section><div id="focused-panel"></div>`;
}

function prepareSettings(state) {
  operationsState = state;
  operationsKeys = [];
}

function bindUsage(state) {
  document.querySelectorAll("[data-usage-window]").forEach((button) => button.addEventListener("click", async () => {
    await loadUsagePage(0, state.page_size, button.dataset.usageWindow);
  }));
  document.querySelectorAll("[data-calls-page]").forEach((button) => button.addEventListener("click", async () => {
    await loadUsagePage(parseInt(button.dataset.callsPage, 10), state.page_size, state.window || "24h");
  }));
  document.querySelectorAll("[data-call-toggle]").forEach((button) => button.addEventListener("click", () => {
    const entry = button.closest("[data-call-entry]");
    const wrap = entry?.querySelector(".model-route-attempt-chain-wrap");
    if (!wrap) return;
    wrap.hidden = !wrap.hidden;
    button.setAttribute("aria-expanded", String(!wrap.hidden));
    button.textContent = wrap.hidden ? "模型路由尝试" : "收起链";
  }));
}

async function loadUsagePage(page, pageSize, window) {
  const query = new URLSearchParams({ page: String(page), page_size: String(pageSize || DEFAULT_CALLS_PAGE_SIZE), window: window || "24h" });
  const state = await request(`/admin/calls-usage?${query}`);
  document.querySelector("#content").innerHTML = usageMarkup(state);
  bindUsage(state);
}

// REL-008: focusedPanel renders a real modal — a fixed overlay with a
// centered panel. The panel keeps the class names the surface contract relies
// on (.focused-panel h2, #focused-panel, data-close-panel). Focus moves to the
// first focusable element on open, Tab is trapped inside the panel, Esc and a
// backdrop click close it, and the previously focused element is restored.
let lastFocusedBeforePanel = null;

function focusedPanel(title, body) {
  return `<div class="panel-overlay"><section class="focused-panel" role="dialog" aria-modal="true" aria-label="${title}"><div class="panel-heading"><h2>${title}</h2><button class="secondary compact" data-close-panel>关闭</button></div>${body}</section></div>`;
}

function closeFocusedPanel() {
  const container = document.querySelector("#focused-panel");
  if (container) container.innerHTML = "";
  const previous = lastFocusedBeforePanel;
  lastFocusedBeforePanel = null;
  if (previous && document.contains(previous)) previous.focus();
}

function focusedPanelFocusables(panel) {
  return [...panel.querySelectorAll("button, input, select, textarea, [href], [tabindex]")]
    .filter((element) => !element.disabled && element.tabIndex >= 0);
}

function attachPanelClose() {
  document.querySelectorAll("[data-close-panel]").forEach((button) => button.addEventListener("click", () => closeFocusedPanel()));
  const overlay = document.querySelector("#focused-panel .panel-overlay");
  if (overlay) overlay.addEventListener("click", (event) => { if (event.target === overlay) closeFocusedPanel(); });
  const panel = document.querySelector("#focused-panel .focused-panel");
  if (panel) {
    if (!lastFocusedBeforePanel) lastFocusedBeforePanel = document.activeElement;
    const focusables = focusedPanelFocusables(panel);
    (focusables[0] || panel).focus();
    panel.addEventListener("keydown", (event) => {
      if (event.key !== "Tab") return;
      const items = focusedPanelFocusables(panel);
      if (!items.length) { event.preventDefault(); return; }
      const first = items[0];
      const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    });
  }
  if (!attachPanelClose.escInstalled) {
    attachPanelClose.escInstalled = true;
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && document.querySelector("#focused-panel .focused-panel")) closeFocusedPanel();
    });
  }
}

async function showProviderPanel(provider) {
  const details = provider ? await request(`/admin/providers/${encodeURIComponent(provider.id)}`) : { display_name: "", base_url: "" };
  const title = provider ? "编辑上游供应商" : "添加上游供应商";
  document.querySelector("#focused-panel").innerHTML = focusedPanel(title, `<form id="provider-form"><label>供应商名称<input name="display_name" maxlength="128" value="${escapeHtml(details.display_name)}" required autofocus></label><label>Base URL<input name="base_url" type="url" value="${escapeHtml(details.base_url)}" placeholder="https://api.example.com/v1" required></label><label>上游 API 密钥<input name="api_key" type="password" autocomplete="off" required></label><p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit">保存供应商</button></div></form>`);
  attachPanelClose();
  document.querySelector("#provider-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    submitPanelForm(form, {
      method: provider ? "PATCH" : "POST",
      path: provider ? `/admin/providers/${encodeURIComponent(provider.id)}` : "/admin/providers",
      body: Object.fromEntries(new FormData(form)),
    });
  });
}

// REL-007: the upstream model name input offers the selected provider cached
// model list as a datalist suggestion, kept in sync with the provider select;
// free typing stays allowed.
function showRoutePanel(state, route) {
  if (!state.providers.length) return showProviderPanel();
  const modelOptions = state.catalog.map((model) => `<option value="${escapeHtml(model.id)}" ${route?.published_model_id === model.id ? "selected" : ""}>${escapeHtml(model.name)}</option>`).join("");
  const providerOptions = state.providers.map((provider) => `<option value="${escapeHtml(provider.id)}" ${route?.provider_id === provider.id ? "selected" : ""}>${escapeHtml(provider.display_name)}</option>`).join("");
  const title = route ? "编辑模型路由" : "添加模型路由";
  document.querySelector("#focused-panel").innerHTML = focusedPanel(title, `<form id="route-form"><label>发布模型<select name="published_model_id" required>${modelOptions}</select></label><label>上游供应商<select name="provider_id" required>${providerOptions}</select></label><label>上游模型名称<input name="upstream_model_name" list="upstream-model-options" maxlength="256" value="${escapeHtml(route?.upstream_model_name || "")}" required autofocus><datalist id="upstream-model-options"></datalist></label><label>协议<select name="protocol"><option value="chat_completions" ${route?.protocol === "chat_completions" ? "selected" : ""}>Chat Completions</option><option value="responses" ${route?.protocol === "responses" ? "selected" : ""}>Responses</option></select></label><label>成本倍率<input name="cost_multiplier" type="number" inputmode="decimal" min="0.000001" step="0.000001" value="${escapeHtml(route?.cost_multiplier || "1")}" required></label><p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit">${route ? "保存路由" : "保存并检查"}</button></div></form>`);
  attachPanelClose();
  const providerSelect = document.querySelector("#route-form select[name=provider_id]");
  const datalist = document.querySelector("#upstream-model-options");
  const updateModelSuggestions = () => {
    const provider = state.providers.find((candidate) => candidate.id === providerSelect.value);
    datalist.innerHTML = (provider?.cached_models || []).map((model) => `<option value="${escapeHtml(model)}"></option>`).join("");
  };
  providerSelect.addEventListener("change", updateModelSuggestions);
  updateModelSuggestions();
  document.querySelector("#route-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    submitPanelForm(form, {
      method: route ? "PATCH" : "POST",
      path: route ? `/admin/model-routes/${encodeURIComponent(route.id)}` : "/admin/model-routes",
      body: Object.fromEntries(new FormData(form)),
      onSuccess: () => {
        renderShell("operations");
        window.setTimeout(() => document.querySelector(".routes-table")?.scrollIntoView({ behavior: "smooth", block: "center" }), 0);
      },
    });
  });
}

function showPricesPanel(model) {
  document.querySelector("#focused-panel").innerHTML = focusedPanel(`编辑 ${escapeHtml(model.name)} 价格`, `<form id="prices-form"><label>输入价格（RMB/百万）<input name="input_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="${escapeHtml(model.input_price_rmb)}" required></label><label>输出价格（RMB/百万）<input name="output_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="${escapeHtml(model.output_price_rmb)}" required></label><label>缓存输入价格（RMB/百万）<input name="cached_input_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="${escapeHtml(model.cached_input_price_rmb)}" required></label><p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit">保存价格</button></div></form>`);
  attachPanelClose();
  document.querySelector("#prices-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    submitPanelForm(form, {
      method: "PATCH",
      path: `/admin/published-models/${encodeURIComponent(model.id)}/prices`,
      body: Object.fromEntries(new FormData(form)),
    });
  });
}

// REL-009: the relay key eligibility panel groups the checkboxes by published
// model (each label shows provider + upstream model), tracks the selected
// route count per model, warns when a model has exactly one selected route
// (no fallback), and offers a per-model select-all button.
function routeEligibilityMarkup(state, key) {
  if (!state.routes.length) return `<p class="muted">请先创建模型路由再签发访问密钥。</p>`;
  const groups = new Map();
  for (const route of state.routes) {
    const group = groups.get(route.published_model_name) || [];
    group.push(route);
    groups.set(route.published_model_name, group);
  }
  return [...groups.entries()].map(([modelName, routes]) => {
    const choices = routes.map((route) => `<label class="route-choice"><input name="model_route_ids" type="checkbox" value="${escapeHtml(route.id)}" ${key?.model_route_ids?.includes(route.id) ? "checked" : ""}><span><strong>${escapeHtml(route.provider_name)}</strong><small>${escapeHtml(route.upstream_model_name)} · ${escapeHtml(route.protocol)}</small></span></label>`).join("");
    return `<fieldset class="route-eligibility"><legend>${escapeHtml(modelName)}<span class="model-route-count">已选 <span data-model-count>0</span> 条</span><button type="button" class="secondary compact" data-select-model>全选</button></legend>${choices}</fieldset>`;
  }).join("");
}

function bindEligibilityCoverage() {
  document.querySelectorAll(".route-eligibility").forEach((fieldset) => {
    const inputs = fieldset.querySelectorAll('input[name="model_route_ids"]');
    const countLabel = fieldset.querySelector("[data-model-count]");
    const update = () => {
      const count = fieldset.querySelectorAll('input[name="model_route_ids"]:checked').length;
      if (countLabel) countLabel.textContent = String(count);
      let warning = fieldset.querySelector(".eligibility-warning");
      if (count === 1) {
        if (!warning) {
          warning = document.createElement("p");
          warning.className = "error eligibility-warning";
          warning.textContent = "仅选中 1 条路由：无回退（Fallback）可用。";
          fieldset.appendChild(warning);
        }
      } else if (warning) {
        warning.remove();
      }
    };
    inputs.forEach((input) => input.addEventListener("change", update));
    const selectAll = fieldset.querySelector("[data-select-model]");
    if (selectAll) selectAll.addEventListener("click", () => {
      inputs.forEach((input) => { input.checked = true; });
      update();
    });
    update();
  });
}

function showRelayAccessKeyPanel(state, key) {
  const title = key ? "编辑中转访问密钥" : "创建中转访问密钥";
  document.querySelector("#focused-panel").innerHTML = focusedPanel(title, `<form id="relay-key-form"><label>标签<input name="label" maxlength="128" value="${escapeHtml(key?.label || "")}" required autofocus></label>${routeEligibilityMarkup(state, key)}<p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit" ${state.routes.length ? "" : "disabled"}>${key ? "保存访问密钥" : "创建访问密钥"}</button></div></form>`);
  attachPanelClose();
  bindEligibilityCoverage();
  document.querySelector("#relay-key-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const formData = new FormData(form);
    submitPanelForm(form, {
      method: key ? "PATCH" : "POST",
      path: key ? `/admin/relay-access-keys/${encodeURIComponent(key.id)}` : "/admin/relay-access-keys",
      body: { label: formData.get("label"), model_route_ids: formData.getAll("model_route_ids") },
      onSuccess: key ? undefined : (result) => showCreatedRelayAccessKey(result),
    });
  });
}

function showCreatedRelayAccessKey(key) {
  document.querySelector("#focused-panel").innerHTML = focusedPanel("中转访问密钥", `<div class="one-time-secret"><label>访问密钥<input id="new-relay-secret" value="${escapeHtml(key.secret)}" readonly></label><button class="secondary compact" id="copy-relay-secret">复制</button></div><p class="muted">此密钥保存在本地，可随时在密钥列表中重新查看和复制。</p><div class="panel-actions"><button type="button" data-close-panel>完成</button></div>`);
  attachPanelClose();
  document.querySelector("#copy-relay-secret").addEventListener("click", async () => { await navigator.clipboard?.writeText(key.secret); });
}

function formatBytes(bytes) {
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KiB`;
  return `${bytes} B`;
}

function backupStatusMarkup(status) {
  const failure = status.last_failed_stage ? `<p class="error">上次失败于 ${escapeHtml(status.last_failed_stage)}：${escapeHtml(status.last_failed_reason || "未知")}</p>` : "";
  return `<div class="backup-summary"><div><span>上次验证</span><strong>${status.last_backup_at != null ? formatTimestamp(status.last_backup_at) : "无"}</strong></div><div><span>触发方式</span><strong>${status.last_trigger ? escapeHtml(status.last_trigger) : "—"}</strong></div><div><span>Schema</span><strong>${status.schema_version != null ? `v${status.schema_version}` : "—"}</strong></div><div><span>大小</span><strong>${status.last_size != null ? formatBytes(status.last_size) : "—"}</strong></div><div><span>下次自动备份</span><strong>${status.next_auto_backup_at != null ? formatTimestamp(status.next_auto_backup_at) : "无待备份变更"}</strong></div><div><span>保留</span><strong>${status.count} / ${status.retention}</strong></div></div>${failure}`;
}

function migrationRestoreDetailMarkup(migration) {
  const preBackup = migration.pre_backup ? ` — 迁移前备份 ${escapeHtml(migration.pre_backup.name)}（v${migration.pre_backup.schema_version}）${migration.pre_backup.ok ? "已验证" : "失败"}` : "";
  let migrationText = "数据库 schema 为当前版本。";
  if (migration.migration_state === "fresh") migrationText = "全新安装——由当前程序初始化 schema。";
  if (migration.migration_state === "migrated") migrationText = `从 schema v${migration.migrated_from_schema} 迁移${preBackup}。`;
  let operation = "此数据库尚未执行过迁移或恢复。";
  if (migration.last_phase === "migration") {
    operation = migration.last_result === "ok"
      ? `迁移完成于 ${formatTimestamp(migration.last_completed_at)}。`
      : `迁移失败于 ${escapeHtml(migration.last_failed_stage || "未知")}：${escapeHtml(migration.last_failed_reason || "未知")}。`;
  } else if (migration.last_phase === "restore") {
    operation = migration.last_result === "ok"
      ? `于 ${formatTimestamp(migration.last_completed_at)} 从 ${escapeHtml(migration.restore_source || "一个备份")} 恢复。`
      : `恢复失败于 ${escapeHtml(migration.last_failed_stage || "未知")}：${escapeHtml(migration.last_failed_reason || "未知")}。`;
  }
  return `<section class="migration-status"><h3>迁移与恢复</h3><div class="backup-summary"><div><span>运行中 schema</span><strong>v${migration.running_schema}</strong></div><div><span>支持的 schema</span><strong>v${migration.supported_schema}</strong></div><div><span>状态</span><strong>${escapeHtml(migration.migration_state)}</strong></div></div><p class="muted">${migrationText}</p><p class="muted">${operation}</p></section>`;
}

async function showBackupsPanel() {
  document.querySelector("#focused-panel").innerHTML = focusedPanel("数据安全", `<p class="muted">正在加载备份状态…</p>`);
  try {
    const [backups, operations] = await Promise.all([request("/admin/backups"), request("/admin/operations")]);
    const { status, data } = backups;
    const rows = data.length ? data.map((backup) => `<div class="table-row"><span>${formatTimestamp(backup.created_at)}</span><span>${escapeHtml(backup.trigger)}</span><span>v${backup.schema_version}</span><span>${formatBytes(backup.size)}</span><span><button class="secondary compact" data-restore-backup="${escapeHtml(backup.name)}">恢复</button></span></div>`).join("") : `<div class="empty"><h2>尚未创建备份</h2></div>`;
    document.querySelector("#focused-panel").innerHTML = focusedPanel("数据安全", `<div class="panel-actions backup-create-action"><button id="create-backup">创建备份</button></div>${backupStatusMarkup(status)}${migrationRestoreDetailMarkup(operations.migration)}<div class="data-table backup-table"><div class="table-head"><span>创建时间</span><span>触发方式</span><span>Schema</span><span>大小</span><span></span></div>${rows}</div>`);
    attachPanelClose();
    document.querySelector("#create-backup")?.addEventListener("click", createBackup);
    document.querySelectorAll("[data-restore-backup]").forEach((button) => button.addEventListener("click", () => restoreBackup(button, button.dataset.restoreBackup)));
  } catch (requestError) {
    document.querySelector("#focused-panel").innerHTML = focusedPanel("数据安全", `<p class="error">${escapeHtml(requestError.message)}</p>`);
    attachPanelClose();
  }
}

async function createBackup() {
  const button = document.querySelector("#create-backup");
  button.disabled = true;
  button.textContent = "正在创建备份…";
  try {
    await request("/admin/backups", { method: "POST" });
    showBackupsPanel();
  } catch (requestError) {
    button.disabled = false;
    button.textContent = "创建备份";
    document.querySelector("#focused-panel").insertAdjacentHTML("beforeend", `<p class="error">${escapeHtml(requestError.message)}</p>`);
  }
}

// UI-012/OPS-015: the explicit restore flow reports its coarse stages
// (verify → switch → recheck) while it runs, points at the exact failed stage
// with an actionable reason, and always leaves the current database selected so
// the operator can keep working.
const RESTORE_STAGES = [
  { key: "verify", label: "验证候选备份" },
  { key: "switch", label: "切换数据库" },
  { key: "recheck", label: "重置路由并重新检查" },
];

function restoreProgressMarkup(progress) {
  const currentIndex = RESTORE_STAGES.findIndex((stage) => stage.key === progress.stage);
  const steps = RESTORE_STAGES.map((stage, index) => {
    const done = index < currentIndex;
    const active = index === currentIndex;
    return `<li class="${done ? "complete" : ""}${active ? " active" : ""}"><span class="check ${done ? "complete" : ""}">${done ? "完成" : String(index + 1)}</span><span>${stage.label}</span>${active ? `<small class="route-detail">进行中…</small>` : ""}</li>`;
  }).join("");
  return `<section class="restore-progress" data-restore-progress role="status"><div class="table-heading"><h2>正在恢复数据库</h2><p class="muted">正在从 <span class="mono">${escapeHtml(progress.candidate)}</span> 恢复。当前数据库会先被保留；只有全部检查通过后候选才会替换它。</p></div><ol class="checklist">${steps}</ol></section>`;
}

function restoreFailureMarkup(migration) {
  const stage = (migration.last_failed_stage || "未知").replaceAll("_", " ");
  // Bridge the fine-grained persisted stage (OPS-015 `last_failed_stage`) back
  // to the coarse phase the progress view shows, so the failure panel reads
  // against the same three-step vocabulary.
  const phase = migration.last_failed_stage === "switch"
    ? "数据库切换期间"
    : "验证候选备份期间";
  const reason = migration.last_failed_reason || "无法完成恢复";
  return `<div class="restore-result restore-failed" data-restore-failed><h3>恢复失败</h3><p>${phase}在 <strong>${escapeHtml(stage)}</strong> 失败：${escapeHtml(reason)}。</p><p>当前数据库已保留并继续使用——未替换任何内容。</p><p class="muted">关闭此面板继续操作，准备就绪后重试恢复。</p></div><div class="panel-actions"><button type="button" class="secondary" data-close-panel>返回</button><button type="button" id="restore-return">返回操作台</button></div>`;
}

async function restoreBackup(button, name) {
  if (!window.confirm(`从备份 ${name} 恢复本地数据库？\n\n当前数据库会先作为新备份保留，然后被替换。`)) return;
  // One restore at a time: refuse to start while another restore is running.
  try {
    const current = await request("/admin/restore/progress");
    if (current.state === "restoring") {
      document.querySelector("#focused-panel").insertAdjacentHTML("beforeend", `<p class="error">恢复已在进行中。</p>`);
      return;
    }
  } catch (_) { /* the restore call itself will surface any failure */ }
  button.disabled = true;
  const panel = document.querySelector("#focused-panel");
  panel.innerHTML = focusedPanel("数据安全", restoreProgressMarkup({ candidate: name, stage: "verify" }));
  attachPanelClose();
  let polling = true;
  (async () => {
    while (polling) {
      await new Promise((resolve) => setTimeout(resolve, 250));
      if (!polling) break;
      try {
        const progress = await request("/admin/restore/progress");
        if (!polling) break;
        if (progress.state === "restoring") {
          const current = panel.querySelector(".restore-progress");
          if (current) current.outerHTML = restoreProgressMarkup(progress);
        } else {
          polling = false;
        }
      } catch (_) { /* keep the last known progress */ }
    }
  })();
  try {
    const result = await request("/admin/restore", { method: "POST", body: JSON.stringify({ name }) });
    polling = false;
    if (!document.querySelector("[data-restore-progress]")) return;
    const completedStages = RESTORE_STAGES.map((stage) => stage.label).join(" → ");
    document.querySelector("#focused-panel").innerHTML = focusedPanel("数据安全", `<div class="restore-result"><h3>恢复完成</h3><p>于 ${formatTimestamp(result.completed_at)} 从 <span class="mono">${escapeHtml(result.restored_from)}</span>（schema v${result.schema_version}）恢复。</p><p>${result.routes_reset_to_checking} 条模型路由重新进入检查状态并正在重新探测。原数据库已作为备份 <span class="mono">${escapeHtml(result.pre_restore_backup)}</span> 保留。</p><p class="muted">恢复阶段已完成：${completedStages}。恢复后的数据库可能需要使用该备份中保存的管理员凭据重新登录。</p></div><div class="panel-actions"><button type="button" id="restore-return">返回操作台</button></div>`);
    attachPanelClose();
    document.querySelector("#restore-return")?.addEventListener("click", () => renderShell("operations"));
  } catch (requestError) {
    polling = false;
    if (!document.querySelector("[data-restore-progress]")) return;
    // The durable failure stage and actionable reason live in the OPS-015
    // migration/restore status; reload it so the panel points at the exact
    // failed stage rather than a generic message.
    let migration = null;
    try { migration = (await request("/admin/operations")).migration; } catch (_) { /* fall back to the request message */ }
    document.querySelector("#focused-panel").innerHTML = focusedPanel("数据安全", restoreFailureMarkup({ last_failed_stage: migration?.last_failed_stage || "未知", last_failed_reason: migration?.last_failed_reason || requestError.message }));
    attachPanelClose();
    document.querySelector("#restore-return")?.addEventListener("click", () => renderShell("operations"));
  }
}

function showRecoveryPanel(settings) {
  document.querySelector("#focused-panel").innerHTML = focusedPanel("路由设置", `<p class="muted">不可用的模型路由按封顶倍增计划探测：首个探测在基础间隔 B 后执行，每次失败将等待时间翻倍，直至 B × 2<sup>N</sup>。探测成功即恢复路由服务。上游截止时间按"与直连对齐"原则放宽并可调。</p><form id="recovery-form"><label>基础间隔 B（秒）<input name="base_interval_seconds" type="number" inputmode="decimal" min="0.1" max="86400" step="0.05" value="${(settings.base_interval_ms / 1000).toFixed(2)}" required></label><label>倍增上限 N<input name="doubling_limit" type="number" min="0" max="20" step="1" value="${settings.doubling_limit}" required></label><label>首事件截止（秒）<input name="first_event_timeout_seconds" type="number" inputmode="decimal" min="1" max="3600" step="1" value="${Math.round(settings.first_event_timeout_ms / 1000)}" required></label><label>流中空闲截止（秒）<input name="stream_idle_timeout_seconds" type="number" inputmode="decimal" min="1" max="3600" step="1" value="${Math.round(settings.stream_idle_timeout_ms / 1000)}" required></label><label>非流式响应截止（秒）<input name="nonstream_timeout_seconds" type="number" inputmode="decimal" min="1" max="3600" step="1" value="${Math.round(settings.nonstream_timeout_ms / 1000)}" required></label><label>可用路由保鲜周期（分钟，0 = 关闭）<input name="freshness_interval_minutes" type="number" inputmode="decimal" min="0" max="1440" step="1" value="${Math.round(settings.freshness_interval_ms / 60000)}" required></label><label>隔离失败阈值（连续可归因失败次数）<input name="quarantine_threshold" type="number" min="1" max="5" step="1" value="${settings.quarantine_threshold}" required></label><label>上游模型清单同步周期（小时，0 = 关闭）<input name="upstream_sync_hours" type="number" inputmode="decimal" min="0" max="168" step="1" value="${Math.round(settings.upstream_sync_interval_ms / 3600000)}" required></label><p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit">保存路由设置</button></div></form>`);
  attachPanelClose();
  document.querySelector("#recovery-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const values = Object.fromEntries(new FormData(event.currentTarget));
    submitPanelForm(event.currentTarget, {
      method: "PATCH",
      path: "/admin/recovery-settings",
      body: {
        base_interval_ms: Math.round(parseFloat(values.base_interval_seconds) * 1000),
        doubling_limit: parseInt(values.doubling_limit, 10),
        first_event_timeout_ms: Math.round(parseFloat(values.first_event_timeout_seconds) * 1000),
        stream_idle_timeout_ms: Math.round(parseFloat(values.stream_idle_timeout_seconds) * 1000),
        nonstream_timeout_ms: Math.round(parseFloat(values.nonstream_timeout_seconds) * 1000),
        freshness_interval_ms: Math.round(parseFloat(values.freshness_interval_minutes) * 60000),
        quarantine_threshold: parseInt(values.quarantine_threshold, 10),
        upstream_sync_interval_ms: Math.round(parseFloat(values.upstream_sync_hours) * 3600000)
      },
    });
  });
}

const DEFAULT_EVENTS_PAGE_SIZE = 50;

function eventRowMarkup(event) {
  const correlation = event.correlation_id ? `<span class="mono">${escapeHtml(event.correlation_id.slice(0, 8))}</span>` : "—";
  return `<div class="table-row"><span>${formatTimestampMs(event.occurred_at_ms)}</span><span class="severity severity-${escapeHtml(event.severity)}">${escapeHtml(event.severity)}</span><span class="mono">${escapeHtml(event.event_code)}</span><span>${correlation}</span><span class="mono event-payload">${escapeHtml(JSON.stringify(event.payload))}</span></div>`;
}

function eventsMarkup(section, state) {
  const events = state.events || [];
  const rows = events.length ? events.map(eventRowMarkup).join("") : `<div class="empty"><p>${escapeHtml(section)} 尚无运维事件记录。</p></div>`;
  const pageSize = state.page_size || DEFAULT_EVENTS_PAGE_SIZE;
  const page = state.page || 0;
  const totalPages = Math.max(1, Math.ceil((state.total || 0) / pageSize));
  return `<section class="table-region events-region"><div class="table-heading"><div><h2>运维事件历史</h2><p class="muted">14 天仅元数据历史 · ${escapeHtml(section)}</p></div></div><div class="data-table events-table"><div class="table-head"><span>时间</span><span>级别</span><span>事件</span><span>关联 ID</span><span>详情</span></div>${rows}</div><div class="pagination"><button class="secondary compact" data-events-page="${page - 1}" ${page <= 0 ? "disabled" : ""}>上一页</button><span>第 ${page + 1} 页，共 ${totalPages} 页</span><button class="secondary compact" data-events-page="${page + 1}" ${page + 1 >= totalPages ? "disabled" : ""}>下一页</button></div></section>`;
}

async function loadEventsPanel(section, page) {
  const state = await request(`/admin/operations/events?section=${encodeURIComponent(section)}&page=${page || 0}`);
  document.querySelector("#focused-panel").innerHTML = focusedPanel("运维事件历史", eventsMarkup(section, state));
  attachPanelClose();
  document.querySelectorAll("[data-events-page]").forEach((button) => button.addEventListener("click", () => {
    loadEventsPanel(section, parseInt(button.dataset.eventsPage, 10)).catch((requestError) => {
      document.querySelector("#focused-panel").insertAdjacentHTML("beforeend", `<p class="error">${escapeHtml(requestError.message)}</p>`);
    });
  }));
}

function showEventsPanel(section) {
  document.querySelector("#focused-panel").innerHTML = focusedPanel("运维事件历史", `<p class="muted">加载中…</p>`);
  attachPanelClose();
  loadEventsPanel(section, 0).catch((requestError) => {
    document.querySelector("#focused-panel").innerHTML = focusedPanel("运维事件历史", `<p class="error">${escapeHtml(requestError.message)}</p>`);
    attachPanelClose();
  });
}

// REL-007: create a published model (name + three prices) from the catalog
// panel; a successful create re-renders Operations.
function showCreatePublishedModelPanel() {
  document.querySelector("#focused-panel").innerHTML = focusedPanel("发布新模型", `<form id="published-model-form"><label>模型名称<input name="name" maxlength="128" placeholder="gpt-4o" required autofocus></label><label>输入价格（RMB/百万）<input name="input_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="0" required></label><label>输出价格（RMB/百万）<input name="output_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="0" required></label><label>缓存输入价格（RMB/百万）<input name="cached_input_price_rmb" type="number" inputmode="decimal" min="0" step="0.000001" value="0" required></label><p class="error" id="panel-error"></p><div class="panel-actions"><button type="button" class="secondary" data-close-panel>取消</button><button type="submit">发布模型</button></div></form>`);
  attachPanelClose();
  document.querySelector("#published-model-form").addEventListener("submit", (event) => {
    event.preventDefault();
    submitPanelForm(event.currentTarget, {
      method: "POST",
      path: "/admin/published-models",
      body: Object.fromEntries(new FormData(event.currentTarget)),
    });
  });
}

// The latest Operations snapshot the page renders (initial render and every
// poll). Delegated actions open panels against this fresh state instead of a
// stale render-time closure.
let operationsState = null;
let operationsKeys = [];
let currentView = null;

// REL-008: one delegated click handler on the app root covers every action in
// the refreshable Operations regions, so poll re-renders never orphan buttons.
// The nearest matching element wins, which keeps an event-history link inside
// a status card from also opening the backups panel.
function installOperationsDelegation() {
  document.querySelector("#app").addEventListener("click", async (event) => {
    const target = event.target.closest("[data-collapse-toggle], [data-open-events], [data-open-backups], [data-open-recovery], [data-open-panel], [data-edit-provider], [data-edit-route], [data-check-route], [data-edit-prices], [data-deprecate-model], [data-create-upstream-model], [data-refresh-provider-models], [data-edit-key], [data-revoke-key], [data-copy-secret]");
    if (!target) return;
    if (target.hasAttribute("data-collapse-toggle")) {
      const id = target.dataset.collapseToggle;
      if (COLLAPSED_SECTIONS.has(id)) COLLAPSED_SECTIONS.delete(id); else COLLAPSED_SECTIONS.add(id);
      const collapsed = COLLAPSED_SECTIONS.has(id);
      target.closest("[data-collapse-section]")?.classList.toggle("collapsed", collapsed);
      target.setAttribute("aria-expanded", collapsed ? "false" : "true");
      target.title = collapsed ? "展开" : "折叠";
      return;
    }
    if (target.hasAttribute("data-open-events")) { showEventsPanel(target.dataset.openEvents); return; }
    if (target.hasAttribute("data-open-backups")) { showBackupsPanel(); return; }
    if (target.hasAttribute("data-open-recovery")) { showRecoveryPanel(operationsState?.recovery); return; }
    if (target.hasAttribute("data-open-panel")) {
      const panel = target.dataset.openPanel;
      if (panel === "provider") return showProviderPanel();
      // Creating a NEW key: pass no key object (an undefined key means
      // create; passing the whole keys array here crashed the panel).
      if (panel === "relay-key") return showRelayAccessKeyPanel(operationsState);
      if (panel === "catalog") return showCreatePublishedModelPanel();
      return showRoutePanel(operationsState);
    }
    if (target.hasAttribute("data-edit-provider")) {
      showProviderPanel(operationsState?.providers.find((provider) => provider.id === target.dataset.editProvider));
      return;
    }
    if (target.hasAttribute("data-edit-route")) {
      showRoutePanel(operationsState, operationsState?.routes.find((route) => route.id === target.dataset.editRoute));
      return;
    }
    if (target.hasAttribute("data-check-route")) {
      const button = target;
      button.disabled = true;
      const originalLabel = button.textContent;
      button.textContent = "检查中…";
      const feedback = button.closest(".table-row")?.querySelector(".check-feedback");
      try {
        await request(`/admin/model-routes/${encodeURIComponent(button.dataset.checkRoute)}/check`, { method: "POST" });
        renderShell("operations");
      } catch (requestError) {
        button.disabled = false;
        button.textContent = originalLabel;
        if (feedback) feedback.textContent = requestError.message;
      }
      return;
    }
    if (target.hasAttribute("data-edit-prices")) {
      showPricesPanel(operationsState?.catalog.find((model) => model.id === target.dataset.editPrices));
      return;
    }
    if (target.hasAttribute("data-deprecate-model")) {
      if (!window.confirm("弃用此发布模型？\n\n新路由将无法引用它，已有路由保持不变。")) return;
      await request(`/admin/published-models/${encodeURIComponent(target.dataset.deprecateModel)}/deprecate`, { method: "POST" });
      renderShell("operations");
      return;
    }
    if (target.hasAttribute("data-create-upstream-model")) {
      await request("/admin/published-models", { method: "POST", body: JSON.stringify({ name: target.dataset.modelName, input_price_rmb: "0", output_price_rmb: "0", cached_input_price_rmb: "0" }) });
      renderShell("operations");
      return;
    }
    if (target.hasAttribute("data-refresh-provider-models")) {
      const result = await request(`/admin/providers/${encodeURIComponent(target.dataset.refreshProviderModels)}/models/refresh`, { method: "POST" });
      if (result.error) {
        const feedback = document.querySelector("#upstream-model-feedback");
        if (feedback) feedback.textContent = `刷新失败：${result.error}`;
      }
      renderShell("operations");
      return;
    }
    if (target.hasAttribute("data-edit-key")) {
      showRelayAccessKeyPanel(operationsState, (operationsKeys || []).find((key) => key.id === target.dataset.editKey));
      return;
    }
    if (target.hasAttribute("data-revoke-key")) {
      if (!window.confirm("撤销此中转访问密钥？")) return;
      await request(`/admin/relay-access-keys/${encodeURIComponent(target.dataset.revokeKey)}/revoke`, { method: "POST" });
      renderShell("operations");
    }
    if (target.hasAttribute("data-copy-secret")) {
      await navigator.clipboard?.writeText(target.dataset.copySecret);
      target.textContent = "已复制";
      window.setTimeout(() => { target.textContent = "复制"; }, 1200);
    }
  });
}

// REL-009: the relay-key search input debounces by 300ms and drops out-of-order
// responses via a sequence number, so a stale reply never overwrites a newer
// query result.
let relayKeySearchTimer = null;
let relayKeySearchSequence = 0;

function bindOperations(state, relayAccessKeys) {
  operationsState = state;
  operationsKeys = relayAccessKeys;
  document.querySelector("#add-route")?.addEventListener("click", () => showRoutePanel(operationsState));
  document.querySelector("#relay-key-search")?.addEventListener("input", (event) => {
    window.clearTimeout(relayKeySearchTimer);
    const query = event.currentTarget.value;
    const sequence = ++relayKeySearchSequence;
    relayKeySearchTimer = window.setTimeout(async () => {
      try {
        const keys = await request(`/admin/relay-access-keys?search=${encodeURIComponent(query)}`);
        if (sequence !== relayKeySearchSequence) return;
        document.querySelector("#relay-key-list").innerHTML = relayAccessKeyRows(keys.data, operationsState?.routes || []);
      } catch (_) { /* keep the current list on a transient failure */ }
    }, 300);
  });
}

// REL-008: the 5-second Operations poll re-renders only the live regions from
// a fresh snapshot, preserving page scroll, the focused panel (never touched),
// input focus in open panels and in-progress check-button disabled states.
let operationsPollTimer = null;
let operationsPollRunning = false;

function stopOperationsPolling() {
  operationsPollRunning = false;
  if (operationsPollTimer != null) {
    window.clearInterval(operationsPollTimer);
    operationsPollTimer = null;
  }
}

function updateLastRefreshIndicator() {
  const indicator = document.querySelector("#last-refresh");
  if (indicator) indicator.textContent = `已刷新 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
}

function refreshOperationsRegions(state, keys) {
  const refresh = (selector, markup) => {
    const region = document.querySelector(selector);
    if (region) region.innerHTML = markup;
  };
  // While a manual check is in flight the routes table and checklist keep
  // their current disabled/loading state untouched (UI-008).
  const checkInFlight = Boolean(document.querySelector("[data-check-route]:disabled"));
  refresh("#ops-status", operationsStatusMarkup(state));
  refresh("#ops-providers", providersMarkup(state.providers));
  refresh("#ops-upstream-models", upstreamModelsMarkup(state));
  if (checkInFlight) return;
  refresh("#ops-routes", routesRegionMarkup(state));
  const checklist = document.querySelector("#ops-checklist");
  const needed = !checklistState(state, keys).complete;
  if (needed && checklist) checklist.outerHTML = checklistMarkup(state, keys, "ops-checklist");
  else if (needed && !checklist) document.querySelector("#ops-status")?.insertAdjacentHTML("afterend", checklistMarkup(state, keys, "ops-checklist"));
  else if (!needed && checklist) checklist.remove();
}

async function pollOperationsTick() {
  if (!operationsPollRunning || currentView !== "operations" || document.hidden) return;
  try {
    const [state, keys] = await Promise.all([request("/admin/operations"), request("/admin/relay-access-keys")]);
    if (!operationsPollRunning || currentView !== "operations") return;
    operationsState = state;
    operationsKeys = keys.data;
    refreshOperationsRegions(state, keys.data);
    updateLastRefreshIndicator();
  } catch (_) { /* transient failures keep the last render */ }
}

function startOperationsPolling() {
  stopOperationsPolling();
  operationsPollRunning = true;
  operationsPollTimer = window.setInterval(() => { pollOperationsTick(); }, 5000);
}

function renderErrorView(error) {
  const message = error && error.message ? error.message : "无法加载管理数据。";
  app.innerHTML = `<section class="error-view"><article class="auth-panel"><p class="eyebrow">Local API Relay</p><h1>加载失败</h1><p class="muted">${escapeHtml(message)}</p><div class="panel-actions"><button id="retry-load">重试</button></div></article></section>`;
  document.querySelector("#retry-load").addEventListener("click", () => renderShell(currentView || "operations"));
}

async function renderShell(view) {
  currentView = view;
  stopOperationsPolling();
  app.innerHTML = shellMarkup(view);
  document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => renderShell(button.dataset.view)));
  document.querySelector("#sign-out").addEventListener("click", async () => { await request("/admin/logout", { method: "POST" }); renderLogin(); });
  const content = document.querySelector("#content");
  content.innerHTML = `<p class="muted loading" role="status">正在加载…</p>`;
  try {
    const state = view === "operations"
      ? await Promise.all([request("/admin/operations"), request("/admin/relay-access-keys")])
      : view === "usage"
        ? [await request("/admin/calls-usage"), null]
        : [await request("/admin/operations"), null];
    content.innerHTML = view === "operations" ? operationsMarkup(state[0], state[1].data) : view === "usage" ? usageMarkup(state[0]) : settingsMarkup();
    if (view === "operations") {
      operationsState = state[0];
      operationsKeys = state[1].data;
      bindOperations(state[0], state[1].data);
      updateLastRefreshIndicator();
      startOperationsPolling();
    } else if (view === "usage") {
      bindUsage(state[0]);
    } else {
      prepareSettings(state[0]);
    }
  } catch (error) {
    stopOperationsPolling();
    // Only a genuine session expiry falls back to the login form: re-check the
    // session, and show the error view with a retry action otherwise.
    try {
      const session = await request("/admin/session");
      if (!session.authenticated) { renderLogin(); return; }
    } catch (_) { /* the session probe itself failed: not an expiry */ }
    renderErrorView(error);
  }
}

async function start() {
  installOperationsDelegation();
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopOperationsPolling();
    else if (currentView === "operations") {
      startOperationsPolling();
      pollOperationsTick();
    }
  });
  try {
    const session = await request("/admin/session");
    if (!session.authenticated) return renderLogin();
    return session.must_change_password ? renderPasswordChange() : renderShell("operations");
  } catch (_) { renderLogin(); }
}

start();
