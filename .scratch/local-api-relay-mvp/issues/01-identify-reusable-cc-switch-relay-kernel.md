# Identify the Reusable CC Switch Relay Kernel

Type: research
Status: resolved
Blocked by:

## Question

Which parts of `tmp/cc-switch` provide reusable evidence or code for provider routing, OpenAI protocol adaptation, streaming, failure classification, circuit breaking, and recovery, and which dependencies or license constraints make direct extraction a poor choice for this lean relay?

## Answer

Use CC Switch as a behavioral reference, not as the relay kernel. Its MIT license permits selective reuse with notice retention, but its routing and health state are keyed by application and provider, its recovery is request-triggered rather than periodic, and its proxy path is tightly coupled to Tauri, database, OAuth, tool-specific transforms, and UI switching. Reimplement model-route identity, multiplier ordering, model-level recovery, and late-stream handling; borrow the circuit-breaker, first-byte priming, and error-policy test scenarios. See [CC Switch Relay Kernel Audit](../research/cc-switch-relay-kernel.md).
