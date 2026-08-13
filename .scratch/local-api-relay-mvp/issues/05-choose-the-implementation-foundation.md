# Choose the Implementation Foundation

Type: grilling
Status: resolved
Blocked by: 01, 02

## Question

Should the implementation extract a narrow CC Switch kernel, adapt selected algorithms behind new boundaries, or start independently with CC Switch used only as behavioral evidence, and which runtime best supports the chosen path?

## Answer

Start the relay as an independent Rust implementation. CC Switch remains
behavioral and test evidence only: do not fork it, depend on its crate, or
extract its proxy modules into the MVP. Reproduce the relevant conformance
cases for routing, first-event streaming priming, cancellation, failure
classification, and recovery against this project's published-model and model-
route semantics. Any later source-level reuse would require a separate scoped
decision, dependency review, provenance note, and retention of the applicable
MIT notice.

Use one standalone Rust process with this foundation:

- **Axum** provides the loopback OpenAI-compatible API, management endpoints,
  and delivery of the local Web management assets.
- **Tokio** owns asynchronous request handling, downstream cancellation,
  timeouts, concurrent startup checks, and scheduled recovery probes.
- **Reqwest** is the upstream HTTP client for native Chat Completions and
  Responses requests, including streaming bodies. Add lower-level Hyper code
  only if a verified compatibility case cannot be met through Reqwest; CC
  Switch's header-case machinery is not part of the starting foundation.
- **Rusqlite with bundled SQLite** is the single local persistent store. The
  exact transaction, migration, backup, export, and secret-handling contract
  remains owned by [Define the Persistence, Backup, and Migration
  Contract](10-define-persistence-backup-and-migration-contract.md).

The production runtime has no Node.js, Tauri, desktop shell, tray, tool-
configuration integration, or separate database service. A frontend build
tool may produce static management assets, but the Rust process is the only
runtime service installed or started by the user. Packaging, startup, data-
directory, and port-conflict behavior are deferred to [Define Packaging and
Startup Experience](11-define-packaging-and-startup-experience.md).
