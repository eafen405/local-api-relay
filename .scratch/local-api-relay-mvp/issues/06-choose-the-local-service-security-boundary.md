# Choose the Local Service Security Boundary

Type: grilling
Status: resolved
Blocked by:

## Question

What are the MVP network exposure and authentication rules for the relay API, Web management page, stored upstream API keys, and optional LAN access?

## Answer

The MVP is a single-user, local-only service. Both the OpenAI-compatible API and
the Web management page bind to `127.0.0.1`; wildcard, LAN, and remote exposure
are not supported. Remote access can be designed as a separate later effort and
does not shape the MVP protocol.

All `/v1/*` requests authenticate with a relay access key supplied as a Bearer
token. Relay access keys can be created and revoked. For each published model,
each key defines an eligible model-route set. Cost ordering, health filtering,
and fallback operate only within that set; a key with no eligible route for a
published model cannot call it.

The management page uses a separate single-administrator credential and a
simple browser session. Relay access keys grant no management capability. The
MVP has no users, roles, organizations, or remote-login model.

Upstream API keys are stored unencrypted in the local data file, whose
permissions restrict access to the current operating-system user. Secret values
are masked in the management page and omitted from logs, ordinary API
responses, and default exports. The MVP does not add an operating-system
keychain dependency, an encrypted vault, TLS termination, or LAN authentication.
