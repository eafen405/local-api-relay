# Establish the OpenAI-Compatible MVP Contract

Type: research
Status: resolved
Blocked by:

## Question

What request, response, streaming, error, model-listing, and cancellation behavior must the MVP preserve for `/v1/models`, `/v1/chat/completions`, and `/v1/responses`, based on first-party specifications and locally available source evidence?

## Answer

Adopt a transparent compatibility subset: expose standard `data[]` model listings, parse only `model`/`stream`, preserve unknown request and response fields, pass through Chat and Responses wire formats without cross-conversion, normalize errors into an OpenAI-style envelope, and propagate downstream cancellation upstream. Failover is allowed only before response bytes are committed; a mid-stream upstream failure closes the stream and quarantines that provider-model route instead of splicing a second generation. Full evidence, contract details, and conformance cases: [OpenAI-Compatible MVP Contract](../research/openai-compatible-mvp-contract.md).
