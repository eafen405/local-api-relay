#!/usr/bin/env node
"use strict";

// Browser automation driver for the Local API Relay management surface
// (ticket 49). Drives a real headless Chromium against a live loopback relay
// process and reports structured user-visible observations that the Rust
// process-boundary tests assert on — never the frontend component tree or
// embedded script strings.
//
// Usage:
//   node driver.js <scenario> --base <base-url> --credential <password>
//       [--new-credential <password>] [--extra <json-string>]
//
// stdout on success: {"ok":true,"evidence":{...}}
// stdout on failure: {"ok":false,"error":"..."} and non-zero exit.
//
// Environment:
//   LOCAL_API_RELAY_CHROMIUM_PATH   explicit chromium executable. WSL2 builds
//                                   usually lack the dedicated headless shell,
//                                   so the Rust harness resolves the full
//                                   chromium binary and hands it over here.

const { chromium } = require("playwright");

const DEFAULT_NEW_CREDENTIAL = "correct-horse-battery-staple";
const WAIT_TIMEOUT_MS = 30000;

// Optional per-step timing trace (LOCAL_API_RELAY_DRIVER_TRACE=1) for
// diagnosing harness slowness without changing the driver contract. Trace
// lines go to /tmp/local-api-relay-driver-trace.log so the Rust harness can
// still collect them after the driver exits.
const TRACE = process.env.LOCAL_API_RELAY_DRIVER_TRACE === "1";
let traceStart = Date.now();
const traceStream = TRACE ? require("fs").createWriteStream("/tmp/local-api-relay-driver-trace.log", { flags: "a" }) : null;
function trace(label) {
  if (TRACE) traceStream.write(`[driver +${Date.now() - traceStart}ms] ${label}\n`);
}

function fail(message) {
  console.log(JSON.stringify({ ok: false, error: String(message) }));
  process.exit(1);
}

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--base") args.base = argv[index + 1];
    else if (argv[index] === "--credential") args.credential = argv[index + 1];
    else if (argv[index] === "--new-credential") args.newCredential = argv[index + 1];
    else if (argv[index] === "--extra") {
      try {
        args.extra = JSON.parse(argv[index + 1]);
      } catch (error) {
        fail(`--extra must be JSON: ${error.message}`);
      }
      index += 1;
    } else if (!args.scenario) args.scenario = argv[index];
  }
  return args;
}

async function launchBrowser() {
  trace("launchBrowser: start");
  const browser = await chromium.launch({
    headless: true,
    executablePath: process.env.LOCAL_API_RELAY_CHROMIUM_PATH || undefined,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
  });
  trace("launchBrowser: connected");
  return browser;
}

// Signs in through the real login form. A fresh bootstrap credential forces
// the one-time password change (SEC-004); an already-activated credential goes
// straight to the Operations shell. Returns whether the password-change form
// was seen.
async function signIn(page, base, credential, newCredential) {
  trace("signIn: goto");
  await page.goto(base + "/", { waitUntil: "domcontentloaded" });
  trace("signIn: login form");
  await page.waitForSelector("#login-form", { timeout: WAIT_TIMEOUT_MS });
  await page.fill('input[name="password"]', credential);
  await page.click("#login-form button[type=submit]");
  trace("signIn: submitted login");
  await page.waitForSelector("#password-form, .shell, #form-error:not(:empty)", {
    timeout: WAIT_TIMEOUT_MS,
  });
  trace("signIn: password-or-shell resolved");
  if (await page.locator("#form-error:not(:empty)").count()) {
    throw new Error(`sign-in rejected: ${(await page.locator("#form-error").textContent()).trim()}`);
  }
  const mustChange = (await page.locator("#password-form").count()) === 1;
  if (mustChange) {
    trace("signIn: password change form");
    await page.fill('input[name="newPassword"]', newCredential || DEFAULT_NEW_CREDENTIAL);
    await page.click("#password-form button[type=submit]");
    trace("signIn: submitted password change");
    await page.waitForSelector(".shell", { timeout: WAIT_TIMEOUT_MS });
    trace("signIn: shell");
  }
  await page.waitForSelector(".shell h1", { timeout: WAIT_TIMEOUT_MS });
  trace("signIn: h1");
  return mustChange;
}

async function h1Text(page) {
  return (await page.locator(".shell h1").textContent()).trim();
}

async function bodyText(page) {
  return page.locator("body").innerText();
}

// ---------------------------------------------------------------------------
// UI-001: login lands on the Operations default view; the primary navigation
// carries Operations and Calls & usage; no Sub2API domain objects (Accounts,
// Groups, Channels) appear anywhere in the surface.
// ---------------------------------------------------------------------------
async function loginDefaultView({ page, base, credential, newCredential }) {
  const mustChangePassword = await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".status-grid", { timeout: WAIT_TIMEOUT_MS });
  const h1 = await h1Text(page);
  const navLabels = (await page.locator('.navigation [data-view]').allTextContents()).map((label) => label.trim());
  const currentView = (await page.locator('.navigation [data-view][aria-current="page"]').textContent()).trim();
  const hasStatusGrid = (await page.locator(".status-grid").count()) === 1;
  const text = await bodyText(page);
  return {
    mustChangePassword,
    h1,
    navLabels,
    currentView,
    hasStatusGrid,
    text,
  };
}

// ---------------------------------------------------------------------------
// UI-001: Calls & usage is the second persistent main view; navigation round-
// trips back to Operations.
// ---------------------------------------------------------------------------
async function usageSecondaryView({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.click('.navigation [data-view="usage"]');
  await page.waitForSelector(".window-selector", { timeout: WAIT_TIMEOUT_MS });
  const h1 = await h1Text(page);
  const windowButtons = (await page.locator("[data-usage-window]").allTextContents()).map((label) => label.trim());
  const hasUsageTotals = (await page.locator(".usage-grid").count()) === 1;
  const usageText = await page.locator(".content").innerText();
  await page.click('.navigation [data-view="operations"]');
  await page.waitForSelector(".status-grid", { timeout: WAIT_TIMEOUT_MS });
  const backH1 = await h1Text(page);
  return { h1, windowButtons, hasUsageTotals, usageText, backH1 };
}

// ---------------------------------------------------------------------------
// UI-002: Operations renders the model-route table grouped by published model,
// with each row carrying provider, upstream model, protocol, multiplier and
// system-owned health.
// ---------------------------------------------------------------------------
async function routeGroups({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".route-group", { timeout: WAIT_TIMEOUT_MS });
  const groups = await page.locator(".route-group").evaluateAll((sections) =>
    sections.map((section) => ({
      title: section.querySelector(".route-group-title").textContent.trim(),
      rows: Array.from(section.querySelectorAll(".table-row")).map((row) => {
        const cells = Array.from(row.children).map((cell) => cell.textContent.trim());
        return {
          provider: cells[0],
          upstreamModel: cells[1],
          protocol: cells[2],
          multiplier: cells[3],
          health: row.querySelector(".health") ? row.querySelector(".health").textContent.trim() : "",
          stateAge: cells[5],
          lastCheck: cells[6],
          nextProbe: cells[7],
        };
      }),
    })),
  );
  const text = await bodyText(page);
  return { groups, text };
}

// ---------------------------------------------------------------------------
// UI-005: adding and editing providers and model routes happens in a focused
// panel and returns to the original Operations context on save or cancel.
// ---------------------------------------------------------------------------
async function focusPanels({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  const evidence = {};

  // Add an upstream provider from the Operations provider region.
  await page.click('.provider-region [data-open-panel="provider"]');
  await page.waitForSelector("#provider-form", { timeout: WAIT_TIMEOUT_MS });
  evidence.addProviderTitle = (await page.locator(".focused-panel h2").textContent()).trim();
  await page.fill('input[name="display_name"]', "Browser provider");
  await page.fill('input[name="base_url"]', "https://browser-provider.example/v1");
  await page.fill('input[name="api_key"]', "browser-provider-key");
  await page.click("#provider-form button[type=submit]");
  await page.waitForSelector(".provider-region", { timeout: WAIT_TIMEOUT_MS });
  // Wait for the saved provider row (CSP-safe: no page-side eval).
  await page.locator(".provider-region").filter({ hasText: "Browser provider" }).waitFor({ timeout: WAIT_TIMEOUT_MS });
  evidence.providerSaved = true;
  evidence.backOnOperationsAfterSave = (await h1Text(page)) === "操作台";

  // Edit the provider: the panel loads the saved values and returns on save.
  await page.click('.provider-region [data-edit-provider]');
  await page.waitForSelector("#provider-form", { timeout: WAIT_TIMEOUT_MS });
  evidence.editProviderTitle = (await page.locator(".focused-panel h2").textContent()).trim();
  evidence.editLoadedName = await page.inputValue('input[name="display_name"]');
  evidence.editLoadedBaseUrl = await page.inputValue('input[name="base_url"]');
  // The edit panel loads name + base URL only (the key stays masked, OPS-020);
  // resubmitting requires a key value (the form field is required).
  await page.fill('input[name="api_key"]', "browser-provider-key");
  await page.fill('input[name="display_name"]', "Browser provider renamed");
  await page.click("#provider-form button[type=submit]");
  await page.locator(".provider-region").filter({ hasText: "Browser provider renamed" }).waitFor({ timeout: WAIT_TIMEOUT_MS });
  evidence.providerRenamed = true;

  // Add a model route from the Operations model-routes region.
  await page.click('.table-region [data-open-panel="route"]');
  await page.waitForSelector("#route-form", { timeout: WAIT_TIMEOUT_MS });
  evidence.addRouteTitle = (await page.locator(".focused-panel h2").textContent()).trim();
  await page.selectOption('select[name="published_model_id"]', { index: 0 });
  await page.selectOption('select[name="provider_id"]', { index: 0 });
  await page.fill('input[name="upstream_model_name"]', "browser-upstream-model");
  await page.selectOption('select[name="protocol"]', "chat_completions");
  await page.fill('input[name="cost_multiplier"]', "1");
  await page.click("#route-form button[type=submit]");
  await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });
  await page.locator(".routes-table").filter({ hasText: "browser-upstream-model" }).waitFor({ timeout: WAIT_TIMEOUT_MS });
  evidence.routeSaved = true;

  // Edit the route: the panel loads the current mapping and cancel returns.
  await page.click('.routes-table [data-edit-route]');
  await page.waitForSelector("#route-form", { timeout: WAIT_TIMEOUT_MS });
  evidence.editRouteTitle = (await page.locator(".focused-panel h2").textContent()).trim();
  evidence.editLoadedUpstreamModel = await page.inputValue('input[name="upstream_model_name"]');
  evidence.editLoadedMultiplier = await page.inputValue('input[name="cost_multiplier"]');
  // UI-007: health is system-owned — the edit panel offers no health field.
  evidence.editPanelHasHealthField = (await page.locator("#route-form input[name=health], #route-form select[name=health]").count()) > 0;
  await page.click('#route-form [data-close-panel]');
  await page.waitForSelector(".shell h1", { timeout: WAIT_TIMEOUT_MS });
  evidence.cancelReturnsToOperations = (await h1Text(page)) === "操作台";
  evidence.panelClosedAfterCancel = (await page.locator("#focused-panel").innerText()).trim() === "";
  return evidence;
}

// ---------------------------------------------------------------------------
// UI-009: creating a relay access key shows the full secret exactly once,
// the list is searchable, and revoking requires explicit confirmation.
// ---------------------------------------------------------------------------
async function relayKey({ page, base, credential, newCredential, extra }) {
  await signIn(page, base, credential, newCredential);
  const evidence = {};

  await page.click('.relay-key-region [data-open-panel="relay-key"]');
  await page.waitForSelector("#relay-key-form", { timeout: WAIT_TIMEOUT_MS });
  evidence.createKeyTitle = (await page.locator(".focused-panel h2").textContent()).trim();
  await page.fill('input[name="label"]', "Browser access key");
  await page.check('.route-eligibility input[name="model_route_ids"]');
  await page.click("#relay-key-form button[type=submit]");
  await page.waitForSelector("#new-relay-secret", { timeout: WAIT_TIMEOUT_MS });
  const secret = await page.inputValue("#new-relay-secret");
  evidence.secretLength = secret.length;
  evidence.copyButtonAtCreation = (await page.locator("#copy-relay-secret").count()) === 1;
  const panelText = await page.locator(".focused-panel").innerText();
  // REL-010: personal-relay keys stay re-displayable, so the creation panel
  // promises re-viewing instead of a one-time display.
  evidence.oneTimeNotice = panelText.includes("随时");
  evidence.secretInputShown = (await page.locator("#new-relay-secret").count()) === 1;

  // Close the one-time display; the Operations list refreshes on reload and
  // must show only the label, the secret prefix and Active — never the secret.
  await page.click('.focused-panel [data-close-panel]');
  await page.waitForSelector(".shell h1", { timeout: WAIT_TIMEOUT_MS });
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForSelector(".relay-key-list", { timeout: WAIT_TIMEOUT_MS });
  await page.locator(".relay-key-list").filter({ hasText: "Browser access key" }).waitFor({ timeout: WAIT_TIMEOUT_MS });
  const listText = await page.locator(".relay-key-list").innerText();
  const bodyTextAfter = await bodyText(page);
  evidence.listShowsLabel = listText.includes("Browser access key");
  evidence.listShowsPrefix = listText.includes(secret.slice(0, 12));
  evidence.listShowsActive = listText.includes("有效");
  // UI-009: the row shows the key's scope (the eligible model routes).
  evidence.listShowsScope = listText.includes("gpt-5.6-sol (chat_completions)");
  evidence.fullSecretAbsentAfterClose = !bodyTextAfter.includes(secret);
  evidence.copyButtonAfterReload = (await page.locator("#copy-relay-secret").count()) === 0;

  // Search filters the list; a no-match query renders the empty result.
  await page.fill("#relay-key-search", "no-such-key-label");
  await page.waitForSelector("#relay-key-list:has-text('没有匹配的访问密钥')", { timeout: WAIT_TIMEOUT_MS });
  evidence.searchNoMatchShown = true;
  await page.fill("#relay-key-search", "Browser");
  await page.locator(".relay-key-list").filter({ hasText: "Browser access key" }).waitFor({ timeout: WAIT_TIMEOUT_MS });
  evidence.searchMatchesLabel = true;
  await page.fill("#relay-key-search", "");
  await page.locator(".relay-key-list").filter({ hasText: "Browser access key" }).waitFor({ timeout: WAIT_TIMEOUT_MS });

  // Revoke requires explicit confirmation; the row then shows Revoked and the
  // Edit/Revoke actions disappear. The dialog handler must be registered before
  // the click and auto-accept (a window.confirm blocks the click otherwise).
  let confirmMessage = "";
  page.once("dialog", (dialog) => {
    confirmMessage = dialog.message();
    dialog.accept();
  });
  await page.click('.relay-key-list [data-revoke-key]');
  evidence.confirmPrompt = confirmMessage.includes("撤销此中转访问密钥？");
  await page.waitForSelector("#relay-key-list .key-status-revoked", { timeout: WAIT_TIMEOUT_MS });
  evidence.revokedStatusShown = true;
  evidence.revokeActionCountAfterRevoke = await page.locator('.relay-key-list [data-revoke-key]').count();
  evidence.fullSecretAbsentAfterRevoke = !(await bodyText(page)).includes(secret);
  return evidence;
}

// ---------------------------------------------------------------------------
// UI-008/ROUTE-022: the Check interaction shows disabled/loading while the
// check runs, then the success state with the route restored to Available;
// the row offers no arbitrary prompt or target-model input.
// ---------------------------------------------------------------------------
async function routeCheckSuccess({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });
  await page.waitForSelector(".health-unavailable", { timeout: WAIT_TIMEOUT_MS });
  const checkButton = page.locator('.routes-table [data-check-route]');
  await checkButton.waitFor({ timeout: WAIT_TIMEOUT_MS });
  const row = checkButton.locator("xpath=ancestor::div[contains(@class,'table-row')]");
  const rowHasNoPromptInput = (await row.locator("input").count()) === 0;
  await checkButton.click();
  const disabledDuringCheck = await checkButton.isDisabled();
  const labelDuringCheck = (await checkButton.textContent()).trim();
  await page.waitForSelector(".health-available", { timeout: WAIT_TIMEOUT_MS });
  const finalHealth = (await page.locator(".routes-table .health").first().textContent()).trim();
  return { disabledDuringCheck, labelDuringCheck, rowHasNoPromptInput, finalHealth };
}

// ---------------------------------------------------------------------------
// UI-008: the Check button is disabled while the route is Checking (startup
// probe in progress), then Available.
// ---------------------------------------------------------------------------
async function routeCheckDisabled({ page, base, credential, newCredential, extra }) {
  await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });
  await page.waitForSelector(".health-checking", { timeout: WAIT_TIMEOUT_MS });
  const checkButton = page.locator('.routes-table [data-check-route]');
  const disabledWhileChecking = await checkButton.isDisabled();
  const titleWhileChecking = await checkButton.getAttribute("title");
  // The Operations view does not auto-refresh: wait for the startup probe to
  // complete (the Rust test passes the probe delay), then reload and poll
  // until the probe outcome is visible.
  const probeDelayMs = (extra && extra.probe_delay_ms) || 12000;
  await page.waitForTimeout(probeDelayMs + 1000);
  for (let attempt = 0; attempt < 8; attempt += 1) {
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });
    if (await page.locator(".health-available").count()) break;
    await page.waitForTimeout(2000);
  }
  await page.waitForSelector(".health-available", { timeout: WAIT_TIMEOUT_MS });
  const finalHealth = (await page.locator(".routes-table .health").first().textContent()).trim();
  return { disabledWhileChecking, titleWhileChecking, finalHealth };
}

// ---------------------------------------------------------------------------
// UI-008: a failed check (overlap with an in-flight recovery probe) shows a
// safe error message and leaves the button enabled for retry.
// ---------------------------------------------------------------------------
async function routeCheckError({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });
  await page.waitForSelector(".health-unavailable", { timeout: WAIT_TIMEOUT_MS });
  // Start a manual check from the onboarding checklist; its probe connects to
  // the scripted upstream and is held open, so the route now has an in-flight
  // check (the shared one-per-route guard, ROUTE-018). Synchronize on the
  // checklist button's disabled loading state rather than a fixed sleep.
  await page.click('.onboarding [data-check-route]');
  await page.waitForSelector('.onboarding [data-check-route]:disabled', { timeout: WAIT_TIMEOUT_MS });
  // A second manual check on the route row must be rejected with 409.
  const rowButton = page.locator('.routes-table [data-check-route]');
  await rowButton.click();
  await page.waitForSelector(".check-feedback:not(:empty)", { timeout: WAIT_TIMEOUT_MS });
  const feedbackText = (await page.locator(".check-feedback").textContent()).trim();
  const retryEnabled = await rowButton.isEnabled();
  const text = await bodyText(page);
  return { feedbackText, retryEnabled, text };
}

// ---------------------------------------------------------------------------
// UI-010/UI-011: a call row in Calls & usage shows the published model and the
// successful provider, and its detail expands a metadata-only model-route
// attempt chain — no request/response content.
// ---------------------------------------------------------------------------
async function callDetailChain({ page, base, credential, newCredential, extra }) {
  await signIn(page, base, credential, newCredential);
  await page.click('.navigation [data-view="usage"]');
  await page.waitForSelector(".calls-table", { timeout: WAIT_TIMEOUT_MS });
  const callRow = page.locator(".call-row").first();
  const callRowText = await callRow.innerText();
  await page.click(".call-row [data-call-toggle]");
  await page.waitForSelector(".model-route-attempt-chain", { timeout: WAIT_TIMEOUT_MS });
  await page.waitForSelector(".model-route-attempt-chain:visible", { timeout: WAIT_TIMEOUT_MS });
  const chainText = await page.locator(".model-route-attempt-chain").innerText();
  const attemptRowCount = await page.locator(".model-route-attempt-row").count();
  const text = await bodyText(page);
  const contentCanary = extra && extra.content_canary ? extra.content_canary : "";
  return {
    callRowText,
    chainText,
    attemptRowCount,
    bodyContainsContentCanary: contentCanary ? text.includes(contentCanary) : null,
    text,
  };
}

// ---------------------------------------------------------------------------
// UI-004: the six-step onboarding checklist is visible for an empty
// configuration and disappears once the whole chain is callable.
// ---------------------------------------------------------------------------
async function checklist({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  const visible = (await page.locator(".onboarding").count()) === 1;
  const steps = await page.locator(".checklist li").evaluateAll((items) =>
    items.map((item) => ({
      label: item.children[1] ? item.children[1].textContent.trim() : "",
      done: Boolean(item.querySelector(".check.complete")),
    })),
  );
  return { visible, stepCount: steps.length, steps, text: await bodyText(page) };
}

// ---------------------------------------------------------------------------
// UI-006: submitting an incomplete or invalid form renders field-attributed
// errors beside the offending inputs and keeps the panel open.
// ---------------------------------------------------------------------------
async function fieldErrors({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.click('.table-region [data-open-panel="route"]');
  await page.waitForSelector("#route-form", { timeout: WAIT_TIMEOUT_MS });
  // Disable the browser's native constraint validation so the server-side
  // validation is exercised (the server is the contract for CFG-011/CFG-012).
  await page.evaluate(() => {
    document.querySelector("#route-form").noValidate = true;
  });
  await page.selectOption('select[name="published_model_id"]', { index: 0 });
  await page.selectOption('select[name="provider_id"]', { index: 0 });

  // The store validates fail-fast on the first offending field, so each
  // invalid submission exercises one field attribution.
  const submitAndCollect = async () => {
    await page.click("#route-form button[type=submit]");
    await page.waitForSelector(".field-error", { timeout: WAIT_TIMEOUT_MS });
    const errors = await page.locator(".field-error").evaluateAll((elements) =>
      elements.map((error) => {
        const container = error.closest("label") || error.closest("fieldset");
        const input = container ? container.querySelector("input, select") : null;
        return { field: input ? input.name : null, message: error.textContent.trim() };
      }),
    );
    const generalError = (await page.locator("#panel-error").textContent()).trim();
    return { errors, generalError };
  };

  // Submission 1: blank (whitespace-only) upstream model name.
  await page.fill('input[name="upstream_model_name"]', "   ");
  await page.fill('input[name="cost_multiplier"]', "1");
  const first = await submitAndCollect();
  // Submission 2: non-positive cost multiplier.
  await page.fill('input[name="upstream_model_name"]', "valid-model-name");
  await page.fill('input[name="cost_multiplier"]', "0");
  const second = await submitAndCollect();
  const panelStillOpen = (await page.locator("#route-form").count()) === 1;

  // Submission 3: a valid route, so the relay-key eligibility form can be
  // exercised. The provider endpoint is dead, but route creation does not
  // fail on a failed probe.
  await page.fill('input[name="cost_multiplier"]', "1");
  await page.click("#route-form button[type=submit]");
  await page.waitForSelector(".routes-table", { timeout: WAIT_TIMEOUT_MS });

  // UI-006: a relay access key with no eligible model routes is rejected with
  // a field-attributed error beside the eligibility group and never becomes
  // callable.
  await page.click('.relay-key-region [data-open-panel="relay-key"]');
  await page.waitForSelector("#relay-key-form", { timeout: WAIT_TIMEOUT_MS });
  await page.fill('input[name="label"]', "No-eligibility key");
  await page.click("#relay-key-form button[type=submit]");
  await page.waitForSelector("#relay-key-form .field-error", { timeout: WAIT_TIMEOUT_MS });
  const keyErrors = await page.locator("#relay-key-form .field-error").evaluateAll((elements) =>
    elements.map((error) => ({ message: error.textContent.trim() })),
  );

  return { first, second, keyErrors, panelStillOpen };
}

// ---------------------------------------------------------------------------
// UI-012: the Data security panel shows backup metadata and a manual create
// action, with no cloud, download or delete controls.
// ---------------------------------------------------------------------------
async function dataSecurityPanel({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.locator("[data-open-backups]").first().click();
  await page.waitForSelector("#create-backup", { timeout: WAIT_TIMEOUT_MS });
  const title = (await page.locator(".focused-panel .panel-heading h2").textContent()).trim();
  const backupSummaryCount = await page.locator(".backup-summary").count();
  const panelText = await page.locator(".focused-panel").innerText();
  await page.click("#create-backup");
  await page.waitForSelector(".backup-table .table-row", { timeout: WAIT_TIMEOUT_MS });
  const rowsAfterCreate = await page.locator(".backup-table .table-row").count();
  return { title, backupSummaryCount, panelText, rowsAfterCreate };
}

// ---------------------------------------------------------------------------
// UI-012/OPS-015: a failed explicit restore reports its exact stage and an
// actionable reason, keeps the current database selected, and returns the
// operator to Operations.
// ---------------------------------------------------------------------------
async function restoreFailurePanel({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.locator("[data-open-backups]").first().click();
  await page.waitForSelector("[data-restore-backup]", { timeout: WAIT_TIMEOUT_MS });
  // Register the confirm handler before the click and auto-accept (a
  // window.confirm blocks the click action otherwise).
  let confirmMessage = "";
  page.once("dialog", (dialog) => {
    confirmMessage = dialog.message();
    dialog.accept();
  });
  await page.click("[data-restore-backup]");
  await page.waitForSelector("[data-restore-failed]", { timeout: WAIT_TIMEOUT_MS });
  const failureText = await page.locator("[data-restore-failed]").innerText();
  const hasReturnAction = (await page.locator("#restore-return").count()) === 1;
  await page.click("#restore-return");
  await page.waitForSelector(".shell h1", { timeout: WAIT_TIMEOUT_MS });
  const backH1 = await h1Text(page);
  return { confirmMessage, failureText, hasReturnAction, backH1 };
}

// ---------------------------------------------------------------------------
// OPS-010: an abnormal Operations status area drills into its 14-day event
// history from the same page.
// ---------------------------------------------------------------------------
async function statusAreaEventHistory({ page, base, credential, newCredential }) {
  await signIn(page, base, credential, newCredential);
  await page.waitForSelector(".health-unavailable", { timeout: WAIT_TIMEOUT_MS });
  await page.click('[data-open-events="routes"]');
  await page.waitForSelector(".events-table", { timeout: WAIT_TIMEOUT_MS });
  const title = (await page.locator(".focused-panel .panel-heading h2").textContent()).trim();
  const eventRows = await page.locator(".events-table .table-row").count();
  const tableText = await page.locator(".events-table").innerText();
  return { title, eventRows, tableText };
}

const scenarios = {
  "login-default-view": loginDefaultView,
  "usage-secondary-view": usageSecondaryView,
  "route-groups": routeGroups,
  "focus-panels": focusPanels,
  "relay-key": relayKey,
  "route-check-success": routeCheckSuccess,
  "route-check-disabled": routeCheckDisabled,
  "route-check-error": routeCheckError,
  "call-detail-chain": callDetailChain,
  "checklist": checklist,
  "field-errors": fieldErrors,
  "data-security-panel": dataSecurityPanel,
  "restore-failure-panel": restoreFailurePanel,
  "status-area-event-history": statusAreaEventHistory,
};

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const scenario = scenarios[args.scenario];
  if (!scenario) fail(`unknown scenario: ${args.scenario}`);
  if (!args.base || !args.credential) fail("--base and --credential are required");
  const browser = await launchBrowser();
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    // A global safety net with a clearable timer: the race timer must not keep
    // the node event loop alive after the scenario completes (that would delay
    // the driver's exit by the full timeout).
    let timeoutId;
    const timeoutPromise = new Promise((_, reject) => {
      timeoutId = setTimeout(() => reject(new Error(`scenario ${args.scenario} timed out`)), 90000);
    });
    const evidence = await Promise.race([
      scenario({ page, base: args.base, credential: args.credential, newCredential: args.newCredential, extra: args.extra }),
      timeoutPromise,
    ]).finally(() => clearTimeout(timeoutId));
    console.log(JSON.stringify({ ok: true, evidence }));
  } catch (error) {
    fail(error && error.message ? error.message : String(error));
  } finally {
    trace("browser.close: start");
    await browser.close();
    trace("browser.close: done");
    // The open trace stream would keep the node event loop alive forever and
    // the Rust harness waiting on the driver's exit; close it explicitly.
    if (traceStream) traceStream.end();
  }
}

main();
