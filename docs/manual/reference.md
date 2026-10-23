# Configuration reference

Every key Simmer accepts. Anything else is refused at startup. "Required" keys
must be present; the rest show their default. Durations are written `30s`, `5m`,
`24h` and so on ([durations](03-configuration.md#durations)).

## Top level

| Key | Type | Default | Chapter |
|---|---|---|---|
| `server` | map | required | [04](04-server.md) |
| `database` | map | required | [05](05-database.md) |
| `admin` | map | required | [12](12-admin-api.md) |
| `logging` | map | `{level: info, format: json}` | [11](11-observability.md#logs) |
| `telemetry` | map | off | [11](11-observability.md#opentelemetry) |
| `link_proxy` | map | off | [10](10-link-proxy.md) |
| `capture` | map | off | [13](13-capture.md) |
| `dot_insensitive_domains` | list of domains | `[gmail.com, googlemail.com]` | [07](07-routes.md#recipient-frequency) |
| `default_ramp` | string | required | [06](06-routing.md#ramps) |
| `ramps` | map of name to ramp | required, at least one | [06](06-routing.md#ramps) |

## `server`

| Key | Type | Default | Notes |
|---|---|---|---|
| `listeners` | list | required, not empty | |
| `listeners[].address` | `host:port` | required | Unique |
| `listeners[].tls` | `off` \| `starttls` \| `starttls_required` \| `implicit` | by port: 465 `implicit`, 587 `starttls_required`, else `off` | Anything but `off` needs `server.tls` |
| `listeners[].auth` | `disabled` \| `optional` \| `required` | by port: 465 and 587 `required`, else `optional` | |
| `listeners[].ramp` | ramp name | none | |
| `listeners[].header_overrides_affinity` | bool | `false` | Only with `ramp` |
| `hostname` | string | required | Not empty |
| `max_message_bytes` | integer | required | Above 0 |
| `max_recipients` | integer | required | At least 1; no effect above 1 |
| `max_concurrent_sessions` | integer | required | At least 1 |
| `allowed_cidrs` | list of CIDR blocks | required, not empty | |
| `timeouts.command` | duration | required | |
| `timeouts.data` | duration | required | |
| `timeouts.session` | duration | required | |
| `tls.certificate` | path | required with `tls` | PEM, server certificate first |
| `tls.private_key` | path | required with `tls` | PEM |
| `auth.allow_insecure_auth` | bool | `false` | |
| `auth.mechanisms` | list of `PLAIN`, `LOGIN` | `[PLAIN, LOGIN]` | |
| `auth.users` | list | none | |
| `auth.users[].username` | string | required | Unique |
| `auth.users[].password_hash` | argon2 string | required | From `server hash-password` |
| `auth.users[].grants.send_as` | list of patterns | required, not empty | Domain, `*.subdomain` or address |
| `auth.users[].grants.ramps` | list of ramp names | none | |

## `database`

| Key | Type | Default | Notes |
|---|---|---|---|
| `url` | string | required | PostgreSQL URL, or SQL Server connection string in the `-mssql` build |
| `max_connections` | integer | `10` | |
| `connect_timeout` | duration | required | |
| `fail_closed` | bool | `true` | |

## `admin`

| Key | Type | Default | Notes |
|---|---|---|---|
| `listen` | `host:port` | required | |
| `auth_token` | string | none | This or `tokens` is required |
| `tokens` | list | none | |
| `tokens[].name` | string | required | Unique |
| `tokens[].token` | string | required | Unique |
| `metrics` | bool, or `{enabled, idle_timeout}` | off | |
| `metrics.enabled` | bool | required in the map form | |
| `metrics.idle_timeout` | duration | `24h` | At least `1m` |

## `logging`

| Key | Type | Default |
|---|---|---|
| `level` | level or filter | `info` |
| `format` | `json` \| `text` | `json` |

## `telemetry`

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | |
| `endpoint` | URL | required | Empty means off |
| `headers` | map | none | |
| `service_name` | string | `simmer` | |
| `resource` | map | none | |
| `level` | filter | `info,sqlx=warn,tiberius=warn` | |
| `traces` | bool | `true` | |
| `sample_ratio` | number | `1.0` | 0 to 1 |
| `metrics` | bool | `true` | |
| `metrics_interval` | duration | `60s` | At least `1s` |
| `metrics_temporality` | `delta` \| `cumulative` | `delta` | |
| `logs` | bool | `true` | |
| `timeout` | duration | `10s` | |
| `recipients` | `none` \| `domain` \| `hashed` \| `address` | `none` | Needs `traces: true` |
| `recipient_hash_key` | string | none | Required with, and only with, `hashed`; at least 16 bytes |

## `link_proxy`

| Key | Type | Default | Notes |
|---|---|---|---|
| `listen` | `host:port` | required | |
| `upstream` | URL | required | Optional path prefix, no query |
| `allowed_cidrs` | list of CIDR blocks | required, not empty | |
| `public_scheme` | `https` \| `http` | `https` | |
| `max_request_bytes` | integer | `1048576` | |
| `max_connections` | integer | `512` | |
| `timeouts.header_read` | duration | `10s` | |
| `timeouts.upstream_connect` | duration | `5s` | |
| `timeouts.upstream_response` | duration | `30s` | |
| `timeouts.idle` | duration | `60s` | |

## `capture`

| Key | Type | Default | Notes |
|---|---|---|---|
| `directory` | absolute path | required | Must be writable |
| `max_body_bytes` | integer | `1048576` | |
| `retention` | duration | `24h` | At least `10m` |
| `on_error` | `continue` \| `defer` | `continue` | |
| `queue_depth` | integer | `1024` | |
| `max_queue_bytes` | integer | `67108864` | At least twice `server.max_message_bytes` |

## `ramps.<name>`

| Key | Type | Default | Notes |
|---|---|---|---|
| `domain_groups` | list | required | Exactly one contains `*` |
| `domain_groups[].name` | string | required | Unique in the ramp |
| `domain_groups[].domains` | list | required, not empty | Each domain in one group only |
| `domain_groups[].mx` | list of suffixes | none | Not on the catch-all |
| `senders` | list | required | May be empty |
| `senders[].match` | pattern | required | Domain, `*.subdomain` or address |
| `senders[].match_on` | `envelope` \| `from_header` \| `either` | required | |
| `senders[].chain` | list of route names | required, not empty | At most one overflow route, last |
| `default_chain` | list of route names | required unless `strict_senders` | Ends in an overflow route, unless `allow_no_overflow_route` |
| `strict_senders` | bool | `false` | |
| `exhausted_chain_reply` | `"451"` \| `"550"` | `"451"` | |
| `thread_affinity` | bool | `false` | |
| `allow_no_overflow_route` | bool | `false` | |
| `black_hole_allowed` | bool | `false` | Needs `allow_no_overflow_route` and `strict_senders: false` |
| `routes` | list | required | |

## `ramps.<name>.routes[]`

| Key | Type | Default | Notes |
|---|---|---|---|
| `name` | string | required | Unique in the ramp |
| `overflow` | bool | `false` | |
| `downstream` | downstream | required | Below |
| `downstream_overrides` | map of group name to downstream | none | |
| `identity.envelope_from` | template | required | Domain must be written out |
| `identity.set_headers` | map of header to template | none | |
| `identity.remove_headers` | list of headers | none | |
| `identity.header_rewrites` | list of `{header, pattern, replacement}` | none | Not for identity headers |
| `identity.body_rewrites` | list of `{pattern, replacement}` | none | |
| `identity.unstable_headers` | list of headers | none | Not identity headers |
| `preflight.enabled` | bool | `true` | Absent block means no preflight |
| `preflight.spf_include` | string | required when enabled | |
| `preflight.dkim_selector` | string | required when enabled | |
| `preflight.require_dmarc` | bool | `false` | |
| `preflight.strict` | bool | `false` | |
| `warmup.started` | RFC 3339 instant | required | Required on a warming route; not allowed on an overflow route |
| `warmup.schedule.default` | list of integers | required, not empty | Each at least 0 |
| `warmup.schedule.overrides` | map of group name to list | none | |
| `warmup.schedule.share` | list of fractions, or `{mode: auto, …}` | none | Not on the last route of a chain |
| `warmup.schedule.share.floor` | fraction | `0.05` | Auto only |
| `warmup.schedule.share.ceiling` | fraction | `1.0` | Auto only |
| `warmup.schedule.share.gain` | number | `4.0` | Auto only; above 0 |
| `warmup.schedule.share.fill_by` | fraction | `0.6` | Auto only |
| `warmup.schedule.share.tail.below` | number | `0.1` | Auto only; 0 to below 1 |
| `warmup.schedule.share.tail.ceiling` | fraction | `1.0` | Auto only |
| `recipient_frequency.mode` | `to_address` \| `to_domain` | required | |
| `recipient_frequency.window.unit` | `hourly` \| `daily` \| `weekly` | required | |
| `recipient_frequency.window.count` | integer | required | At least 1 |
| `recipient_frequency.threshold` | integer | required | At least 1 |

A fraction is above 0 and at most 1.

### Downstream

Used by `downstream` and each `downstream_overrides` entry.

| Key | Type | Default | Notes |
|---|---|---|---|
| `host` | string | required | |
| `port` | integer | required | |
| `tls` | `off` \| `opportunistic` \| `required` \| `required_verify` | `required_verify` | |
| `auth.username` | string | required with `auth` | |
| `auth.password` | string | required with `auth` | |
| `pool.max_connections` | integer | required | At least 1 |
| `pool.idle_ttl` | duration | required | |
| `pool.max_messages_per_connection` | integer | required | At least 1 |
| `timeouts.connect` | duration | `10s` | |
| `timeouts.command` | duration | `30s` | |
| `timeouts.data` | duration | `120s` | |
| `smtputf8` | bool | `false` | |
| `assume_8bitmime` | bool | `false` | |
