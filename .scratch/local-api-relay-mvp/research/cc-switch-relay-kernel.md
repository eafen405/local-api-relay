# CC Switch Relay Kernel Audit

## Scope

This audit uses the local CC Switch source at commit `413c09e0790c304506888ae24b9be72820aca126` (2026-08-06). The question is whether its relay code should be extracted for the lean local API relay, whose routing unit is one published model route and whose ordering is driven by a user-supplied cost multiplier.

## Recommendation

Use CC Switch as a behavioral reference, not as the MVP's proxy kernel. Selectively port small, pure ideas and their test cases: circuit-breaker transitions, first-byte stream priming, bounded response reads, and the retryability matrix. Rewrite the route model, candidate ordering, health state, recovery scheduler, and late-stream failure handling around this project's simpler contract.

The reason is structural rather than licensing: CC Switch routes by application and provider, integrates deeply with its database, provider configuration, OAuth managers, Tauri state, tray/UI switching, protocol rectifiers, and usage accounting. Its core abstractions do not represent a published model with several independently healthy upstream model routes. [Source: `provider_router.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/provider_router.rs), [source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L5), [source: `failover_switch.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/failover_switch.rs#L8)

## Findings

### Routing and health granularity must be rewritten

CC Switch reads an explicitly ordered failover queue and tries providers in that order. It does not rank candidates by cost multiplier. Its in-memory breaker key is `app_type:provider_id`, and its persisted health record is likewise keyed by provider and application. [Source: `provider_router.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/provider_router.rs#L15), [source: `failover.rs`](../../../tmp/cc-switch/src-tauri/src/database/dao/failover.rs#L21), [source: `schema.rs`](../../../tmp/cc-switch/src-tauri/src/database/schema.rs#L185)

That is incompatible with the required isolation boundary: one supplier's one upstream model route. The new relay should give every route an identity equivalent to `(published_model, provider, upstream_model)` and sort eligible routes by their configured multiplier, with a deterministic tie-breaker. CC Switch's existing multiplier is evidence for cost accounting, not routing: the routing code only consumes queue order. [Source: `provider_router.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/provider_router.rs#L51), [source: `schema.rs`](../../../tmp/cc-switch/src-tauri/src/database/schema.rs#L194)

### The circuit-breaker state machine is useful as a reference

`CircuitBreaker` implements `Closed`, `Open`, and `HalfOpen`, with consecutive-failure and error-rate thresholds, a recovery timeout, a half-open success threshold, and a single concurrent half-open probe. Its tests cover closed-to-open, half-open-to-closed, reset, and preservation of an in-flight probe permit. [Source: `circuit_breaker.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/circuit_breaker.rs#L13), [source: `circuit_breaker.rs` tests](../../../tmp/cc-switch/src-tauri/src/proxy/circuit_breaker.rs#L401)

The recovery mechanism is not the required periodic model check. An open breaker becomes half-open only when `is_available` or `allow_request` is called after the timeout. CC Switch's separate reachability check probes only a base URL and deliberately does not mutate breaker state; any HTTP response, including `401` or `403`, counts as reachable. The MVP therefore needs its own scheduler and a model-level probe that exercises the configured API/model before returning a route to service. [Source: `circuit_breaker.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/circuit_breaker.rs#L125), [source: `stream_check.rs`](../../../tmp/cc-switch/src-tauri/src/services/stream_check.rs#L1)

### Error classification is close enough to seed tests, not code reuse

CC Switch retries timeouts, forwarding failures, stream idle timeouts, provider-unhealthy failures, most upstream `4xx`, and all `5xx`. It treats `400`, `405`, `406`, `413`, `414`, `415`, `422`, and `501` as non-retryable. Only retryable errors update breaker/database health; non-retryable and client-abort paths release a half-open permit without degrading provider health. [Source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L1003), [source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L2655)

The policy also contains account- and product-specific exceptions, such as different authentication behavior for official Codex and xAI OAuth providers. The lean relay should encode a small explicit policy table for generic OpenAI-compatible upstreams and use CC Switch's cases as test inputs, rather than inherit these exceptions. [Source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L2655), [source: error-policy tests](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L4396)

### Streaming fallback has a hard commit boundary

For non-streaming requests, CC Switch reads the full body before recording success, allowing body timeouts or read failures to return to the retry loop. For streaming requests, it waits for and replays the first chunk; the Responses path may wait for a productive or valid terminal event. This is useful evidence for a pre-commit failover boundary. [Source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L2282), [source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L2341), [source: priming tests](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L3914)

After priming, however, the forwarder records success and returns the stream. Later idle timeouts and I/O failures are emitted to the client and end the stream; they do not return to provider selection or update route health. Consequently, CC Switch does not implement fallback after partial output, nor does it mark that late-interrupted route unhealthy. [Source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L493), [source: `response_processor.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/response_processor.rs#L683)

The MVP must state this boundary precisely. Seamless retry after bytes have reached the client cannot be implemented by simply selecting another provider: it requires buffering the whole response, or a protocol for discarding/reconciling duplicated partial generation. CC Switch provides no reusable mechanism for that behavior.

### Protocol code should remain optional

The small `ProviderAdapter` interface is a useful shape for URL construction, authentication headers, and optional transforms, but it still depends on CC Switch's `Provider` and `ProxyError`. [Source: `adapter.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/providers/adapter.rs#L10)

CC Switch also contains substantial Responses-to-Chat request and streaming conversion logic, selected using provider-specific configuration and URL inference. That code handles tools, reasoning, media, cache routing, and Codex-specific compatibility cases. If the MVP initially forwards Chat Completions to Chat-compatible routes and Responses to Responses-compatible routes, transparent forwarding is the smaller and safer design. Revisit extraction only if the product explicitly decides that a Chat-only upstream must satisfy a Responses request. [Source: `codex.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/providers/codex.rs#L25), [source: `transform_codex_chat.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/providers/transform_codex_chat.rs), [source: `streaming_codex_chat.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/providers/streaming_codex_chat.rs)

CC Switch's model mapper is also not a suitable generic alias layer: it is built around Claude-family defaults and can pass an unmapped name through. This project requires explicit mappings and an immediate error when a requested published model has no route. [Source: `model_mapper.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/model_mapper.rs#L9)

## Dependency and License Assessment

CC Switch is MIT licensed. Copying or modifying its code is permitted, but copies or substantial portions must retain its copyright and permission notice. There is no project-level license barrier to selective reuse. [Source: `LICENSE`](../../../tmp/cc-switch/LICENSE#L1), [source: `Cargo.toml`](../../../tmp/cc-switch/src-tauri/Cargo.toml#L1)

Direct extraction is still a poor engineering tradeoff. The Rust package includes Tauri and desktop plugins alongside Axum, Tokio, Reqwest, Hyper, Rusqlite, protocol transforms, OAuth managers, compression, scripting, and platform-specific dependencies. The proxy forwarder directly imports database-backed routing, application types, Tauri state, provider-specific OAuth, rectifiers, usage state, and UI switch behavior. Extracting it would mean either carrying those dependencies or performing a broad disentangling refactor before any MVP behavior exists. [Source: `Cargo.toml`](../../../tmp/cc-switch/src-tauri/Cargo.toml), [source: `forwarder.rs`](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L5)

Any code copied later should receive a provenance note and the MIT notice; third-party crates retained by an implementation must also be checked under their own licenses as part of dependency selection.

## Reuse Decision

| Area | Decision |
| --- | --- |
| Candidate routing | Rewrite for model-route identity and multiplier ordering |
| Circuit breaker | Reimplement the small state machine; reuse scenarios/tests as reference |
| Recovery | Rewrite as periodic model-level probes |
| Error policy | Reimplement as an explicit generic policy table |
| Streaming | Reuse first-byte priming idea; separately decide the post-commit interruption contract |
| OpenAI forwarding | Implement transparent Chat/Responses forwarding first |
| Responses-to-Chat conversion | Defer; evaluate selective MIT-compliant extraction only when required |
| Tauri, tool takeover, tray/UI switching, OAuth managers | Exclude from the relay kernel |

## Implications for the Remaining Map

1. The route data-model ticket must own model-route identity, explicit mappings, multiplier ordering, and health fields.
2. The failure/recovery ticket must distinguish pre-commit failures from post-commit stream interruption and define a periodic probe contract.
3. The protocol-contract ticket should not promise Responses-to-Chat translation unless it is made an explicit MVP requirement.
4. The implementation-foundation ticket should select a small HTTP/runtime/database stack independently of CC Switch; CC Switch does not need to be forked.
