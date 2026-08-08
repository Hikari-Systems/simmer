# CLAUDE.md — simmer

Guidance for AI assistants working on this codebase.

## What this is

An SMTP relay facade that applies a domain reputation warm-up ramp. It sits
between an application and real SMTP providers, selects an outbound route by
quota state, rewrites the message identity to match, forwards synchronously, and
maps the downstream reply back on the same connection.

`docs/SPEC.md` is **authoritative**. Read the relevant section before changing
anything, and do not amend `SPEC.md` — record divergences in `DECISIONS.md`.

## The three things that will bite you

1. **This is not an MTA.** No spool, no queue, no retry scheduler, no DSN
   generation. If you find yourself designing message persistence, stop. The only
   persistence is quota state and recipient-frequency events. The `DATA` buffer
   (§8.1) spills to tmpfs and is explicitly *not* durable.

2. **The cutover invariant (§1.1) outranks convenience.** Simmer is temporary, so
   its output must always be exactly expressible as application-side config.
   Rewrites are absolute assignments, never relative transformations, and they
   are stable under repetition. Identity fields (`envelope_from`, `From:`,
   `Sender:`, `Message-ID:`) must be stable with no override; other headers can be
   exempted only by declaring them in `unstable_headers`.

3. **Never emit a reply that makes a client record permanent state.** §14.1. A
   `550` puts a deliverable recipient on suppression lists that outlive Simmer by
   years. Chain exhaustion is `451`. When adding any new failure path, apply this
   test to it — that is how the §10.1 `5xx` mapping came to be split by stage
   (`DECISIONS.md` D-008).

Also: quota increments on downstream `2xx` only, via the §7.4 reserve/send/commit
protocol. There is no failover between routes on downstream failure — falling
through would emit under the wrong identity and corrupt the ramp.

## Build and run

```sh
docker compose up -d simmer-db   # tests/quota*.rs need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check
docker compose up -d --build     # the real gate; do not skip
```

`SIMMER_CONFIG` overrides the config path (default `simmer.yaml`).

## Key files

```
src/config/mod.rs        the §4.1 schema; every struct is deny_unknown_fields
src/config/validate.rs   §4.2 — accumulates ALL violations, never short-circuits
src/config/interpolate.rs  ${ENV_VAR}, over the parsed tree not the raw text
src/routing/sender_match.rs  §5.4 — wildcard precedence, first match wins
src/smtp/session.rs      §5.2 state machine; PIPELINING means never drop buffered input
src/smtp/reply.rs        EVERY reply Simmer can emit. Adding a 5xx here is a decision
src/smtp/buffer.rs       §8.1 — transient, tmpfs above 1 MiB. Not a spool
src/downstream/outcome.rs  §10.1 + D-008, as data. The §14.1 test lives in its tests
src/downstream/client.rs   the outbound conversation; §10.2's ambiguity is in `deliver`
src/relay.rs             decide → reserve → relay → commit/release (§7.4)
src/quota/postgres.rs    the §7.4 protocol. The row lock is what makes it correct
src/quota/day.rs         §7.2 elapsed-duration day index; NEVER calendar arithmetic
src/routing/chain.rs     §3.2 step 3 — the walk. Headroom check and reserve are ONE op
src/metrics.rs           §9.1 counters; no exporter until phase 7
src/db.rs                pool + migrations
src/admin/               §9 control plane (phase 1: GET /health only)
tests/support/mod.rs     the scripted fake downstream (§12.3)
```

## House pattern

Storage follows the hikari-systems Rust data-service pattern: runtime `sqlx`
queries (never `query_as!`, which needs a live `DATABASE_URL` at compile time and
breaks the Docker build), `models/<entity>.rs` free functions over `&PgPool`
returning `anyhow::Result`, plain-SQL migrations applied by `sqlx::migrate!` at
startup, idempotent baselines, `TIMESTAMPTZ` + `DateTime<Utc>`.

It deliberately diverges elsewhere — YAML config rather than the `config.json`
layering, axum rather than actix, JSON logging rather than
`hs_utils::logging::init`. Each divergence is a numbered entry in `DECISIONS.md`
with its reason. See the `hs-rust-data-service` skill for the unmodified pattern.

## Working agreement

Build in the phases of `SPEC.md` §13. **Write a short plan at the start of each
phase and wait for confirmation before writing code.** Summarise at the end of
each phase: what changed, what is tested, what is not, and anything the spec did
not cover. Open questions live at the bottom of `DECISIONS.md`; settle the
relevant ones at the start of the phase that needs them rather than guessing.

Test-first for the logic-heavy parts — sender matching and wildcard precedence,
day-index arithmetic, the reservation protocol, template rendering, the reply
mapping table, rewrite stability. That is where the bugs will be and they are all
cheaply testable in isolation.

Raise rather than assume: an ambiguous spec, a constraint that appears to
conflict with another, anything that would need a message persisted beyond its
client connection, or a restrictive dependency licence.
