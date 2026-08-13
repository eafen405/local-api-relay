# Define the Model Catalog and Cost Accounting Boundary

Type: grilling
Status: resolved
Blocked by: 03

## Question

How are preset model prices sourced and updated, how do provider multipliers affect displayed cost, and what usage accounting belongs in the MVP without turning routing into a billing system?

## Answer

Use a small, locally maintained catalog rather than an external price service.
The MVP catalog contains only these published models, with prices in RMB per
million tokens:

| Published model | Input | Output | Cached input |
| --- | ---: | ---: | ---: |
| `gpt-5.6-sol` | 5 | 30 | 0.5 |
| `gpt-5.6-terra` | 2 | 12 | 0.2 |
| `deepseek-v4-flash` | 1 | 2 | 0.02 |

The administrator enters and updates these prices locally. For a model route,
the displayed estimated charge is calculated from the upstream-reported token
usage and then multiplied by that route's configured cost multiplier:

`(uncached_input * input_price + cached_input * cached_price + output * output_price) / 1,000,000 * route_multiplier`

`input_total = uncached_input + cached_input`, and cache hit rate is
`cached_input / input_total`. If the upstream omits cached-input usage, treat
it as zero. The resulting charge is informational only and never changes route
ordering or fallback decisions.

Retain token consumption and calculated charge for six selectable windows:
`1h`, `5h`, `24h`, `7d`, `14d`, and all-time. The management console shows pie
charts for token share by published model and, within one published model, by
upstream provider. Billing amounts are displayed alongside these views but are
not routing inputs.
