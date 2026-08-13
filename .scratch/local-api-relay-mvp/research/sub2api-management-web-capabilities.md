# Sub2API Management Web Capabilities for the Local Relay MVP

## Research question

Which current Sub2API management-Web pages, information hierarchy, and
interaction patterns are useful functional references for the capabilities
already decided by the local relay map, and which Sub2API-only product scope
must be omitted?

## Source basis and freshness limit

The refresh was performed against the supplied first-party source snapshot at
`tmp/sub2api-0.1.173/sub2api-0.1.173`, inspected on 2026-08-10. Its checked-in
server version is `0.1.172` (`backend/cmd/server/VERSION`, line 1); the
directory name is therefore not treated as a release identifier. The snapshot
has no `.git` directory or other usable commit metadata, so it cannot provide
an immutable upstream commit SHA or prove that it is the latest upstream
release. A sorted path-and-content manifest of its 3459 files hashes to
`0815606e098e4a634b3676b77dbf06707712c4b242519511997c1321f0cd806d`.
The local source links below are the primary evidence for this refresh. The
historical official `0.1.168` commit links retained in the correspondence table
are useful cross-checks, but do not override the supplied snapshot or establish
current-remote freshness. No secondary sources were substituted.

Local source root: [`tmp/sub2api-0.1.173/sub2api-0.1.173`](../../../tmp/sub2api-0.1.173/sub2api-0.1.173).

## Local snapshot cross-check (directory label 0.1.173; metadata 0.1.172)

The supplied snapshot confirms the correspondence and exclusions below, but it
also shows that several Sub2API surfaces have become broader. Those additions
strengthen the exclusions; they do not enlarge the relay MVP:

- **Hierarchy and progressive disclosure:** admin routes include Dashboard,
  Ops, Users, Groups, Channels, Accounts, Settings, risk/prompt audit, and Usage
  ([router/index.ts, lines 399-623](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/router/index.ts#L399-L623));
  the sidebar feature-gates entries, nests channel pricing/monitoring, and
  supports a reduced simple mode
  ([AppSidebar.vue, lines 692-836](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/components/layout/AppSidebar.vue#L692-L836)).
  The old conclusion is unchanged: collapse these pages into Operations, Calls
  & usage, and focused panels rather than copying the page inventory.
- **Summary and in-place operations:** Dashboard renders API-key, account,
  request, user, token, cost, and performance cards
  ([DashboardView.vue, lines 9-190](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/DashboardView.vue#L9-L190)).
  Ops now combines selectable filters/ranges, auto-refresh, fullscreen,
  concurrency, switch/throughput trends, latency/errors, alerts, logs, and
  request/error dialogs
  ([OpsDashboard.vue, lines 13-135](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ops/OpsDashboard.vue#L13-L135)).
  Only the summary-to-detail and in-place-dialog patterns transfer; realtime
  platform analytics, alerting, and wallboard behavior remain omitted.
- **Dense management tables have grown more product-specific:** Accounts keeps
  filter/search and refresh/create, but also adds auto-refresh, sync/import/
  export, column controls, bulk operations, a virtualized status/usage table,
  and scheduled-test/statistics modals
  ([AccountsView.vue, lines 5-193](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/AccountsView.vue#L5-L193),
  [lines 194-327](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/AccountsView.vue#L194-L327),
  [lines 432-484](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/AccountsView.vue#L432-L484)).
  Channels retains the useful status/search/refresh/create/table/empty-state
  grammar, but its editor is a multi-platform pricing surface with model
  restrictions and domain-specific validation
  ([ChannelsView.vue, lines 5-145](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ChannelsView.vue#L5-L145),
  [lines 141-220](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ChannelsView.vue#L141-L220),
  [lines 1441-1569](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ChannelsView.vue#L1441-L1569)).
  Reuse the table/dialog grammar, not bulk data movement, provider schedulers,
  groups, proxies, pricing rules, or channel entities.
- **Guided workflow and probe remain patterns only:** the admin guide anchors a
  group -> account -> key tour to real controls and filters group steps in
  simple mode
  ([Guide/steps.ts, lines 9-243](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/components/Guide/steps.ts#L9-L243)).
  `AccountsView` imports the admin test component
  (`frontend/src/views/admin/AccountsView.vue`, lines 456 and 513); that dialog
  selects arbitrary models/modes, accepts prompts and image/audio uploads, and
  streams copyable terminal-like output
  ([admin/account/AccountTestModal.vue, lines 44-225](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/components/admin/account/AccountTestModal.vue#L44-L225)).
  Adapt only its in-context idle/loading/success/error/retry state; the relay
  probe remains fixed-protocol and metadata-only.
- **Usage drill-down is now an unrestricted analytics surface:** Usage combines
  an arbitrary date picker with model/group/endpoint distributions, a token
  trend, usage/errors/ranking tabs, filters, pagination, export, and cleanup
  ([UsageView.vue, lines 7-181](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/UsageView.vue#L7-L181));
  the toolbar exposes user, API-key, model, account, group, request/billing,
  cleanup, and export controls
  ([UsageFilters.vue, lines 7-190](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/components/admin/usage/UsageFilters.vue#L7-L190)).
  Rows can render raw inbound/upstream endpoints, image/cache token details,
  cost/latency, user agent, client IP, and geolocation
  ([UsageTable.vue, lines 99-250](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/components/admin/usage/UsageTable.vue#L99-L250)).
  The relay's six windows, metadata allowlist, and no export/cleanup are local
  decisions, not claims about current Sub2API behavior.
- **Backup is broader than a local data-safety panel:** Settings mounts Backup
  as a tab
  ([SettingsView.vue, lines 8577-8580](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/SettingsView.vue#L8577-L8580),
  [lines 8764-8775](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/SettingsView.vue#L8764-L8775)).
  Backup configures S3/R2, separate image storage, cron/retention, manual
  create/refresh, download, restore, and delete
  ([BackupView.vue, lines 3-168](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/BackupView.vue#L3-L168),
  [lines 170-268](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/BackupView.vue#L170-L268)).
  These controls strengthen the cloud-storage and arbitrary-retention omission;
  the relay keeps only its already-decided protected local snapshot workflow.
- **Stored-key and privacy divergences remain:** Keys offers group/status
  filtering, copy of stored values, quotas/rate limits, and a Use Key modal fed
  the selected key
  ([KeysView.vue, lines 1-80](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/user/KeysView.vue#L1-L80),
  [lines 97-233](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/user/KeysView.vue#L97-L233),
  [lines 991-999](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/user/KeysView.vue#L991-L999)).
  Usage/error tables still expose endpoint, user-agent, IP/geolocation, and
  message text, including parsed arbitrary JSON error/message/detail fields
  ([OpsErrorLogTable.vue, lines 125-150](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ops/components/OpsErrorLogTable.vue#L125-L150),
  [lines 313-332](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/frontend/src/views/admin/ops/components/OpsErrorLogTable.vue#L313-L332)).
  Hash-only key persistence and the relay diagnostic allowlist remain hard
  boundaries.
- **Product scope signal:** the snapshot README describes API gateway features
  including multi-account OAuth/API-key management, billing, smart scheduling,
  concurrency/rate limiting, payments, Web administration, Composite Groups,
  and external integration
  ([README_CN.md, lines 172-186](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/README_CN.md#L172-L186),
  [README.md, lines 169-184](../../../tmp/sub2api-0.1.173/sub2api-0.1.173/README.md#L169-L184)).
  Those product entities and controls remain explicit omissions for the local
  single-user relay.

## Finding

Sub2API is useful as an **interaction-pattern library**, not as a page inventory
or domain model. Its first-party description is a multi-account API gateway
that also owns authentication, billing, load balancing, key distribution,
concurrency/rate limits, payments, and a Web administration surface
([README_CN.md, lines 173-187](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/README_CN.md#L173-L187)).
The relay MVP is intentionally a single-user, local-only service with a much
smaller persistent graph. Therefore the correspondence is many Sub2API pages
collapsed into two durable relay views plus focused panels:

1. **Operations** remains the landing view decided by
   [Validate the Local Management Workflow](../issues/08-validate-the-local-management-workflow.md).
   It combines the useful parts of Sub2API's Admin Dashboard, Accounts, and Ops
   pages: persistent system status, published-model route rows, upstream
   provider identity and health, access-key scope, and direct repair/probe
   actions.
2. **Calls & usage** is the one secondary high-density view. It combines the
   selected-window summaries, distributions, metadata-only call rows, and
   drill-down behavior needed by
   [Define the Model Catalog and Cost Accounting Boundary](../issues/09-define-model-catalog-and-cost-accounting.md)
   and
   [Define Operational Diagnostics and Retention](../issues/12-define-operational-diagnostics-and-retention.md).
3. **Guided configuration** and **data safety** remain focused panels/dialogs
   opened from Operations. They do not need Sub2API's separate Accounts,
   Groups, Channels, Keys, Settings, and Backup navigation entries.

This is a functional correspondence only. It does not add pages, entities,
metrics, or actions beyond the closed relay decisions.

## Lean capability correspondence

| Relay capability already decided | First-party Sub2API reference | Pattern to carry forward, adapted to the relay | Explicitly not inherited |
| --- | --- | --- | --- |
| Operations-first information hierarchy | Sub2API defines separate admin routes for Dashboard, Ops, users, groups, channels, accounts, settings, risk control, prompt audit, and usage ([router, lines 399-623](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/router/index.ts#L399-L623)); its sidebar then groups those entries and can hide much of them in simple mode ([AppSidebar, lines 695-831](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/layout/AppSidebar.vue#L695-L831)). | Reuse a stable shell, a compact navigation hierarchy, and progressive disclosure. Make Operations the default and avoid navigation for concepts that are only edit/detail panels. | Do not reproduce Sub2API's admin/personal split or its full sidebar taxonomy. |
| At-a-glance relay state | Sub2API's Admin Dashboard leads with API-key, upstream-account, request, token, and cost summaries ([DashboardView, lines 9-71](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/DashboardView.vue#L9-L71), [lines 98-163](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/DashboardView.vue#L98-L163)). Its Ops page places a filterable header above concurrency, switching, throughput, latency, error, token, alert, and system-log sections, with detail dialogs rather than navigating away ([OpsDashboard, lines 13-133](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ops/OpsDashboard.vue#L13-L133)). | Reuse the summary-to-detail hierarchy and in-place drill-down. The relay's top summary is the already-decided five-area status strip: Storage, model routes, backups, migration/restore, and usage completeness. | Do not add Sub2API's health score, realtime QPS, concurrency, alert rules, fullscreen wallboard, or broad platform analytics. |
| Upstream-provider and model-route management | Sub2API's Accounts view uses one dense, filterable table with refresh/create actions and rows that combine identity, platform, health/status, schedulability, current usage, and row actions ([AccountsView, lines 1-18](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/AccountsView.vue#L1-L18), [lines 177-318](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/AccountsView.vue#L177-L318)). Its Channels view pairs search/status filters and create/edit actions with an empty-state CTA and a focused edit dialog ([ChannelsView, lines 1-175](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ChannelsView.vue#L1-L175)). | Reuse the dense table grammar, visible status, explicit refresh, empty-state CTA, and focused add/edit dialog. Group rows by **published model**, then show each protocol-specific model route with upstream provider, upstream model, protocol, multiplier, health, last failure/check, and next probe. | Do not copy Sub2API's Accounts/Groups/Channels entities. The relay keeps only upstream providers, published models, model routes, route eligibility, access keys, and route health from [Define the Provider and Model Route Data Model](../issues/03-define-provider-and-model-route-data-model.md). |
| Guided first-run and add/edit flow | Sub2API's interactive administrator guide moves through real group, upstream-account, and key controls, including names, platform/type, multiplier/priority, associations, and submit actions ([Guide steps, lines 22-226](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/Guide/steps.ts#L22-L226)). | Reuse task-oriented guidance anchored to the actual controls. Adapt the sequence to provider -> supported published model -> explicit upstream model/protocol route -> positive multiplier -> access-key eligibility -> validate and enable, exactly as already decided. | Do not copy the group-first business workflow, interactive product tour text, or Sub2API's platform/account-type branching. |
| Route check and repair action | Sub2API opens a focused connection-test dialog, requires a model selection, shows connecting/success/error states, and offers retry without leaving the account list ([AccountTestModal, lines 1-120](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/account/AccountTestModal.vue#L1-L120), [lines 198-238](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/account/AccountTestModal.vue#L198-L238)). | Reuse the in-context, stateful probe dialog. Its relay action runs the already-defined native-protocol recovery probe and reports only safe metadata. | Do not expose a prompt editor, generated content, raw terminal output, or a user-selected arbitrary model. Probe semantics remain those in [Define the Route Failure and Recovery State Machine](../issues/04-define-route-failure-and-recovery-state-machine.md). |
| Relay access-key lifecycle and scope | Sub2API's Keys view combines search/status/group filters, create action, table status and usage, a focused create/edit form, and destructive confirmation ([KeysView, lines 1-175](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L1-L175), [lines 372-475](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L372-L475), [lines 911-965](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L911-L965)). | Reuse searchable key rows, clear state, focused creation, route-scope selection, and confirmation for revocation. Keep the section reachable from Operations because keys determine route eligibility. | Sub2API renders and copies stored key values and passes them to a use-key dialog ([KeysView, lines 97-120](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L97-L120), [lines 992-998](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L992-L998)); the relay must instead show a complete relay secret only once and persist only its hash. Do not inherit quotas, rate limits, IP restrictions, or groups. |
| Usage summaries and distributions | Sub2API's Usage view places selected-range controls above model/group/endpoint distributions and a token trend, then keeps usage, errors, and ranking in a single tabbed detail area with filters and a paginated table ([UsageView, lines 1-164](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/UsageView.vue#L1-L164)). Its summary cards show requests, total/input/output/cache tokens, cost, and average duration ([UsageStatsCards, lines 1-87](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/admin/usage/UsageStatsCards.vue#L1-L87)). | Reuse a selected-window summary followed by distributions and one paginated call table. Keep only the relay's six windows, token share by published model/upstream provider, RMB estimated charge, and cache hit rate. | Do not add endpoint distributions, user rankings, standard-vs-account billing comparisons, arbitrary date ranges, exports, or cleanup controls. |
| Metadata-only call row | Sub2API's usage table shows a requested-to-upstream model hierarchy, separate upstream-account identity, token/cache details, cost, first-token time, completion duration, and call time ([UsageTable, lines 47-73](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/admin/usage/UsageTable.vue#L47-L73), [lines 113-220](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/admin/usage/UsageTable.vue#L113-L220)). | Reuse the scannable hierarchy: published model as the primary label, successful upstream provider beneath it, then total/cache tokens, estimated RMB charge, completion latency, streaming-only first-token latency, and time. | Do not import Sub2API's extra token/billing dimensions or imply that failed attempts are charged. Failed relay calls use `-` for unknowns and remain outside token/cost aggregates. |
| Fallback and abnormal-call drill-down | Sub2API's Ops request-details dialog shows time, success/error kind, platform, model, duration, status, request ID, and a link to an error detail, in responsive list/table forms ([OpsRequestDetailsModal, lines 155-319](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ops/components/OpsRequestDetailsModal.vue#L155-L319)). | Reuse the row-to-detail modal interaction only. The relay detail is its own ordered, metadata-only route-attempt chain and stays under one downstream call record. | The inspected Sub2API dialog is a flat request list, not the relay's attempt-chain contract. Do not infer fallback semantics or fields from it. |
| Backup and restore operations | Sub2API embeds Backup as a Settings tab rather than a top-level route ([SettingsView, lines 7971-7977](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/SettingsView.vue#L7971-L7977), [lines 8157-8167](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/SettingsView.vue#L8157-L8167)). Its operations section shows explicit create/refresh controls and backup rows with state, size, trigger, start time, and restore action ([BackupView, lines 170-268](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/BackupView.vue#L170-L268)). | Reuse a focused data-safety panel reached from the Operations status strip: safe backup metadata, manual backup, explicit restore selection, stage/progress, and actionable failure. | Do not add cloud-object-storage setup, arbitrary schedules/retention, backup download, or manual deletion. The local relay's automatic 24-hour policy, 10-snapshot rotation, protected paths, and backup-gated migrations remain authoritative. |

## Interaction patterns worth reusing

The following patterns are supported by the cited first-party source and fit
the already-decided relay workflows:

- **Dense table page with a quiet action strip**: search/filter on the left,
  refresh and create on the right, explicit status in rows, compact row actions,
  pagination, and an empty-state action. The Channels and Keys views provide
  direct examples
  ([ChannelsView, lines 1-139](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ChannelsView.vue#L1-L139),
  [KeysView, lines 1-92](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L1-L92)).
- **Focused create/edit dialogs** that preserve list context and return to the
  same operational surface after saving
  ([ChannelsView, lines 141-175](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ChannelsView.vue#L141-L175),
  [KeysView, lines 447-475](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/user/KeysView.vue#L447-L475)).
- **Summary -> filtered detail -> modal drill-down** for operational data. The
  Usage view keeps summary charts and tabs over one detail table; Ops keeps
  overview sections in place and opens request/error dialogs
  ([UsageView, lines 1-164](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/UsageView.vue#L1-L164),
  [OpsDashboard, lines 42-133](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ops/OpsDashboard.vue#L42-L133)).
- **Stateful manual checks** with disabled/loading/success/error/retry states,
  adapted to the relay's fixed recovery probe
  ([AccountTestModal, lines 80-120](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/account/AccountTestModal.vue#L80-L120),
  [lines 198-238](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/account/AccountTestModal.vue#L198-L238)).
- **Guidance attached to real actions**, useful for the relay's incomplete-state
  checklist and add/edit panels
  ([Guide steps, lines 22-226](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/Guide/steps.ts#L22-L226)).

## Hard omissions and privacy divergences

These are not future relay requirements; they are Sub2API-only scope that the
MVP must deliberately exclude.

- **Multi-tenant commerce and governance**: users, roles/groups, subscriptions,
  payments/orders, redemption/promo codes, affiliates, announcements, audit,
  risk control, prompt audit, and external/custom pages. The first-party router
  exposes these as distinct product surfaces
  ([router, lines 189-397](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/router/index.ts#L189-L397),
  [lines 399-623](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/router/index.ts#L399-L623),
  [lines 626-700](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/router/index.ts#L626-L700)).
- **Provider-account complexity**: OAuth account types, quotas, concurrency,
  session stickiness, schedulability controls, proxies, bulk import/export,
  platform-specific transformations, and arbitrary account testing. Sub2API's
  own overview makes multi-account management, concurrency, rate limiting, and
  intelligent scheduling core product features
  ([README_CN.md, lines 177-187](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/README_CN.md#L177-L187)).
- **Sensitive diagnostic fields**: Sub2API's usage/error tables can display raw
  upstream endpoints, user agents, client IPs/geolocation, and message text
  ([UsageTable, lines 81-90](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/admin/usage/UsageTable.vue#L81-L90),
  [lines 223-233](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/components/admin/usage/UsageTable.vue#L223-L233),
  [OpsErrorLogTable, lines 125-150](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ops/components/OpsErrorLogTable.vue#L125-L150)).
  Its message formatter also extracts and renders arbitrary JSON `error`,
  `message`, or `detail` text
  ([OpsErrorLogTable, lines 313-332](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/ops/components/OpsErrorLogTable.vue#L313-L332)).
  The relay instead uses the strict metadata allowlist and normalized categories
  from [Define Operational Diagnostics and Retention](../issues/12-define-operational-diagnostics-and-retention.md).
- **Cloud backup product surface**: Sub2API configures S3/R2 credentials,
  separate image object storage, arbitrary cron and retention, and download or
  delete actions
  ([BackupView, lines 1-168](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/BackupView.vue#L1-L168),
  [lines 233-256](https://github.com/Wei-Shaw/sub2api/blob/5a6143097db142b72a6fc848c214e97214470bdd/frontend/src/views/admin/BackupView.vue#L233-L256)).
  The relay exposes only protected local snapshot metadata, manual creation,
  and explicit restore under
  [Define the Persistence, Backup, and Migration Contract](../issues/10-define-persistence-backup-and-migration-contract.md).
- **Visual cloning**: Sub2API's CSS, branding, colors, component density, and
  responsive composition are not requirements. The evidence above supports
  workflows and information relationships only.

## Handoff rule

When assembling the MVP specification, cite this correspondence to explain why
the relay uses an Operations-first console, focused guided panels, a dedicated
Calls & usage view, and progressive metadata-only drill-down. For behavior,
data fields, retention, security, and recovery semantics, always cite the
closed relay decision ticket rather than Sub2API. Sub2API is evidence that the
interaction patterns are workable; it is not the owner of the relay contract.
