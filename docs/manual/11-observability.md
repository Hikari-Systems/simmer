# Logs, metrics and OpenTelemetry

Simmer can tell you what it is doing in three ways: log lines on standard output,
Prometheus metrics on the admin port, and an optional OpenTelemetry export of
traces, metrics and logs.

## Logs

```yaml
logging:
  level: info       # default
  format: json      # default; or text
```

| Key | Default | Meaning |
|---|---|---|
| `level` | `info` | What to log. A level (`error`, `warn`, `info`, `debug`, `trace`), or a filter such as `info,sqlx=warn` that sets levels per component |
| `format` | `json` | `json`, one object per line, for log collectors; `text` for reading in a terminal |

`RUST_LOG`, if set, overrides `level` for standard output. It is handy for a
one-off debugging session without editing the configuration.

Every message has a `correlation_id` that appears on every line about it, and
can be put in a header with the `{{correlation_id}}` template. A message's lines
record which ramp and sender rule it matched, every route considered and why
each was skipped, the route chosen, the provider's reply, and the total time.

**What is never logged:** message bodies; recipient addresses below `debug`; the
link proxy's query strings and cookies; passwords and tokens. A provider's reply
text often quotes the recipient back, so it is logged with every address's local
part replaced by `*`: `550 5.1.1 <*@example.com> unknown user`.

## Prometheus metrics

`/metrics` on the admin port is **off** unless you turn it on:

```yaml
admin:
  metrics: true
  # or, with the idle expiry changed from its default of 24h:
  metrics: { enabled: true, idle_timeout: 6h }
```

Off, `/metrics` answers `404` and Simmer keeps nothing in memory for it. On, it is
open to anyone who can reach the admin port, with no token.

A counter that nothing has increased for `idle_timeout` (default 24h, at least
1m) is dropped, and starts again from zero if it is increased later, which
Prometheus's `rate()` and `increase()` treat as an ordinary counter reset. That
stops counters labelled with values clients choose, such as the sender domain in
`simmer_unmatched_sender_total`, from growing for the life of the process. The
pruning happens while answering a scrape, so turn `/metrics` on only where
something scrapes it.

Every series labelled with a `route` also carries `ramp`, because a route name is
only unique within its ramp. Include `ramp` in `sum by (...)` clauses.

### Worth an alert

| Metric | Means |
|---|---|
| `simmer_unmatched_sender_total` | Mail whose sender matched no rule took the default chain. Often a typo in `senders`, sending unwarmed mail at full volume |
| `simmer_downstream_config_error_total` | A provider refused at a stage that suggests misconfiguration, such as unfinished domain authentication. Invisible in the mail flow otherwise |
| `simmer_sender_not_permitted_total` | A logged-in client tried to send as an identity its grants do not allow: a misconfigured application, or someone else's credentials |
| `simmer_preflight_ok{route,check} == 0` | A route's DNS records are not what its provider needs |
| `simmer_ambiguous_delivery_total` | The connection to a provider dropped after the message was sent and before the reply. The retry may be a duplicate |
| `simmer_reservation_expired_total` | Reservations cleared by the sweeper rather than by their own message: crashes, or a mistuned timeout |
| `simmer_quota_unavailable_total` | Messages answered `451` because the database was unreachable |
| `simmer_admin_auth_failures_total{reason="invalid"}` | Wrong admin tokens. The admin API can pause routes, so a run of these is someone guessing |
| `simmer_no_overflow_total{outcome="discarded_black_hole"}` | Mail accepted and thrown away, if you set `black_hole_allowed` |
| `simmer_ambiguous_terminator_total` | Messages refused for the SMTP smuggling pattern. A steady trickle is usually one application with bare line endings; anything else is worth finding the sender for |
| `simmer_capture_deferred_total` | Mail being stopped because the capture cannot be written, with `capture.on_error: defer` |
| `simmer_thread_affinity_total{outcome="over_cap"}` | Replies sent past a route's cap by thread affinity |

### Traffic and quota

| Metric | Labels | Meaning |
|---|---|---|
| `simmer_messages_total` | `ramp`, `route`, `domain_group`, `outcome` | Messages, by result |
| `simmer_quota_allowance` | `ramp`, `route`, `domain_group` | Today's cap. `+Inf` for an overflow route |
| `simmer_quota_committed` | `ramp`, `route`, `domain_group` | Messages delivered today |
| `simmer_quota_reserved` | `ramp`, `route`, `domain_group` | Messages being sent right now |
| `simmer_warmup_day` | `ramp`, `route` | The route's day index. Negative before `started` |
| `simmer_route_paused` | `ramp`, `route` | 1 while paused |
| `simmer_route_skipped_total` | `ramp`, `route`, `reason` | Routes passed over in a chain, by reason |
| `simmer_partial_ramp_share` | `ramp`, `route`, `domain_group` | The share currently offered, when a partial ramp is holding some back |
| `simmer_ramp_selected_total` | `ramp`, `source` | Which rule picked each message's ramp |
| `simmer_ramp_header_rejected_total` | `reason` | `X-Simmer-Ramp` headers ignored |
| `simmer_mx_lookups_total` | `result` | MX lookups for domain grouping |
| `simmer_sender_mismatch_total` | | Envelope sender and `From:` disagreed |
| `simmer_body_rewrite_skipped_total`, `simmer_header_rewrite_skipped_total` | | Parts or headers a rewrite should have changed and could not |
| `simmer_recipient_events_evicted_total` | | Recipient-frequency records expired |

### Providers and connections

| Metric | Labels | Meaning |
|---|---|---|
| `simmer_downstream_errors_total` | `ramp`, `route`, `downstream`, `class` | Failures talking to providers |
| `simmer_downstream_latency_seconds` | `ramp`, `route`, `downstream` | Histogram: connecting to the provider until its reply to the message |
| `simmer_pool_connections` | `ramp`, `route`, `downstream`, `state` | Connections held to each provider |
| `simmer_pool_retries_total` | `ramp`, `route`, `downstream` | Messages re-sent because a pooled connection was dead. A steady rate means `idle_ttl` is above the provider's idle timeout |
| `simmer_connections_refused_total` | `reason` | Client connections refused: outside `allowed_cidrs`, or over `max_concurrent_sessions` |
| `simmer_inbound_tls_failures_total` | `mode`, `reason` | Client TLS that did not complete |
| `simmer_sessions_active`, `simmer_sessions_max` | | SMTP connections open, and the limit |

`downstream` is `default` for a route's own provider, or the domain group of a
`downstream_overrides` entry.

### The process

`process_resident_memory_bytes`, `process_open_fds`, `process_max_fds`,
`process_threads` and `process_start_time_seconds` under their standard names,
plus:

- `simmer_db_pool_connections{state}` against `simmer_db_pool_max`: `in_use`
  stuck at the maximum comes before `451 4.3.0`.
- `simmer_reservations_in_flight`, which should be 0 whenever no mail is moving.
- `simmer_auth_verifies_in_flight` against `simmer_auth_verifies_max`: password
  checks running. Each uses about 19 MiB while it runs; stuck at the maximum
  means logins are queueing.
- `simmer_tasks_alive`: growth with a steady number of connections is a leak.

The link proxy's metrics are in [The link proxy](10-link-proxy.md#metrics) and
the capture's in [Capture and replay](13-capture.md).

## OpenTelemetry

Simmer can export traces, metrics and logs over OTLP/gRPC to an OpenTelemetry
collector, or straight to a vendor that accepts OTLP.

```yaml
telemetry:
  endpoint: "${SIMMER_OTEL_ENDPOINT}"      # http://collector:4317; empty means off
  headers: { x-honeycomb-team: "${HONEYCOMB_KEY}" }
  resource: { deployment.environment: production }
```

It is off when the block is absent, when it says `enabled: false`, or when
`endpoint` is empty. The last means one configuration can turn it on or off per
environment by setting or emptying a variable.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | `false` keeps the block in the file and exports nothing |
| `endpoint` | required | The collector, `http://host:4317` or `https://host:4317` |
| `headers` | none | gRPC metadata sent with every export, typically an API key |
| `service_name` | `simmer` | `service.name` on everything exported |
| `resource` | none | Extra resource attributes |
| `level` | `info,sqlx=warn,tiberius=warn` | What is exported, in the same form as `logging.level` but separate from it |
| `traces` | `true` | Export spans |
| `sample_ratio` | `1.0` | The fraction of traces kept, between 0 and 1 |
| `metrics` | `true` | Export the metrics |
| `metrics_interval` | `60s` | How often metrics are exported. At least 1s |
| `metrics_temporality` | `delta` | `delta` or `cumulative` |
| `logs` | `true` | Export log records |
| `timeout` | `10s` | Per export |
| `recipients` | `none` | Whether spans carry the recipient ([below](#recipients-in-traces)) |
| `recipient_hash_key` | none | The key for `recipients: hashed` |

Turning off all three of `traces`, `metrics` and `logs` is refused: use
`enabled: false` instead.

**Configuration comes only from this block.** The standard `OTEL_*` environment
variables are not read, and `RUST_LOG` does not affect what is exported, so
turning a container's own logging up to `debug` does not send debug lines to a
vendor. Use `${VAR}` in the block for anything that differs per environment.

**It only observes.** No trace header is added to relayed mail or to link proxy
requests. A collector that is down or slow costs telemetry, never mail: Simmer
connects lazily, exports run separately from message handling, and failed
exports are logged to standard output as warnings.

An `http://` endpoint to a host other than the local machine is a startup
warning when `headers` are set, because they usually carry an API key.

### What is exported

**Traces**, one per SMTP connection:

- `smtp.session`: the connection.
  - `smtp.transaction`: one message, with its `correlation_id` and final reply code.
    - `simmer.relay`: the ramp, route, domain group, day and result, with children:
      - `simmer.route`: the chain walk, with every skip reason;
      - `simmer.rewrite`;
      - `smtp.downstream`: the provider's reply code, the stage, and whether the
        pooled-connection retry happened;
      - `simmer.quota.resolve`, and after a delivery, `simmer.quota.usage`.
- Also `simmer.preflight`, `admin.request` and `link_proxy.request` spans.

A deferred or refused message's spans are marked as errors.

**Metrics**: the same series as `/metrics`, with the same names and labels. This
is independent of `admin.metrics`: either, both or neither can be on. With
`delta` temporality (the default), counters and histograms report changes since
the last export, and a series that stops changing is dropped from memory after
the next export. Gauges are always cumulative. Use `cumulative` for a backend
that refuses delta sums; Simmer then keeps every series it has seen, up to 2,000
per metric.

**Logs**: every log record, carrying the trace and span it was written in.

The same privacy rules as the logs apply: no bodies, no link proxy query strings,
and no recipient addresses, unless you opt in below.

### Recipients in traces

By default, nothing in the export identifies a recipient. That makes per-recipient
questions ("what happened to mail for this customer?", "which recipients are
being deferred?") hard to answer. `recipients` can put the recipient on spans:

```yaml
telemetry:
  endpoint: "${SIMMER_OTEL_ENDPOINT}"
  recipients: hashed
  recipient_hash_key: "${SIMMER_TELEMETRY_RECIPIENT_KEY}"
```

| `recipients` | On `smtp.transaction` and `simmer.relay` | On `smtp.downstream` |
|---|---|---|
| `none` (default) | nothing | nothing more |
| `domain` | `simmer.recipient.domain` | `smtp.response.text`, the provider's reply with addresses masked (`<*@example.com>`) |
| `hashed` | the above, and `simmer.recipient.hash` | the same |
| `address` | the above, and `simmer.recipient.address` | `smtp.response.text`, unmasked |

- **The provider's reply text** often includes its queue ID, which is what lets you
  find the message in the provider's own logs.
- **A refused `RCPT TO`** (such as the second recipient of a message) is recorded
  as an `smtp.rcpt.refused` event on `smtp.transaction`, with the same attributes
  and `smtp.reply.code`.
- **Spans only.** In no mode does the recipient reach standard output, exported
  log records or a metric label.
- **`address` sends personal data to whoever runs your collector.** You are
  responsible for that, including how long the vendor keeps it. Simmer warns at
  startup if it would travel over plain `http://` to another host.
- `recipients` needs `traces: true`; `recipient_hash_key` is required with
  `hashed`, must be at least 16 bytes, and is refused with any other mode, so a
  stale key cannot sit unnoticed in the configuration.

#### The hash

`hashed` lets you find a known recipient's traces without exporting addresses:
anyone holding the key can hash an address and search for that value. Use the
same key on every instance so their hashes agree, and treat it as a secret.

`simmer.recipient.hash` is HMAC-SHA256, keyed with the UTF-8 bytes of
`recipient_hash_key` exactly as written (not hex- or base64-decoded), of the
recipient address normalised as follows, truncated to the first 16 bytes and
written as 32 lowercase hex characters.

The address is the one given in `RCPT TO`, without the angle brackets. To
normalise it:

1. Remove leading and trailing whitespace.
2. Lowercase the whole address, using full Unicode lowercasing that does not
   depend on locale. (Not ASCII-only lowercasing, and no punycode or Unicode
   normalisation.)
3. Split it at the **last** `@`. With no `@`, the result is the lowercased string;
   stop.
4. If the part before the `@` starts with `"` (a quoted local part), the result is
   the lowercased string; stop.
5. If the local part contains `+`, cut it at the **first** `+`, keeping what comes
   before.
6. If the domain is in the top-level `dot_insensitive_domains` list (default
   `gmail.com` and `googlemail.com`; compared ignoring ASCII case), remove every
   `.` from the local part.
7. The result is the local part, `@`, and the domain.

So `Bob.Smith+news@Gmail.com` and `bobsmith@gmail.com` have the same hash. This is
the same normalisation the [recipient frequency](07-routes.md#recipient-frequency)
limit uses. The key is deliberately not the one that limit uses internally, so the
exported hash cannot be matched against Simmer's own records.

A worked example, with the key `test-key-not-for-production-0001`:

| Step | Value |
|---|---|
| Address | `Fake.Person.0001+promo@GMail.com` |
| Lowercased | `fake.person.0001+promo@gmail.com` |
| Tag removed | `fake.person.0001@gmail.com` |
| Dots removed (gmail.com) | `fakeperson0001@gmail.com` |
| HMAC-SHA256, hex | `4bd0018b6aa447d483ced2f2e29d057c6731f96a0fffe795cd5cc43eb6728780` |
| **`simmer.recipient.hash`** | **`4bd0018b6aa447d483ced2f2e29d057c`** |

To implement the hash elsewhere, for example in a tool that looks up a customer's
traces, test it against [`examples/recipient-hash-vectors.json`](examples/recipient-hash-vectors.json).
Each case gives a key, a domain list, an address, the expected normalised form,
the hash, and the untruncated HMAC to help find which step is wrong. Simmer's own
test suite checks the same file, so the values are what Simmer exports.

## Trying it locally

The repository's acceptance stack includes a dummy collector that writes
everything it receives to a volume; see [`docs/DEVELOPMENT.md`](../DEVELOPMENT.md).
