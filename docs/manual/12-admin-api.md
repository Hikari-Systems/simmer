# The admin API

The admin API is an HTTP API on its own port. It answers health checks, shows
each route's live state, lets you pause routes and adjust allowances without a
restart, and runs the dry run.

```yaml
admin:
  listen: "0.0.0.0:8080"
  auth_token: "${SIMMER_ADMIN_TOKEN}"
  tokens:                                    # optional, named tokens
    - { name: oncall,    token: "${SIMMER_TOKEN_ONCALL}" }
    - { name: deploybot, token: "${SIMMER_TOKEN_DEPLOY}" }
  metrics: true                              # serve /metrics; default off
```

| Key | Default | Meaning |
|---|---|---|
| `listen` | required | Where the API listens |
| `auth_token` | none | A bearer token, recorded as `default` in the audit log |
| `tokens` | none | Named tokens, recorded under their names |
| `metrics` | off | Serve `/metrics`; see [Logs, metrics and OpenTelemetry](11-observability.md#prometheus-metrics) |

At least one token is required, from `auth_token` or `tokens`. No two may share a
name or a secret. Use at least 16 random characters; Simmer warns about shorter
ones. Every token can do everything: there are no read-only tokens.

**Keep the port private.** The API can pause routes, and its read endpoints show
every provider's hostname and the whole routing setup. Publish it on a host only
on loopback (`-p 127.0.0.1:8080:8080`), or keep it on an internal network.

## Endpoints

| Request | Token | Answers |
|---|---|---|
| `GET /health` | no | `200` when Simmer is up and can reach its database, `503` when it cannot |
| `GET /healthcheck` | no | `200` when Simmer is up, without checking the database (`?deps=true` checks it). The container's `HEALTHCHECK` uses this |
| `GET /metrics` | no | Prometheus metrics, when `admin.metrics` is on; `404` otherwise |
| `GET /ramps`, `GET /ramps/{ramp}` | yes | Each ramp: its name, whether it is the default, the listeners routing to it, its routes |
| `GET /ramps/{ramp}/routes`, `GET /ramps/{ramp}/routes/{name}` | yes | Each route's configuration and live state |
| `GET /quota?ramp=&route=&group=` | yes | Today's counters, filtered. `ramp` is required when there is more than one ramp |
| `POST /ramps/{ramp}/routes/{name}/pause`, `…/resume` | yes | Take a route out of use, or put it back |
| `POST /ramps/{ramp}/routes/{name}/graduate` | yes | Give the route the last value of its schedule from now on |
| `POST /ramps/{ramp}/routes/{name}/allowance` | yes | Override one domain group's cap for the rest of the route's day |
| `POST /quota/reset` | yes | Clear one route and group's counters for today |
| `POST /dryrun` | yes | What would happen to a message, without sending it |

Send the token as `Authorization: Bearer <token>`. The old paths from before named
ramps (`/routes` and everything under it) answer `410 Gone` with the path that
replaced them, rather than guessing which ramp was meant.

## Reading a route's state

```sh
TOKEN=...
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/ramps/main/routes/warming | jq
```

The main fields:

| Field | Meaning |
|---|---|
| `status` | `active`, `paused` (by an operator), or `not_started` (`warmup.started` is in the future) |
| `day_index`, `day_started_at`, `day_ends_at` | Which day of the schedule the route is on, and when it started and ends |
| `graduated` | Whether the route has been graduated |
| `groups[]` | One entry per domain group, below |
| `preflight` | The last DNS preflight result, check by check. `null` means nothing has been checked, which is not the same as passing |
| `pool` | Connections: `max_connections`, the current `idle` and `active`, and lifetime `opened`, `reused`, `retired` and `discarded` |
| `downstream_overrides[]` | Each per-group provider, with its own pool |

For each domain group:

| Field | Meaning |
|---|---|
| `scheduled` | What the configuration's schedule says for today |
| `allowance` | The cap actually being enforced today |
| `override` | An allowance override, if one is set |
| `committed` | Messages delivered today |
| `reserved` | Messages being sent right now |
| `headroom` | What is left: `allowance` minus `committed` and `reserved`. `null` means unlimited |
| `drift` | `true` when `allowance` and `scheduled` differ. Normal for the rest of the day after a schedule change, and the first thing to check when the route is not doing what the configuration says |
| `partial_ramp_share` | The share currently offered, or `null` for all |
| `row_exists` | `false` means nothing has been sent on this route and group today, so the numbers are what the first message will start from |

Reading the pool: `reused` far below `opened` means connections are not lasting
between messages, and a growing `discarded` means the provider is closing them
from its side; lower `idle_ttl`.

## Changing things without a restart

Every change is persisted in the database, so it applies to every instance and
survives a restart. Each is logged at `INFO` with the acting token's name, what
it changed, and the value it replaced. Each response also lists any chains the
change has just left with no eligible route, because a pause can mean every
message on a chain gets `451`.

```sh
H=(-H "Authorization: Bearer $TOKEN" -H 'content-type: application/json')

# Take a route out of use. Its mail goes to the next route in each chain.
curl -s -X POST "${H[@]}" localhost:8080/ramps/main/routes/warming/pause
curl -s -X POST "${H[@]}" localhost:8080/ramps/main/routes/warming/resume

# Let it send 500 to Google today. Lasts until the route's next day begins.
curl -s -X POST "${H[@]}" -d '{"domain_group":"google","allowance":500}' \
  localhost:8080/ramps/main/routes/warming/allowance
# Remove the override:
curl -s -X POST "${H[@]}" -d '{"domain_group":"google","allowance":null}' \
  localhost:8080/ramps/main/routes/warming/allowance

# Jump to the end of the schedule from now on; and back again.
curl -s -X POST "${H[@]}" localhost:8080/ramps/main/routes/warming/graduate
curl -s -X POST "${H[@]}" -d '{"graduated":false}' localhost:8080/ramps/main/routes/warming/graduate

# Clear today's counters for one route and group. Destructive.
curl -s -X POST "${H[@]}" \
  -d '{"ramp":"main","route":"warming","domain_group":"google","confirm":"reset"}' \
  localhost:8080/quota/reset
```

- **Pause** is the tool for "reputation is dropping, stop now". A paused route is
  skipped until resumed.
- **Allowance** sets a cap for today only. An allowance of 0 stops one group
  for the day. It is a temporary adjustment; change the schedule for anything
  lasting.
- **Graduate** pins a warming route to the last value of its schedule, whatever
  day it is on. It does not make it uncapped.
- **Reset** clears today's counters, letting the route send its whole cap again
  today. It needs `"confirm": "reset"` exactly.

## The dry run

The dry run is how to check a configuration before it carries mail. It runs the
real sender matching, chain walk and rewrite on a message you describe, and sends
nothing, reserves nothing and records nothing.

```sh
curl -s -X POST "${H[@]}" localhost:8080/dryrun -d '{
  "envelope_from": "jane@oldbrand.com",
  "from_header": "Jane <jane@oldbrand.com>",
  "recipients": ["bob@gmail.com"],
  "subject": "Your order",
  "body": "Track it at https://oldbrand.com/orders/123"
}' | jq
```

Request fields, all optional except `recipients`:

| Field | Meaning |
|---|---|
| `envelope_from` | The envelope sender. Absent or `null` is the empty sender, `<>` |
| `from_header` | The `From:` value: an address, or `Display Name <address>` |
| `recipients` | The recipient, in a list |
| `subject`, `body` | A subject and plain-text body for the sample message |
| `message` | A complete message, headers and all, instead of the fields above. The only way to test a MIME structure |
| `in_reply_to`, `references` | Header values, to see where a reply would go with [thread affinity](09-thread-affinity.md) |
| `include_message` | `true` to return the whole rewritten message |
| `ramp` | The ramp to use. Or give `listener` (an address from `server.listeners`), `auth_user` and `ramp_header`, and the dry run chooses the ramp the way a real session would |

The answer gives the ramp and how it was chosen, the sender rule matched, the
chain, and for the recipient:

- `domain_group`, and `domain_group_basis`: `literal`, `mx:<host>`, `fallback` or
  `fallback:mx-unavailable`;
- `evaluation`: every route in the chain, and why it was or was not chosen;
- `selected`, and which provider (`downstream`) it would go to;
- `would_reply`: the reply the application would get;
- `outbound`: the rewritten envelope sender and headers, and each
  `body_rewrites` and `header_rewrites` pattern with how many times it matched,
  **including the ones that matched nothing**. That is usually the answer to
  "why is my rewrite not working".

The walk it shows is the same code the real relay runs, checked by the test
suite step for step.

## Examples

```sh
# Why is mail to Google deferring?
curl -s -H "Authorization: Bearer $TOKEN" 'localhost:8080/quota?ramp=main&group=google' | jq

# Which day is each route on, and how much is left?
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/ramps/main/routes \
  | jq '.routes[] | {name, status, day_index, groups: [.groups[] | {domain_group, allowance, committed, headroom}]}'
```
