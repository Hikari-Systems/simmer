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

Phase 1 of the ten in `docs/SPEC.md` §13: configuration loading, full startup
validation, structured logging, and the container skeleton. There is **no SMTP
listener yet** — that is phase 2.

What works today: the service loads and validates `simmer.yaml`, reports every
violation at once, applies migrations, and serves `GET /health`.

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
cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check                 # licences, advisories, sources
docker compose up -d --build     # not optional before pushing
```

CI runs all of these. Note this is *not* the house norm — the standard
hikari-systems `build.yml` builds and pushes the image with no test or lint gate.
Simmer adds one because its central correctness argument is a property test.

## Layout

```
src/config/     the §4.1 schema, ${ENV_VAR} interpolation, §4.2 validation
src/routing/    sender matching (§5.4); chain selection lands in phase 3
src/admin/      the §9 control plane; phase 1 has GET /health only
src/db.rs       pool construction and migrations
migrations/     plain SQL, applied at startup
docs/SPEC.md    the specification
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
