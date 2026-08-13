# Map Sub2API Management Web Capabilities to the Relay MVP

Type: research
Status: resolved
Blocked by:

## Question

Using current first-party Sub2API documentation, source code, and management
Web assets, which pages, information hierarchy, and interaction patterns cover
the local relay capabilities already decided by this map? Produce a lean
capability correspondence that identifies what the MVP should use as a
functional reference and what Sub2API product scope it must omit; do not treat
visual similarity or Sub2API-only features as new relay requirements.

## Answer

Use Sub2API as an interaction-pattern reference, not as the relay's page
inventory or domain model. Consolidate the useful parts of its Dashboard,
Accounts, Ops, Keys, Usage, and Settings/Backup surfaces into the relay's
Operations landing view, one Calls & usage view, and focused guided
configuration/data-safety panels. Reuse dense filterable tables, explicit
status, focused add/edit and probe dialogs, summary-to-detail drill-down, and
guidance attached to real controls.

The refresh against the supplied snapshot (`tmp/sub2api-0.1.173`, server
version `0.1.172`) confirms the correspondence and makes the exclusions more
explicit: omit multi-tenant users/groups, commerce, provider-account pooling,
advanced scheduling/concurrency, cloud S3/R2 backup controls, arbitrary usage
export/cleanup, raw endpoint/message/IP/UA diagnostics, and Sub2API's persisted
key redisplay. Relay behavior, fields, privacy, and retention remain owned by
the already-resolved local decisions.

Full primary-source correspondence and the source-basis freshness limit:
[Sub2API Management Web Capabilities for the Local Relay MVP](../research/sub2api-management-web-capabilities.md).
