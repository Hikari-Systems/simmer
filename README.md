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

Phases 1–3 of the ten in `docs/SPEC.md` §13.

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

What works today: **an end-to-end relay that applies the ramp.** What does not,
yet:

- **No rewriting.** The message is forwarded byte for byte, under the identity it
  arrived with. Header and body rewriting are phases 4 and 5, and they are what
  make the route's *outbound identity* mean anything.
- **No connection pool.** One downstream connection per message (phase 10).
- **No admin API and no metrics endpoint.** The counters are being recorded, and
  `pause` / `graduate` / `allowance` are honoured from the database, but nothing
  writes or exports them until phase 7.
- No preflight, no recipient-frequency constraint, no multi-recipient splitting.

```sh
$ printf 'EHLO me\r\nMAIL FROM:<jane@oldbrand.com>\r\nRCPT TO:<bob@gmail.com>\r\nDATA\r\n' | nc simmer 25
220 simmer.internal simmer ESMTP ready
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

See `simmer.yaml`, which is commented throughout, and `.env.example` for the
variables it expects.

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

## Development

```sh
docker compose up -d simmer-db   # the quota tests need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check                 # licences, advisories, sources
docker compose up -d --build     # not optional before pushing
```

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
src/routing/    sender matching (§5.4); chain selection lands in phase 3
src/smtp/       §5 ingress: listener, state machine, AUTH, DATA buffer, replies
src/downstream/ §8 outbound: TLS, the SMTP client, the §10.1 reply mapping
src/quota/      §7 day index, allowance, the reserve/commit protocol, sweeper
src/models/     runtime sqlx over &PgPool, house pattern
src/relay.rs    decide -> reserve -> relay -> commit/release
src/metrics.rs  §9.1 counters; the exporter arrives in phase 7
src/admin/      the §9 control plane; phase 1 has GET /health only
src/db.rs       pool construction and migrations
migrations/     plain SQL, applied at startup
tests/support/  a scripted fake downstream (§12.3)
docs/SPEC.md    the specification
docs/STATE.md   where the build has got to (snapshot, for session handover)
DECISIONS.md    divergences from it, and the questions still open
LICENSES.md     dependency licence findings
```

## Deployment

Simmer is **not** deployed on the hikari-systems spot fleet. That matters: the
fleet's roll method requires target capacity ≥2 and replaces instances one at a
time, which would run two Simmers against one database during every deploy —
precisely the window in which quota overshoot occurs. Spec §2.2 says one instance
owns its quota state, and that stands. See `DECISIONS.md` D-007.

The container publishes no ports by default. The SMTP listener is plaintext and
accepts plaintext AUTH (spec §2.3), so it belongs on a trusted internal segment;
exposing port 25 to a host interface must be a deliberate act.
