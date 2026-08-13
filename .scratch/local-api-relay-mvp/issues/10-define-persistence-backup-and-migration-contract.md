# Define the Persistence, Backup, and Migration Contract

Type: grilling
Status: resolved
Blocked by: 03, 05

## Question

Once the implementation foundation is chosen, what durability and atomicity guarantees must the single local data store provide, how are schema versions migrated, and what backup/export/import behavior preserves configuration while excluding or protecting secrets and rebuildable route-health state?

## Answer

Use bundled SQLite as the single persistent store, with two durability classes.

### Commit and failure contract

Configuration and authentication state is strongly committed. An operation that
changes upstream providers, published models and prices, model routes, relay
access keys and eligibility, administrator authentication, or recovery settings
must apply all related rows in one transaction. Enforce foreign keys and data
constraints in SQLite, publish the new runtime configuration only after commit,
and report success to the management client only after the commit succeeds. A
failed transaction leaves both the previous database state and active runtime
configuration unchanged.

Usage, estimated charges, and route-health history are operational records, not
an audit ledger. Persist them transactionally when reliable upstream facts are
available, but do not fail or retroactively invalidate an otherwise successful
relay response when those writes fail. Route-state transitions take effect in
memory immediately; a persistence failure must become an operator-visible
degraded-storage signal. Never invent missing usage for an interrupted stream.
On every process start, ignore stored route health for candidate selection and
put every configured route through its required startup check.

Configure SQLite for foreign-key enforcement, WAL journaling, and full durable
commit semantics. Serialize writes within the single process; readers may use a
consistent snapshot without observing partially applied configuration.

### Schema migrations

Record one integer schema version in the database. Ship ordered, forward-only
migrations with the binary; do not add downgrade migrations or a migration UI.
When an older schema is opened:

1. create and verify a consistent full backup;
2. run the entire required migration chain and schema-version update in one
   transaction; and
3. enter ready state only after the migrated schema passes validation.

If the prerequisite backup fails, do not begin migration. If migration or
validation fails, roll back the transaction, retain the old database, and keep
the service out of ready state with an actionable error. A binary that opens a
schema version newer than it supports must refuse to write it and must not try
to downgrade it.

### Full local backups

Create backups through SQLite's online backup/snapshot API rather than copying
the live database file, because a WAL-backed database may span multiple files.
A backup is a complete local disaster-recovery artifact: it includes
configuration, usage records, relay-key hashes, administrator state, upstream
API keys, and any stored route-health history. Store the backup directory with
owner-only access and each artifact with owner-read/write permissions. Treat
every backup as secret-bearing; never expose its contents in logs or ordinary
management responses.

If durable data has changed since the last scheduled snapshot, create at most
one automatic backup per 24-hour period. Also create a mandatory backup before
schema migration and before an explicit restore, and allow the administrator to
request one manually. After a new snapshot has been created and verified,
rotate the managed set to the 10 most recent backups.

### Explicit restore

Never restore automatically after corruption and never silently create an empty
database in its place. Preserve the failed database files as recovery evidence,
keep the relay out of ready state, and let the administrator explicitly choose
a backup.

Validate a selected backup's SQLite integrity, application identity, and schema
compatibility in an isolated candidate database. Reject a schema newer than the
running binary; migrate an older candidate under the migration contract above.
Before cutover, preserve the current database, then replace it only after the
candidate passes all checks. Any failure before cutover leaves the current
database selected.

A successful restore retains the backed-up configuration, secrets, relay-key
hashes, and usage history. Discard the restored route-health state for routing
purposes, place every route in Checking, and rebuild candidate pools from fresh
native-protocol startup checks.

The MVP has no portable configuration export/import or cross-machine migration
format. Those capabilities require a separate future decision; full local
backup and explicit restore are the only MVP data-transfer operations.
