# Validate the Local Management Workflow

Type: prototype
Status: resolved
Blocked by: 03, 04, 06

## Question

Does a minimal local Web workflow make it clear how to add an upstream provider, select supported models, map published names, set default or per-route cost multipliers, create or revoke relay access keys with per-model eligible routes, inspect health, and correct invalid configuration without exposing CC Switch-style complexity?

## Answer

Use an **Operations console** as the primary management surface, with a
**Guided setup** flow embedded in first-run and add/edit actions.

- The default view is a dense, model-route-focused console showing upstream
  providers, published models, route cost multipliers, route health and last
  check/failure information, plus relay access keys and their model scope.
- When the configuration is empty or incomplete, show a five-step checklist:
  add an upstream provider, select supported models, create explicit
  published-to-upstream mappings and protocol, set a positive cost multiplier,
  assign eligible routes to relay access keys, then validate and enable.
- New provider, model-route, and access-key actions open the guided flow as a
  focused panel and return to the operations console after saving. Validation
  prevents an incomplete mapping, invalid multiplier, or key with no eligible
  route from becoming callable.
- Route rows expose the system-owned `Available`, `Checking`, and `Temporarily
  unavailable` states and offer health-neutral configuration correction or a
  recovery probe where appropriate. Upstream API keys remain masked; relay
  access keys remain separate from the administrator credential.

The throwaway artifact that established this decision is
[Management Workflow Prototype](../prototypes/management-workflow.html).

## Comments

- Throwaway UI prototype: [management-workflow.html](../prototypes/management-workflow.html). Open it directly and switch with `?variant=A`, `?variant=B`, or `?variant=C`.
- Variant A is a dense operations console; B is a linear setup checklist; C is a published-model-first route matrix. All three show provider setup, explicit model mapping, cost multipliers, per-key route eligibility, health states, and invalid/unavailable configuration affordances.
- The inline script passes a Node VM syntax check. A localhost preview server could not be started in this sandbox because the required socket permission escalation was rejected.
