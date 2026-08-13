# OpenAI-Compatible MVP Contract

## Evidence boundary

The current environment could not retrieve OpenAI's developer or legacy API-reference pages: both official hosts returned HTTP 403, and the OpenAI Docs MCP server is unavailable. The official pages below therefore identify the intended first-party contract, but their current text was not independently captured in this session:

- [Models: list](https://platform.openai.com/docs/api-reference/models/list)
- [Chat Completions: create](https://platform.openai.com/docs/api-reference/chat/create)
- [Responses: create](https://platform.openai.com/docs/api-reference/responses/create)
- [Responses streaming events](https://platform.openai.com/docs/api-reference/responses-streaming)
- [API errors](https://platform.openai.com/docs/guides/error-codes/api-errors)

Accordingly, this note separates the stable public wire shape from behavior verified in the local CC Switch source. CC Switch is implementation evidence, not authority for OpenAI's API.

## Decision

The MVP should implement a **transparent compatibility subset**, not a reduced reimplementation of OpenAI schemas:

1. Accept only the three paths in scope: `GET /v1/models`, `POST /v1/chat/completions`, and `POST /v1/responses`.
2. Require a relay bearer key at the public boundary, replace it with the chosen upstream credential, and never expose upstream keys.
3. Parse only enough JSON to validate and map `model` and inspect `stream`; preserve all other request fields, including unknown future fields, when forwarding.
4. Preserve successful upstream status, body shape, SSE event order, and end-to-end fields. Rewrite only the model identifier where the public alias must remain stable.
5. Preserve upstream HTTP error status and OpenAI-style JSON error body whenever possible. Relay-generated failures also use an `{"error":{"message", "type", "param", "code"}}` envelope; `param` and `code` may be `null` when not applicable.

This policy gives new harness features a chance to work without a relay release and avoids silently dropping tool, reasoning, multimodal, metadata, or usage fields.

## Endpoint contract

### `GET /v1/models`

Return the relay's **published aliases**, not every raw upstream model:

```json
{
  "object": "list",
  "data": [
    {
      "id": "coding-main",
      "object": "model",
      "created": 0,
      "owned_by": "local-api-relay"
    }
  ]
}
```

The only client-routing field is `data[].id`; `object`, `created`, and `owned_by` should still be emitted for ordinary OpenAI SDK compatibility. Ordering must be deterministic. Disabled aliases and aliases with no configured route are omitted; temporarily unhealthy routes do not remove an alias while another route remains available.

Local evidence: CC Switch's generic OpenAI-compatible model fetcher authenticates with `Authorization: Bearer`, consumes top-level `data`, and reads each entry's `id` and optional `owned_by` ([model_fetch.rs](../../../tmp/cc-switch/src-tauri/src/services/model_fetch.rs#L20), [request](../../../tmp/cc-switch/src-tauri/src/services/model_fetch.rs#L75), [fixture](../../../tmp/cc-switch/src-tauri/src/services/model_fetch.rs#L455)). Its separate proxy `handle_models` returns a Codex-private catalog with top-level `models`; that format is specifically described as a Codex startup catalog and must **not** be copied as the public OpenAI model-list contract ([handlers.rs](../../../tmp/cc-switch/src-tauri/src/proxy/handlers.rs#L73)).

### `POST /v1/chat/completions`

Minimum accepted request: a JSON object containing a non-empty string `model` and an array `messages`. `stream` defaults to `false`. The relay must pass through every other member rather than whitelist parameters.

For non-streaming success, preserve the upstream Chat Completion object, including at least `id`, `object: "chat.completion"`, `created`, public `model`, `choices`, and any `usage`. Do not collapse message content or tool calls into plain text.

For streaming success, return `Content-Type: text/event-stream`; preserve each SSE `data:` JSON chunk as a Chat Completion chunk and the terminal `data: [DONE]` marker. Preserve chunk boundaries/order where practical, but clients must not depend on transport packet boundaries. The relay may observe chunks for health and usage, but must not buffer the entire response.

Local evidence: CC Switch parses `stream` with a false default and otherwise forwards the JSON `Value` rather than deserializing into a narrow request struct ([handlers.rs](../../../tmp/cc-switch/src-tauri/src/proxy/handlers.rs#L702)). Its tests treat a first-chunk read failure as retryable and replay a successfully primed first chunk without loss ([forwarder.rs](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L3936)).

### `POST /v1/responses`

Minimum accepted request: a JSON object containing a non-empty string `model` and an `input` value accepted by the upstream. `stream` defaults to `false`. Preserve all other fields, especially `instructions`, tools and tool choice, reasoning controls, previous-response/conversation identifiers, metadata, and output controls.

For non-streaming success, preserve the complete Response object. At minimum, clients need its identity/lifecycle (`id`, `object: "response"`, `created_at`, `status`, public `model`), `output`, and any `error`, `incomplete_details`, and `usage` fields. A 2xx body whose Response status is `failed` or `cancelled`, or whose `error` is non-null, is a semantic failure rather than a healthy route result.

For streaming success, return `Content-Type: text/event-stream` and preserve named SSE events in order. The minimum lifecycle is `response.created`, `response.in_progress`, zero or more typed output delta/item events, and exactly one terminal `response.completed`, `response.failed`, or `response.incomplete` event. Do not translate the stream to Chat Completion chunks at the public boundary. Do not require `[DONE]` as the Responses success signal; the typed terminal event is authoritative.

Local evidence: CC Switch forwards arbitrary JSON to native Responses upstreams ([handlers.rs](../../../tmp/cc-switch/src-tauri/src/proxy/handlers.rs#L790)). Its shared Responses SSE builder uses named `event:` plus JSON `data:` framing and emits typed lifecycle and output events ([codex_responses_sse.rs](../../../tmp/cc-switch/src-tauri/src/proxy/providers/codex_responses_sse.rs#L19)). It explicitly detects `failed`/`cancelled` Response bodies and early SSE failure envelopes even when HTTP status is 2xx ([forwarder.rs](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L2966)).

## Errors

- **Client/request errors:** invalid JSON, missing or unknown model mapping, invalid request fields, payload too large, and relay authentication failures return immediately and never affect route health. Do not try another upstream for a request that is known to be invalid independent of provider.
- **Upstream HTTP errors:** before any downstream response is committed, preserve the final upstream status and OpenAI-style error envelope after routing policy has exhausted eligible routes. Avoid returning an upstream's HTML/plain-text error page; normalize it into the same JSON envelope and retain a bounded diagnostic message.
- **Relay transport errors:** use gateway semantics (`502` for malformed/broken upstream response, `503` when no route is available, `504` for upstream timeout) with the OpenAI-style envelope. These status choices are an MVP design rule, not a claim about OpenAI's own service.
- **No secrets:** error bodies and logs must not include upstream API keys, full authorization headers, or unbounded upstream bodies.

Local evidence: CC Switch preserves upstream status and JSON error bodies, normalizes non-JSON upstream errors, and wraps internal proxy errors under `error` ([error.rs](../../../tmp/cc-switch/src-tauri/src/proxy/error.rs#L82)).

## Cancellation and failover commit point

Cancellation is transport behavior for these synchronous endpoints: when the downstream client disconnects or aborts, cancel/drop the active upstream request promptly, do not start fallback, and do not mark the route unhealthy. A separate API-level cancellation endpoint for background Responses is outside this three-endpoint MVP.

Failover has a strict commit point:

- Before response headers/body are committed downstream, a connection failure, timeout, invalid upstream response, or semantic Responses failure may select another route.
- After any response bytes have been delivered downstream, the relay cannot splice a new provider's generation into the same Chat/Responses stream without duplicating or contradicting tokens, tool-call IDs, response IDs, ordering, and usage. It must terminate the stream, mark that provider-model route temporarily unavailable, and let the harness retry as a new request if it supports retry.

CC Switch's first-chunk priming tests are useful implementation evidence for this boundary: failure before the first chunk is retryable, while a successful first chunk is replayed and commits the selected response ([forwarder.rs](../../../tmp/cc-switch/src-tauri/src/proxy/forwarder.rs#L3936)).

## MVP conformance tests

1. Model listing emits only published aliases in `object=list` / `data[]` form and is deterministic.
2. Both POST endpoints reject missing/unknown `model` before contacting an upstream.
3. Unknown request fields survive forwarding; unknown response fields survive return.
4. Non-streaming JSON is returned as JSON with upstream status and public model alias.
5. Chat streaming preserves chunk order and `[DONE]`; Responses streaming preserves named events and a typed terminal event.
6. Upstream failure before commit retries the next eligible route without leaking bytes from the failed attempt.
7. Upstream failure after commit closes the stream and never appends output from another provider.
8. Downstream cancellation aborts upstream work, starts no fallback, and leaves health neutral.
9. HTTP and semantic Responses errors are not counted as successful health checks.
10. No response, error, or log exposes an upstream credential.

