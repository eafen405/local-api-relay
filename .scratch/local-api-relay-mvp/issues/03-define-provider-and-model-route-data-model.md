# Define the Provider and Model Route Data Model

Type: grilling
Status: resolved
Blocked by: 01, 02

## Question

What is the smallest persistent model for upstream providers, published models, explicit model mappings, model routes, relay access keys and their per-model eligible route sets, preset model pricing, cost multipliers, upstream secrets, and system-owned health state?

## Answer

Use six persistent logical records. Keep the model storage-engine-neutral until
the implementation foundation is chosen.

1. **Upstream provider**: stable ID, display name, Base URL, and one upstream
   API key. This is a callable connection configuration, not a vendor catalog.
   Base URLs need not be unique: two differently priced keys for the same URL
   are represented as two upstream providers so they can route, fail, and
   recover independently. The upstream key remains plaintext in the
   OS-user-protected local data file and is masked everywhere else.
2. **Published model**: stable ID, unique client-visible model name, and its
   owned preset base-price fields. The exact price dimensions and update source
   belong to [Define the Model Catalog and Cost Accounting Boundary](09-define-model-catalog-and-cost-accounting.md).
3. **Model route**: stable ID, published-model ID, upstream-provider ID,
   explicit upstream model name, protocol (`chat_completions` or `responses`),
   and a positive fixed-precision cost multiplier. There is no separate model
   mapping entity: the upstream model name on this record is the explicit
   mapping. Route identity is unique across `(published model, upstream
   provider, upstream model name, protocol)`. Chat Completions and Responses
   are separate routes even when they target the same upstream model, so they
   have independent health.
4. **Relay access key**: stable ID, label, recognizable non-secret prefix,
   secret hash, creation time, and optional revocation time. Show the complete
   secret once at creation; never persist or redisplay it.
5. **Route eligibility**: the unique many-to-many association `(relay access
   key, model route)`. Because every route belongs to exactly one published
   model, these rows directly form each key's per-model eligible route set; no
   separate key-to-model or eligible-set container is needed.
6. **Route health**: one system-owned record per model route. It retains the
   last known state plus minimal failure/check/recovery metadata; management
   cannot edit it as configuration. On startup the relay checks every route and
   rebuilds the candidate pools from current results. During operation, healthy
   routes are not periodically probed; temporarily unavailable routes are
   periodically checked and rejoin multiplier ordering after recovery. Exact
   transitions, retry intervals, and the user-selectable token-consuming probe
   option belong to [Define the Route Failure and Recovery State Machine](04-define-route-failure-and-recovery-state-machine.md).

For a request, first select routes matching the requested published model and
native protocol, intersect them with the relay key's route eligibility, remove
non-healthy routes, then order the remainder by ascending cost multiplier with
a deterministic stable-ID tie-breaker. A missing published model, missing
protocol-specific mapping, or empty eligible healthy set fails explicitly.
