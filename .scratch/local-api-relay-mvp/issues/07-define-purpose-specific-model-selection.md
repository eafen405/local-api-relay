# Define Purpose-Specific Model Selection

Type: grilling
Status: resolved
Blocked by: 02, 03

## Question

How should harnesses select explicit model names for main tasks, subagents, and review so every name maps deterministically, missing mappings fail fast, and economical routes do not silently alter the requested model identity?

## Answer

Treat purpose as a caller concern, not a relay concept. The relay exposes a shelf of explicit published model names; the person or harness selects the model name for the main agent, and an agent selects the model name when dispatching a subagent. The relay never infers `main`, `subagent`, or `review` from prompts, request shape, or key labels, and it does not maintain purpose-to-model aliases.

The requested `model` value is authoritative and must resolve exactly to an enabled published model with an explicit protocol-specific model mapping and at least one eligible healthy model route for the relay access key. Unknown names, missing mappings, and an empty eligible set fail explicitly; the relay never substitutes another published model.

Routes are explicitly grouped under a published model by the administrator. A published model is one logical model identity, so cost ordering and Fallback remain within that group. For equivalent upstreams, the upstream model names are expected to be the same; the relay does not infer semantic equivalence between different names. A cheaper route therefore cannot silently change the requested model identity.
