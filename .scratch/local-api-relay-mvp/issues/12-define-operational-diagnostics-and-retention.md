# Define Operational Diagnostics and Retention

Type: grilling
Status: resolved
Blocked by: 04, 08, 09, 10, 11

## Question

Which persistence-degradation, route-state, backup, migration, restore, and
usage-accounting signals must the operations console and local logs expose, and
how should those records be retained and redacted so the administrator can
distinguish upstream faults from local data-loss risk without leaking secrets?

## Answer

Use two operator-facing record families: metadata-only call records for normal
usage inspection and operational events for faults, lifecycle changes, and
local data-protection work. Both surfaces use the same strict redaction
contract.

### Call records and route attempts

Store one call record per downstream client call. Its normal row shows:

- call time and published model;
- the upstream provider whose model route ultimately completed the call,
  displayed beneath the published model;
- total tokens with cached tokens shown as a subset;
- the estimated charge from that successful model route; and
- response-completion latency, plus time to first token only for streaming
  calls.

When Fallback occurs, keep one call record and expose an expandable model-route
attempt chain. Each attempt contains only its order, model-route and upstream
provider identifiers, start time and duration, HTTP status when available,
normalized failure category, downstream-commit phase, and whether it caused
Fallback or stream termination. Do not turn attempts into independent call
records.

Token and charge accounting uses only the reliable usage reported by the model
route that successfully completed the call. Earlier failed attempts are not
included; the relay neither assumes that they were free nor estimates missing
usage. If every route fails, retain the call record with a failed outcome and
no successful upstream provider. Show unknown token, charge, first-token, and
completion values as `-`, not zero, and exclude the failed call from token and
charge aggregates.

Retain call records and attempt details for 14 days. Preserve all-time usage
through permanent daily aggregates by published model and upstream provider;
these contain token and calculated-charge totals but no per-call identifiers or
attempt details.

### Operations console signals

Keep a persistent system-status strip on the Operations console. It summarizes
five independently actionable areas and links each abnormal state to its
14-day operational-event history:

- **Storage**: `Healthy`, `Degraded`, or `Not ready`; state start time; affected
  record class; last normalized persistence error; known dropped-record count
  or `unknown`; and the start and end of any usage-accounting gap.
- **Model routes**: counts in `Available`, `Checking`, and `Temporarily
  unavailable`, with each route row showing state age, last check or attributable
  failure time and category, last HTTP status when safe, next recovery-probe
  time, and the current capped-doubling interval.
- **Backups**: last verified backup time, trigger, schema version and size; next
  automatic backup time; retained snapshot count; and the last failed creation,
  verification, or rotation stage with a normalized reason.
- **Migration and restore**: running and supported schema versions, prerequisite
  backup result, current or last stage, validation result, completion time, and
  the actionable reason for a not-ready startup. A restored route-health record
  is never presented as current health; fresh startup checks remain visible.
- **Usage completeness**: whether accounting is complete for the selected
  interval and every known gap caused by missing upstream usage or failed local
  persistence. Gaps remain explicit and are never estimated or backfilled.

An operational-record write failure does not invalidate a successful relay
response. It moves Storage to `Degraded` immediately and opens an accounting
gap where applicable. Clear the degraded state automatically only after the
same record class persists successfully again and SQLite passes a lightweight
integrity check. Closing the current degraded state does not erase its event or
claim that the historical gap is complete.

### Local structured logs

Write structured diagnostics to standard error for the installed launcher to
capture. Log process lifecycle and readiness, model-route transitions and
probes, Fallback and abnormal calls, storage degradation and recovery, backup,
migration, restore, and log-rotation failures. Do not emit one log event for an
ordinary successful call because its metadata already belongs to the call
record and usage views.

Every event includes a timestamp, severity, stable event code, process version,
and locally generated correlation identifiers relevant to the event. Include
safe model-route, published-model, and upstream-provider identifiers plus
normalized status, stage, duration, or error category as applicable.

Rotate captured log files at the earlier of the local calendar-day boundary or
20 MiB. Retain no file older than 14 days and cap the managed log set at 200 MiB,
deleting oldest files first when either limit is exceeded. Operational events in
SQLite use the same 14-day retention. Current status, managed-backup metadata,
and permanent daily usage aggregates are not deleted with diagnostic history.

### Redaction boundary

Use an allowlist of metadata fields rather than attempting to scrub arbitrary
payloads after capture. The console, call records, operational events, and local
logs must never store or render request or response bodies, prompts, tool
arguments, raw upstream error bodies, raw headers, query strings, upstream API
keys, relay access keys, administrator credentials, or backup contents. Do not
log complete Base URLs; identify upstreams by their non-secret local identifier
and display name. Upstream failures become stable normalized categories with a
safe locally generated description. Secret-bearing backups remain visible only
through safe metadata and their protected local identity, never through their
contents.
