# simmer

An SMTP relay facade that applies a domain reputation warm-up ramp.

Simmer sits between an application and one or more real SMTP providers. It
accepts a message, selects an outbound route according to quota state, rewrites
the message's identity to match that route, forwards it synchronously, and
returns the downstream's verdict to the client on the same connection.

**Simmer is temporary infrastructure.** It exists for the duration of a warm-up
and is then removed. `docs/SPEC.md` is the authoritative specification;
`DECISIONS.md` records divergences from it.

## What it is not

Simmer is **not an MTA**, and the distinction is load-bearing rather than
pedantic. There is no spool, no queue, no retry scheduler and no DSN generation.
The client connection is held for the duration of the downstream conversation; a
message is either delivered during that conversation or the client is told it was
not. The only things persisted are quota state and recipient-frequency events.

It also holds no DKIM key material and performs no signing. The downstream signs,
exactly as it would have if the application had connected to it directly.

## The cutover invariant

Every design decision is subordinate to §1.1 of the spec: **Simmer's output
identity must be exactly expressible as application-side configuration**, so it
can be removed in either order relative to the application being reconfigured.
Two consequences worth knowing before editing anything:

- Rewrites are **absolute assignments**, never relative transformations. "Set the
  `From:` domain to `newbrand.com`" is fine; "append `.new` to the sending
  domain" is not, because applying it to already-migrated traffic corrupts it.
- Rewrites are **stable**: applying one twice changes nothing. Identity fields
  (`envelope_from`, `From:`, `Sender:`, `Message-ID:`) must satisfy this with no
  override. Other headers may be exempted by naming them in the route's
  `unstable_headers`, which is an acknowledgement that the header is
  migration-only and will change the moment the application is cut over.

A corollary that catches people out: Simmer never emits a reply that would make a
client record *permanent* state about a message or recipient. A chain exhausted
by its daily quota returns `451`, not `550` — a `550` would put a perfectly
deliverable recipient on a suppression list that outlives Simmer by years.

## Status

All ten phases of `docs/SPEC.md` §13, except phase 9, which is void — see D-047
below — plus phase 11, which came after them.

**Phase 1** — configuration loading, full startup validation, structured
logging, the container skeleton.

**Phase 2** — SMTP ingress and the outbound leg. Simmer accepts a message on
port 25, authenticates the client, buffers the body, forwards it to a
downstream over TLS, and maps the downstream's verdict back on the same
connection.

**Phase 3** — the warm-up itself. Route selection now walks the chain by quota
state: a warming route carries traffic up to its daily allowance for that
recipient's domain group, and everything beyond it falls through to the overflow
route. Counters increment on downstream `2xx` only, via the §7.4
reserve/send/commit protocol, so a failed send never consumes allowance and
concurrent sessions cannot both claim the last slot.

**Phase 4** — the rewriting engine, which is what makes a route's *outbound
identity* mean anything. Authentication artefacts are stripped unconditionally
(§6.5), `remove_headers` and `set_headers` are applied with §6.3's templates
rendered against the message as it arrived, a `Received:` header is prepended, and
the envelope sender is computed from the route. §6.6's stability property —
`rewrite(rewrite(m)) == rewrite(m)` — is enforced at startup against a synthetic
probe and again as a property test over generated messages.

Phase 4 also built the §12.3 acceptance suite ahead of schedule: two Mailpit traps
on a compose profile, a ramp walked across simulated days by moving
`warmup.started`, and an assertion that both arrangements of the cutover invariant
produce byte-equal output.

**Phase 5** — body rewriting (§6.4), which completes §6. Each `text/*` part is
decoded according to its `Content-Transfer-Encoding` and charset, the route's
`body_rewrites` patterns are applied in order, and the part is written back in the
encoding and charset it arrived with. A URL split across a quoted-printable soft
line break matches, which is the case §6.4 exists for. Attachments, non-text
parts, and signed or encrypted parts are never touched, and a part that cannot be
decoded is left alone, warned about and counted.

A part that matches nothing is not re-encoded at all — a body in which nothing
matched is forwarded as the bytes it arrived as, MIME boundaries and trailing
whitespace included.

**Phase 6** — the recipient-frequency constraint (§7.3), preceded by a policy
reversal: **a transaction may now carry exactly one recipient** (D-047, below).

A route may declare `recipient_frequency`, and a recipient at or over its
threshold inside a rolling window makes that route **ineligible** — the message
steers to the next link in the chain and is never dropped. The recipient is
normalised first, so `Bob.Smith+news@gmail.com` and `bobsmith@gmail.com` count as
the one inbox they are: lowercased, everything from `+` to `@` removed, and dots
folded out of the local part at the providers listed in `dot_insensitive_domains`.

What is stored is a **keyed hash**, never the address: HMAC-SHA256 under a salt
minted once and persisted in `instance_config`, truncated to 16 bytes. No
plaintext address reaches the database, a log line or a metric label. Events are
recorded only on a downstream `2xx`, in the same transaction that commits the
quota, and only for routes that declare a constraint; an hourly sweeper evicts
anything past the longest configured window plus a margin.

**Phase 7** — the control plane (§9): a Prometheus exporter, a read API, a write
API and dry run. The endpoints are below under [Control plane](#control-plane).

Two things in it are worth knowing before you use it. The read API reports both
what the *schedule* says and what the *row* says, and flags the difference,
because `quota_usage.allowance` is authoritative once written (D-026) — a
schedule edit plus a restart does not raise today's ceiling, and an API that
reported the schedule would mislead you at exactly the wrong moment. And every
mutation tells you which chains it has just left with no eligible route: pausing
a route, or setting an allowance of zero, is a legitimate thing to do and also
the thing most likely to make every message on a chain `451` without anyone
meaning it.

**Phase 8** — the DNS preflight (§6.7), which completes §6. A route with a
`preflight` block has its outbound identity's domain checked for SPF, DKIM and
DMARC at startup and every fifteen minutes. It is **non-blocking by design**: a
failure is a `WARN` and a `0` gauge and nothing else, and only `strict: true`
makes the route ineligible — at which point the message *steers* to the next link
exactly as §7.3 does, and a chain with none left is §10.3's `451`, never a `5xx`.
A route with no verdict yet is eligible, so a slow resolver at boot cannot empty a
chain.

**Phase 10** — hardening. §8.3's connection pool: per route, `max_connections`
held as a bound rather than a hint, `RSET` between messages, `NOOP` validation of
a connection idle beyond a short threshold, and retirement at `idle_ttl` or
`max_messages_per_connection`. §10.4's shutdown drains it. Two things about it are
worth knowing and are below under [Connection pooling](#connection-pooling).

Phase 10 also fixed a defect carried since phase 2: the decoy hash that equalises
the cost of a failed login now takes its parameters from the credentials actually
configured, rather than from a fixed guess that only matched them by coincidence
(`DECISIONS.md` D-066).

**Phase 11** — listeners on 25, 587 and 465, inbound TLS, and a sender ACL
(`docs/INGRESS.md`, D-070, D-071). Each listener has its own `tls` and `auth`
policy, defaulting to what its port's RFC says. `STARTTLS` and implicit TLS use one
PEM certificate read at startup. Each user carries `grants.send_as`, the sender
identities it may present, and an authenticated message whose envelope or `From:`
falls outside them is refused `550 5.7.1`. Details under
[Listeners and TLS](#listeners-and-tls) and [Sender grants](#sender-grants).

What works today: **an end-to-end relay that applies the ramp, rewrites both the
identity and the body, paces how often one recipient hears from a warming route,
pools its downstream connections, accepts submissions over verified TLS from
applications limited to their own sender identities, and can be inspected and
steered without a restart.** What does not:

- **No scopes on admin tokens.** Every token can do everything; a token that
  could read but not mutate is a plausible ask and is not built.
- **No pre-authentication limits.** A client that never authenticates is bounded
  by `max_concurrent_sessions` and the timeouts like any other, but there is no
  separate, tighter budget for it (D-072). That matters only once Simmer listens
  somewhere `allowed_cidrs` cannot be tight.

And one thing that will not arrive, because it is a decision rather than a gap:

- **A transaction may carry exactly one recipient.** A second `RCPT TO` is
  refused `452 4.5.3 multiple recipients not permitted`, and no configuration
  changes that. An application that batches recipients into one transaction must
  send one message per recipient instead — which is what it will be doing anyway
  once Simmer is unplugged. The reason is that SMTP allows one reply per
  transaction, so several per-recipient outcomes have to be collapsed into a
  single code, and every way of doing that either drops mail silently or records
  one recipient's permanent failure against the others. See `DECISIONS.md` D-047
  and `docs/RECIPIENTS.md`.

```sh
$ openssl s_client -quiet -starttls smtp -connect simmer:587 -servername simmer.internal
EHLO me
250-simmer.internal greets me
250-PIPELINING
250-8BITMIME
250-SIZE 26214400
250-AUTH PLAIN LOGIN
250 AUTH=PLAIN LOGIN
```

## Quick start

```sh
cp .env.example .env        # fill in the values
docker compose up -d --build
curl -s localhost:8080/health | jq
```

Or locally, against a Postgres you already have:

```sh
export DATABASE_URL=postgres://simmer:simmer@localhost/simmer
export SIMMER_ADMIN_TOKEN=dev SIMMER_CFAPP_HASH='$argon2id$...'
export POSTAL_USER=x POSTAL_PASS=y SENDGRID_KEY=z
cargo run --bin server
```

`SIMMER_CONFIG` overrides the config path (default `simmer.yaml`).

## Configuration

YAML, with `${ENV_VAR}` interpolation resolved at startup. An unresolvable
reference is a fatal startup error, as is any validation violation — and **all of
them are reported together**, because a service that takes minutes to build
should not be debugged one error at a time.

Interpolation happens over the parsed YAML tree rather than the raw text, so a
secret containing a newline and a colon cannot restructure the document.

Unknown keys are rejected. There is no hot reload (spec §2.2): changes need a
restart.

**One rule catches people out.** A route's `identity.envelope_from` must have a
**constant domain** — the part after the `@` cannot contain a template. The local
part can (`bounce+{{recipient.local}}@newbrand.com` is fine); the domain cannot.
This is not fussiness about templates: a route exists to build reputation for one
domain, so a route whose domain varies per message warms nothing while its quota
row still reads like a healthy ramp. Startup refuses it rather than letting you
discover that in six weeks of unchanged deliverability. See `DECISIONS.md` D-069.

See `simmer.yaml`, which is commented throughout, and `.env.example` for the
variables it expects.

### Listeners and TLS

`server.listeners` has one entry per port. An entry that names only an address
takes its port's RFC defaults, and startup logs each listener's effective policy:

| Port | `tls` | `auth` | |
|---|---|---|---|
| 25 | `off` | `optional` | RFC 5321 transfer — what Simmer did before listeners existed |
| 587 | `starttls_required` | `required` | RFC 6409 submission |
| 465 | `implicit` | `required` | RFC 8314 submissions |
| any other | `off` | `optional` | |

`tls` is one of `off`, `starttls` (offered, a client may decline), `starttls_required`
(nothing but `EHLO`, `NOOP`, `RSET`, `QUIT` and `STARTTLS` until the handshake) and
`implicit`. `auth` is one of `disabled`, `optional` and `required`.

```yaml
server:
  listeners:
    - address: "0.0.0.0:25"
      auth: required
    - address: "0.0.0.0:587"
    - address: "0.0.0.0:465"
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"   # leaf first
    private_key: "/etc/simmer/tls/privkey.pem"
  auth:
    allow_insecure_auth: false
```

Four things worth knowing:

- **`allow_insecure_auth` means what it says, and defaults false.** AUTH over an
  unencrypted session is refused `538 5.7.11` and is not advertised there. Before
  phase 11 this key had to be *true*, because there was no TLS to use instead; a
  config carried over will still work, but you probably want it false now.
- **The certificate is checked at startup as the listener will load it.** A
  missing, unreadable, unparseable or mismatched file refuses to start, alongside
  every other violation. The container runs as **UID 1000**, so mount the key
  readable by that user — the error says so if you do not. A certificate that
  does not cover `server.hostname`, or that expires within fourteen days, is a
  startup warning rather than a failure, because refusing to start would take the
  plaintext listeners down too. Rotation is a restart.
- **Plaintext pipelined behind `STARTTLS` drops the connection.** Bytes sent in the
  same packet as `STARTTLS` came from whoever is on the wire, not the TLS peer.
- **TLS does not make Simmer a public MX** (spec §2.3). It lets Simmer sit on a
  segment where cleartext credentials are unacceptable, which is a much smaller
  claim. `allowed_cidrs` still applies to every listener, and so does one shared
  `max_concurrent_sessions`.

`server.listen` and `server.auth.required` were removed; a config using either
fails to start with a message saying what replaced it.

### Sender grants

Every user in `server.auth.users` carries the sender identities it may present,
in the same pattern grammar as `senders`: an exact domain, `*.subdomain`, or a full
address.

```yaml
    users:
      - username: "cfapp"
        password_hash: "${SIMMER_CFAPP_HASH}"
        grants:
          send_as: ["oldbrand.com", "*.oldbrand.com", "newbrand.com"]
```

For an authenticated session, the `MAIL FROM` address is checked at `MAIL FROM` and
the first `From:` address at the final dot; either outside the grants is `550 5.7.1
sender not permitted`, and so is a message with no parseable `From:`. Anything not
granted is refused. Watch `simmer_sender_not_permitted_total`: it means either a
misconfigured application or somebody else's credentials.

**The grants decide whether a message is accepted, never where it goes.** Routing
is still `senders`, exactly as before, and the username still plays no part in it.
Two users granted the same identity send byte-identical mail.

**They apply only to sessions that authenticated.** A listener with `auth:
optional` — port 25's default — accepts unauthenticated mail with any sender from
any address in `allowed_cidrs`, and startup warns about each such listener once
users exist. Set `auth: required` wherever that is too much trust.

To mint a password hash, pipe the password in; it is never taken as an argument,
where it would land in shell history:

```sh
printf '%s' "$PASSWORD" | docker run -i --rm simmer:local hash-password
```

### The ramp

A warming route's `warmup.schedule` is indexed by **elapsed days from
`warmup.started`**, not by calendar date. A route started at 09:00 rolls over at
09:00 every day, is immune to DST, and can never see a 23- or 25-hour window. Past
the end of the array the final value repeats — routes never auto-graduate to
uncapped; that is a deliberate act (`POST /routes/{name}/graduate`, or editing the
config).

Quota is keyed on `(route, domain_group)` because mailbox providers throttle
independently of one another, and bucketed per day. **An unused allowance does not
carry over**, and a route that sent nothing yesterday still advances its day
index.

Overflow routes are never capped, but they *are* counted — "how much is spilling
to overflow" is the number that tells you whether the ramp is set too low.

### SMTPUTF8

`EHLO` advertises `SMTPUTF8` only when **every** route reachable from a chain
sets `downstream.smtputf8: true`, and the default is `false`. Simmer cannot know
at `EHLO` time which route a message will take, so it can only promise what all
of them can deliver. A UTF-8 address presented when the capability was not
advertised is refused `550 5.6.7` at `MAIL FROM`, rather than discovered
mid-relay. See `DECISIONS.md` D-018.

### Timeout budget

The sum of the per-stage downstream timeouts (`connect` + `command` + `data`)
must sit comfortably below the **client's** own SMTP timeout. Simmer holds the
client connection open for the whole downstream conversation, so if Simmer is
still waiting on the downstream when the client gives up, the client sees a
dropped connection rather than the `451` Simmer was about to send — and a `451`
it can act on is much more useful than a socket error it cannot.

With the shipped defaults the downstream budget is 10s + 30s + 120s = 160s
against a client `data` timeout of 300s. If you raise the downstream timeouts,
check that relationship still holds.

### Connection pooling

Each route has its own pool, configured under `downstream.pool`. Two things about
it are not obvious from the keys:

**`max_connections` is a bound, not a hint.** It is held for the whole time a
connection is in use, so a route can never have more connections open than it
says — which is the point: it protects the downstream from a burst as much as it
saves Simmer a handshake. A session that cannot get one within the route's
`connect` budget is answered `451 4.4.5 downstream connection pool exhausted`,
its own error class so that a dashboard can tell it apart from a downstream that
is refusing connections. The two call for opposite responses. If you see it,
raise `max_connections`.

**`idle_ttl` should sit below whatever the downstream's own idle timeout is.** If
it does not, Simmer will regularly pick up connections the provider has already
closed. That is survivable — the message is retried once on a fresh connection and
the client never sees it — but it costs a reconnect, and
`simmer_pool_retries_total` is the metric that tells you it is happening. The
retry is deliberately never attempted after the message body has been sent
(`DECISIONS.md` D-068): past that point a delivery may already have happened, and
retrying would send it twice.

## Control plane

On `admin.listen`, port 8080 by default. `/health`, `/healthcheck` and `/metrics`
are open; everything else needs `Authorization: Bearer <token>`, including the
reads — `/routes` discloses every downstream hostname and the whole routing
shape. See `DECISIONS.md` D-055.

| | |
|---|---|
| `GET /health` | Liveness plus database reachability. `503` when the database is down |
| `GET /metrics` | Prometheus exposition (§9.1) |
| `GET /routes`, `GET /routes/{name}` | Configuration plus live state: warm-up day, per-group allowance and usage, paused, graduated, preflight results, pool statistics |
| `GET /quota?route=&group=` | The same windows, filtered. Both filters optional and independent |
| `POST /routes/{name}/pause`, `/resume` | Make a route ineligible without a restart. Persisted |
| `POST /routes/{name}/graduate` | Pin to the final schedule value. `{"graduated": false}` reverses it |
| `POST /routes/{name}/allowance` | `{"domain_group": …, "allowance": N\|null}`. Expires at the route's next day boundary |
| `POST /quota/reset` | Destructive. Needs `"confirm": "reset"` |
| `POST /dryrun` | What *would* happen. Sends nothing, reserves nothing, writes nothing |

```sh
TOKEN=$SIMMER_ADMIN_TOKEN

# Why is mail deferring for Google?
curl -sH "Authorization: Bearer $TOKEN" localhost:8080/quota?group=google | jq

# Let the warming route send more to Google, today only.
curl -sXPOST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"domain_group":"google","allowance":500}' \
  localhost:8080/routes/warming/allowance | jq

# What would this message do?
curl -sXPOST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"envelope_from":"jane@oldbrand.com","from_header":"Jane <jane@oldbrand.com>",
       "recipients":["bob@gmail.com"],"body":"see https://oldbrand.com/x"}' \
  localhost:8080/dryrun | jq
```

**Reading `/routes`.** Each route also reports its `preflight` verdict — `null`
means nothing has been checked, which is not the same as a pass and is reported
differently on purpose — and its `pool`: `max_connections` and the live `idle` and
`active` counts, plus lifetime `opened`, `reused`, `retired` and `discarded`.
`reused` far below `opened` means connections are not surviving between messages;
a climbing `discarded` means the downstream is closing them underneath you.

Each domain group reports `scheduled` (what the
configuration says today), `allowance` (what the row says, which is what the
reservation protocol enforces), `override`, `committed`, `reserved`, `headroom`
and `drift`. `drift: true` means the row and the schedule disagree — the expected
state for the rest of the day after a schedule change, and the thing to look at
first when the ramp is not doing what the YAML says. `row_exists: false` means
nothing has been sent on that route and group today.

**Dry run** is the tool for validating a configuration before it carries traffic.
It runs the real sender match, the real chain walk and the real rewrite engine, so
what it shows is what would go out; the tests assert its chain evaluation is
identical to the relay's across every skip reason. It reports each
`body_rewrites` pattern's match count **including the ones that matched nothing**,
which is usually the answer to "why is my rewrite not firing".

**Every mutation is audited** at `INFO` with the acting token's name, the target,
and the value it replaced. Name your tokens (`admin.tokens`) and that line names
somebody; leave `auth_token` alone and it says `default`. See D-053.

**Watch `simmer_admin_auth_failures_total{reason="invalid"}`.** The write API can
pause a route, and a run of those is somebody guessing.

## Development

```sh
docker compose up -d simmer-db   # the quota tests need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check                 # licences, advisories, sources
docker compose up -d --build     # not optional before pushing
```

**Every dependency comes from crates.io.** There are no git dependencies, so
`deny.toml` has no `allow-git` allow-list and `unknown-git = "deny"` rejects all
of them; `cargo build` needs no credentials for a private repository. `hs-utils`
was the one exception until phase 7 — it supplied a single stdlib-only function,
which now lives in `src/healthcheck.rs` behaving identically (D-060). If you need
something from the estate's shared crates, copy it and record why, rather than
taking the dependency back.

The §12.3 acceptance suite runs against its own stack and is not part of
`cargo test` — it needs Docker and about two minutes of container restarts:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

It walks a warm-up across simulated days by moving `warmup.started` and
re-creating the container, and it is the only tier that proves both arrangements
of the cutover invariant (§1.1) produce byte-equal output. It also submits over
587 with `STARTTLS`, verifying the certificate a one-shot `tls-init` service mints
for each run. `docs/ACCEPTANCE.md`
explains the topology; `DECISIONS.md` D-042 explains what will bite.

`DATABASE_URL` is needed only by `tests/quota*.rs`, which use `#[sqlx::test]` to
get a fresh database per test. Faking Postgres there would defeat the point:
§12.3's overshoot test is a claim about what two transactions do to one row at the
same time, and a post-hoc increment passes every other test in the suite and fails
that one. Nothing about the Docker build depends on it — the queries are checked
at runtime, never by `query_as!` (see `CLAUDE.md`).

CI runs all of these. Note this is *not* the house norm — the standard
hikari-systems `build.yml` builds and pushes the image with no test or lint gate.
Simmer adds one because its central correctness argument is a property test.

The `docker compose up` step is not ceremony: it is the only thing that exercises
the privileged port-25 bind, the tmpfs the §8.1 buffer spills onto, and the
platform root store the §8.2 `required_verify` mode needs.

## Layout

```
src/config/     the §4.1 schema, ${ENV_VAR} interpolation, §4.2 validation
src/routing/    sender matching (§5.4), domain groups (§3.2.2), the chain walk
src/smtp/       §5 ingress: listeners, state machine, AUTH, DATA buffer, replies
  tls.rs          §5.1's certificate: loaded once, checked by §4.2 the same way
  acl.rs          §5.3's sender grants. Gates acceptance, never routing (D-071)
src/downstream/ §8 outbound: TLS, the SMTP client, the §10.1 reply mapping
  pool.rs         §8.3's per-route pool. max_connections is a bound, not a hint
src/quota/      §7 day index, allowance, the reserve/commit protocol, sweeper
src/frequency/  §7.3 normalisation, the keyed hash, the rolling window, eviction
src/models/     runtime sqlx over &PgPool, house pattern
src/rewrite/    §6 the rewriting engine: templates, headers, encoding, stability
src/preflight/  §6.7 the DNS preflight: three checks, a registry, an interval
src/relay.rs    decide -> reserve -> rewrite -> relay -> commit/release
src/metrics.rs  §9.1 counters and the Prometheus recorder
src/admin/      the §9 control plane: reads, writes, dry run, /metrics
  view.rs         §9.2's projections, as pure functions. D-026's drift flag
  auth.rs         §9.3's bearer token, and O-11's answer to whose it was
  mutate.rs       the four mutations, the audit line, §14.1's warnings
  dryrun.rs       §9.4, over the real engine
src/db.rs       pool construction and migrations
src/healthcheck.rs  the `healthcheck` subcommand the container's HEALTHCHECK runs
src/hash_password.rs  `server hash-password`: argon2id from stdin, never argv
src/bin/loadgen.rs  the acceptance suite's bulk sender; not in the shipped image
migrations/     plain SQL, applied at startup
tests/support/  a scripted fake downstream (§12.3)
tests/rewrite_stability.rs  §6.6 as a property test over generated messages
tests/admin_api.rs   §9 against the real router and real Postgres
tests/pool.rs        §8.3 from the downstream's side: connections, not intentions
tests/ingress_tls.rs §5.1 and §5.3 end to end: STARTTLS, implicit TLS, the ACL
tests/metrics_endpoint.rs  §9.1 against a real recorder; its own binary
tests/acceptance.rs  §12.3 against real mail servers; behind --ignored
simmer.acceptance.yaml  config for the acceptance stack
docs/SPEC.md    the specification
docs/STATE.md   where the build has got to (snapshot, for session handover)
docs/ACCEPTANCE.md  the §12.3 acceptance harness: design, and now built
DECISIONS.md    divergences from it, and the questions still open
LICENSES.md     dependency licence findings
```

## Deployment

Simmer is **not** deployed on the hikari-systems spot fleet. The fleet's roll
method requires target capacity ≥2 and replaces instances one at a time, so a
standard deploy would run two Simmers against one database for the minutes a
replacement takes to build.

**That window is not a quota-overshoot window**, and an earlier version of this
section said it was. The warm-up counters are safe across instances: §7.4's
reservation does its headroom check and its write inside one transaction holding a
row lock, and Postgres serialises contenders for that row whether they are two
tasks in one process or two processes on different hosts. Nothing in the quota
path is per-instance — the counters, the reservations and the route states are all
rows. `tests/quota_multi_instance.rs` races two independent connection pools to
show it.

What the window actually costs is narrower, and neither part is fixed by a lock:

- **Config skew.** `quota_usage.allowance` is authoritative once written (D-026),
  so two instances running different schedules will have whichever writes the
  day's first row set that day's ceiling, and the other will silently honour it.
- **The recipient-frequency race** (D-049). §7.3's count is read outside the
  reservation transaction, so `C` sends that all read before any of them commits
  can take the window to `threshold + (C - 1)`. It is a reputation-shaping
  heuristic rather than an accounting invariant, and it is already true within one
  instance — a second instance widens the window rather than introducing the bug.

Spec §2.2 says one instance owns its quota state, and simmer stays single-instance,
but the reason is the two constraints above rather than the counters. See
`DECISIONS.md` D-061 and D-007, and `docs/MULTI_INSTANCE.md` — which the spec's
author needs to rule on, since §2.2 rules multi-instance out in as many words.

The container publishes no ports by default. Simmer belongs on a trusted internal
segment (spec §2.3) whatever its listeners are configured to do: inbound TLS lets it
sit where cleartext credentials are unacceptable, not where hostile peers can reach
it. Exposing an SMTP port to a host interface must be a deliberate act. To serve
587 or 465, mount a certificate and key readable by UID 1000 and name them in
`server.tls`; see [Listeners and TLS](#listeners-and-tls).
