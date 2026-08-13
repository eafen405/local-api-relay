# Define Packaging and Startup Experience

Type: grilling
Status: resolved
Blocked by: 05, 06

## Question

For the standalone Rust loopback service, how should the MVP be distributed,
installed, started, stopped, and upgraded on its supported operating systems;
where should its data and logs live; how should it handle administrator
bootstrap, port conflicts, readiness, and startup failures; and how should it
serve the local Web management assets without adding a desktop shell?

## Answer

Support one primary MVP topology: a Linux x86_64 relay process runs inside
WSL2 on a Windows host, while Windows-native agents, WSL2 agents, and the
Windows default browser all reach that same loopback service. Do not ship a
native Windows relay binary or widen the listener for LAN access. Acceptance
must exercise the relay from both Windows and WSL2 and open the management
console from Windows, so WSL localhost forwarding is a tested contract rather
than an assumption.

### Distribution and local layout

Publish a self-contained, versioned Linux x86_64 archive containing the Rust
binary and idempotent installation and lifecycle scripts. Do not require a
package repository, root-owned system directories, a container runtime, Node.js,
or a desktop shell in production.

Install versioned program files side by side and expose the selected version
through a stable user-level executable path. Follow the XDG user layout:

- keep the SQLite database and protected backups under
  `~/.local/share/local-api-relay/`;
- keep process-level configuration under `~/.config/local-api-relay/`; and
- keep runtime state and local log files under
  `~/.local/state/local-api-relay/`.

All directories and secret-bearing files are owner-only. The service writes
structured diagnostics to standard error, and the installed launcher captures
them into the state directory; exact rotation, retention, and redaction belong
to [Define Operational Diagnostics and
Retention](12-define-operational-diagnostics-and-retention.md).

Build the management frontend ahead of release and embed its static assets in
the Rust binary. The installed service has no separate frontend directory or
frontend runtime, so the console and management API cannot drift across
versions.

### Startup and daily use

The installer creates a per-user Windows scheduled task that starts at user
login. The task directly owns a long-running `wsl.exe` invocation of
`local-api-relay serve`, keeps WSL2 alive, and applies a bounded restart policy
when the process exits unexpectedly. It does not depend on WSL systemd and does
not run before Windows login.

Fixed lifecycle commands provide `start`, `stop`, `restart`, and `status`.
Process lifecycle is not managed through the browser. A separate desktop
console launcher checks the dedicated local readiness endpoint, opens the
management console in the Windows default browser when ready, and otherwise
shows service status and actionable diagnostic commands. In normal use the
login task has already started the always-on service before the console is
opened. There are no browser-configurable startup commands or arbitrary shell
hooks.

On first installation, an explicit CLI initialization command creates and
prints a one-time administrator bootstrap credential. The first browser login
must replace it. Do not place that credential in the scheduled task, process
environment, ordinary logs, or desktop launcher.

### Binding, readiness, and failure

Use a stable default listener at `127.0.0.1:8787`. A deliberate configuration
change may choose another stable port, but the process never scans for or
silently switches to a free port. A collision is an actionable startup
failure, because silently changing the address would invalidate agent and
console configuration.

The service becomes ready after its persistent store and configuration have
opened, migrated, and validated successfully and the loopback listener has
bound. It does not wait for upstream startup checks: the console and relay API
remain reachable while routes are `Checking`, and calls without an available
eligible route fail explicitly under the route-state contract.

Database corruption, an unsupported newer schema, a failed backup-gated
migration, invalid process configuration, or a port collision prevents ready
state and exits nonzero. The scheduled task retries only within a bounded
policy and then leaves the task failed instead of looping forever. Never create
an empty replacement database, automatically restore a backup, bind a random
port, or widen the network interface to recover from startup failure.

### Stop and upgrade

On stop or restart, stop accepting new relay requests and allow in-flight
requests up to 30 seconds to finish. After the deadline, cancel the remaining
requests, close persistent resources, and exit.

Upgrade by unpacking a new version beside the current one, validating it,
honoring the mandatory pre-migration backup contract, atomically switching the
stable executable path, and restarting the scheduled task. Retain the previous
program version for rollback. If no schema migration committed, a failed
upgrade can switch directly back; after a forward-only schema migration,
rollback requires the previous binary together with an explicit restore of the
pre-migration backup rather than attempting to downgrade the live database.
