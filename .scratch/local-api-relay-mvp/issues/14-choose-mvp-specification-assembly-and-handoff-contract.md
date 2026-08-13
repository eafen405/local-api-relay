# Choose the MVP Specification Assembly and Handoff Contract

Type: grilling
Status: resolved
Blocked by: 13

## Question

What canonical document structure, requirement granularity, acceptance
evidence, and handoff boundary should assemble the resolved map decisions and
the Sub2API functional reference into an implementation-ready MVP
specification without turning the planning map itself into the specification?

## Answer

Use `.scratch/local-api-relay-mvp/spec.md` as the single canonical,
self-contained normative contract for the MVP. The planning map and its
decision tickets retain rationale and source evidence, but they are not part of
the build contract. An implementation session should be able to work from the
specification and its assigned implementation ticket; following map links must
be optional background reading rather than a prerequisite for discovering a
requirement.

The specification has this canonical structure:

1. **Goal, success boundary, and scope**: define the product outcome, supported
   environment, in-scope capabilities, and explicit exclusions.
2. **Domain language and system context**: carry forward the necessary terms
   from `CONTEXT.md`, identify the caller, relay, administrator, upstreams, and
   trust boundaries, and state the system-wide invariants.
3. **Normative requirements grouped by capability**: cover the downstream API
   contract; administration and authentication; provider, published-model,
   route, eligibility, and pricing configuration; selection, Fallback, route
   health, and recovery; persistence, migration, backup, and restore; calls,
   usage, diagnostics, and retention; management workflows; and packaging,
   lifecycle, and WSL2/Windows reachability.
4. **Acceptance and evidence matrix**: map every requirement ID to repeatable
   evidence and identify the few system-boundary checks that require recorded
   manual evidence.
5. **Decision traceability appendix**: map each resolved wayfinder decision to
   the requirement IDs that carry it. The links explain why; they do not add
   normative behavior. Sub2API and CC Switch remain non-normative references,
   so no behavior is inherited unless a requirement states it.

Give every normative requirement a stable, capability-prefixed ID such as
`API-001`, `ROUTE-001`, `SEC-001`, `DATA-001`, `OPS-001`, `UI-001`, or
`PKG-001`. Each requirement states one observable behavior or invariant with
normative language, its triggering conditions, and its failure or boundary
behavior where applicable. Split independently testable behaviors instead of
bundling them into narrative user stories. Requirements may constrain data
relationships and cross-module semantics, but they do not prescribe Rust
module names, function layouts, or other reversible code organization.

Evidence is requirement-level and automation-first. Each requirement names a
repeatable test, inspection, or build/runtime check capable of proving it;
scenario-heavy behavior includes concrete success, rejection, failure, and
recovery cases. Manual evidence is allowed only where the real boundary cannot
be represented reliably in the ordinary automated suite, notably Windows to
WSL2 reachability, browser launch, login-task lifecycle, installation,
upgrade, and restore drills. Such evidence must record the environment,
procedure, expected observation, and actual result. A global test plan may
summarize commands, but it cannot replace the per-requirement mapping.

The specification is ready to hand to `/to-tickets` only when:

- every in-scope behavior and cross-module constraint has a stable requirement
  ID and acceptance evidence;
- externally visible protocols, domain invariants, security boundaries,
  failure and recovery semantics, durability, retention, lifecycle behavior,
  and required management workflows are decided;
- all resolved map decisions are represented in the traceability appendix and
  do not conflict with `CONTEXT.md`;
- exclusions are explicit and no normative `TODO`, `TBD`, unresolved product
  choice, or implicit dependency on a reference product remains; and
- the resulting implementation tickets can be reviewed against the spec
  without reopening product decisions.

Implementation tickets may still choose module seams, internal APIs, concrete
function organization, and other reversible technical details, provided those
choices preserve the specified contracts and evidence. If ticketing exposes a
missing product, protocol, data, security, or other cross-ticket decision, the
specification is not implementation-ready and must be amended before that work
continues.
