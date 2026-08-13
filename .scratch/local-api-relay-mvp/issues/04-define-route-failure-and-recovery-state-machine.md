# Define the Route Failure and Recovery State Machine

Type: grilling
Status: resolved
Blocked by: 01, 02

## Question

Exactly when does a protocol-specific model route become temporarily unavailable, how does fallback behave for streaming and non-streaming requests, and what check returns the route to service? Define the startup check of every route, the retry interval for unavailable routes, and the user-selectable option for whether recovery sends a token-consuming minimal completion request; healthy routes must not be periodically probed.

## Answer

Use a three-state model per protocol-specific model route:

- **Checking**: the route has not passed its startup check and is excluded from
  candidate selection.
- **Available**: the route may be selected from an eligible route set in cost
  multiplier order.
- **Temporarily unavailable**: the route is excluded until a dedicated recovery
  probe from the current quarantine cycle succeeds.

The Web management surface and relay API become ready without waiting for
upstreams. On every process start, ignore persisted health for candidate
selection, put all routes into Checking, and probe them concurrently. A route
enters Available only after its probe succeeds; otherwise it enters Temporarily
unavailable and starts its recovery schedule.

### Failure classification

A single route-attributable failure moves an Available route immediately to
Temporarily unavailable. Do not use a consecutive-failure count, error-rate
window, or circuit-breaker threshold. Qualifying failures are:

- DNS, TLS, connection, forwarding, response-read, and upstream stream failures;
- connection, response, and stream-idle timeouts;
- upstream `401`, `403`, `404`, `429`, and `5xx` responses;
- malformed, truncated, or protocol-invalid upstream responses; and
- Responses failures whose error is clearly attributable to the upstream rather
  than to the client's request.

Invalid JSON, missing or unknown published models, invalid client fields, relay
authentication failures, payload limits, downstream cancellation, and upstream
`4xx` responses outside the allowlist are health-neutral. They neither trigger
Fallback nor change route health. Downstream cancellation aborts the active
upstream request and starts no new attempt.

Once a route has entered Temporarily unavailable, successes from requests that
were already in flight cannot restore it. They may be recorded for diagnostics,
but only the dedicated probe belonging to the current quarantine cycle may
change its state back to Available.

### Fallback and commitment

Before any downstream response is committed, each qualifying failure quarantines
the failed route and advances to the next Available route in the request's
eligible, multiplier-ordered candidate set.

For a non-streaming request, read and validate the complete upstream response
before committing it downstream. A qualifying status, body-read failure,
malformed body, or attributable Responses semantic failure can therefore use
the next route.

For a streaming request, prime and validate the first protocol event before
committing downstream headers or bytes. A qualifying failure during priming can
use the next route. After commitment, never splice another route's generation
into the stream: quarantine the failed route, terminate the downstream stream,
and leave any whole-request retry to the client. If all candidates fail, return
the final normalized error under the already established OpenAI-compatible
contract.

### Probe and recovery schedule

Startup and recovery use the route's native protocol and configured upstream
model. Send a non-streaming completion with the smallest valid input and the
smallest supported output allowance. Minimize token use rather than targeting a
budget; the request must remain below 100 tokens. Only a complete, protocol-valid
success passes. Any other result leaves the route excluded.

There is no user option for a metadata-only or non-token-consuming probe.
Healthy routes are never periodically probed.

Recovery scheduling has two global user settings:

- base interval `B`, defaulting to 30 seconds; and
- doubling limit `N`, defaulting to 5 and accepting zero or a positive integer.

After a runtime or startup failure, schedule the first recovery probe after
`B`. If a probe fails, increment its failed-probe index `k` and schedule the next
one after `B * 2^min(k, N)`. At the cap, continue probing at that maximum
interval. Thus the defaults yield 30 seconds, 1 minute, 2 minutes, 4 minutes, 8
minutes, and then 16 minutes repeatedly. With `N = 0`, every interval is `B`.
Allow at most one recovery probe in flight per route.

A successful recovery probe moves the route to Available, clears the failed
probe index, and returns it to normal multiplier ordering. A later qualifying
failure begins again at `B`.
