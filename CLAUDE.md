# CLAUDE.md — simmer

Guidance for AI assistants working on this codebase.

## What this is

An SMTP relay facade that applies a domain reputation warm-up ramp. It sits
between an application and real SMTP providers, selects an outbound route by
quota state, rewrites the message identity to match, forwards synchronously, and
maps the downstream reply back on the same connection.

`docs/SPEC.md` is **authoritative**. Read the relevant section before changing
anything, and do not amend `SPEC.md` — record divergences in `DECISIONS.md`.

## The four things that will bite you

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

   Since phase 4 this is **enforced**, not just documented: `config::validate`
   runs the real rewrite engine against a synthetic probe at startup and refuses
   to boot on a violation. It caught one in `SPEC.md`'s own §4.1 example
   configuration — see D-036 before assuming a failure is the checker's fault.
   Three corollaries when working in `src/rewrite/`: a header the route does not
   name keeps its **original bytes** (D-039); anything derived from the message
   gets escaped at the substitution boundary, never afterwards (D-038); and since
   phase 5 the same applies to the body, as **spans** — a `text/*` part no
   `body_rewrites` pattern matched is never decoded and re-encoded, so it keeps
   its original bytes too (D-043). Re-encoding is not the identity, so never
   widen what gets re-encoded.

3. **Never emit a reply that makes a client record permanent state.** §14.1. A
   `550` puts a deliverable recipient on suppression lists that outlive Simmer by
   years. Chain exhaustion is `451`. When adding any new failure path, apply this
   test to it — that is how the §10.1 `5xx` mapping came to be split by stage
   (`DECISIONS.md` D-008).

4. **A transaction carries exactly one recipient** (D-047). §5.6's
   `single_recipient_only` switch and §13's phase 9 splitting are gone, not
   deferred: collapsing several per-recipient outcomes into SMTP's single reply
   either drops mail silently or records one recipient's failure against the
   others. A second `RCPT TO` is `452`, unconditionally. `docs/RECIPIENTS.md` is
   the reasoning and it still needs the spec's author — which is also why O-8 and
   O-9 are struck through as *dissolved* rather than settled.

Also: quota increments on downstream `2xx` only, via the §7.4 reserve/send/commit
protocol — and so do §7.3's recipient-frequency events, in the same transaction.
There is no failover between routes on downstream failure — falling through would
emit under the wrong identity and corrupt the ramp. §7.3 is the opposite: over
threshold makes a route ineligible so the message *steers* to the next link, and a
chain with none left is §10.3's `451`, never a drop.

## Build and run

```sh
docker compose up -d simmer-db   # tests/quota*.rs need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
docker compose up -d --build     # the real gate; do not skip
```

**Anything that builds the shipped image must pass `--target runtime`.** The
Dockerfile's last stage is `acceptance` (it builds on `runtime`, so it has to come
after it), and an unpinned `docker build` produces the *last* stage — which is the
loadgen, not the server. `docker-compose.yml`'s `app` service pins it; a
deployment pipeline would have to as well.

`SIMMER_CONFIG` overrides the config path (default `simmer.yaml`).

The §12.3 acceptance suite runs against its own stack and is **not** in
`cargo test` — it needs Docker and a couple of minutes of container restarts:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

Run it after any change to `src/rewrite/`, the quota walk or the config schema.
It is the only tier that proves both arrangements of §1.1 produce byte-equal
output. `docs/ACCEPTANCE.md` is the topology; `DECISIONS.md` D-042 is what bites —
in particular, **every** compose invocation must carry `SIMMER_WARMUP_STARTED`, or
compose silently re-creates `app` at the default warm-up instant mid-test and the
suite measures the wrong day while appearing to pass.

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
src/rewrite/mod.rs       §6.1's order of operations. The order is not arbitrary
src/rewrite/template.rs  §6.3's variables, parsed. An unknown one is fatal (D-034)
src/rewrite/encode.rs    RFC 2047/5322 conformance. EVERY function is idempotent
src/rewrite/headers.rs   an untouched header keeps its ORIGINAL BYTES (D-039)
src/rewrite/mime.rs      §6.4's structure, as byte RANGES into the body (D-043)
src/rewrite/body.rs      §6.4 itself. No match means no re-encode — that is the point
src/rewrite/transfer.rs  quoted-printable and base64, each with a matching encoder
src/rewrite/charset.rs   four charsets, by hand. Anything else is "unknown" (D-044)
src/rewrite/stability.rs §6.6's property; `validate.rs` runs it at startup. The
                         body half is `body::Rules::fixed_point_violation` (D-046)
src/relay.rs             decide → reserve → rewrite → relay → commit/release (§7.4)
src/frequency/mod.rs     §7.3 — normalisation, the keyed hash, the rolling window
src/frequency/sweeper.rs §7.3's eviction. Hourly; not started if nothing needs it
src/quota/postgres.rs    the §7.4 protocol. The row lock is what makes it correct
                         `commit` also records §7.3's events, in ONE transaction
src/quota/day.rs         §7.2 elapsed-duration day index; NEVER calendar arithmetic
src/routing/chain.rs     §3.2 step 3 — the walk. Headroom check and reserve are ONE op
src/metrics.rs           §9.1 counters; no exporter until phase 7
src/models/recipient_event.rs  §7.3's rows. A key is 16 bytes and never plaintext
src/models/instance_config.rs  §7.3's salt: insert-if-absent, then read (D-050)
src/db.rs                pool + migrations
src/admin/               §9 control plane (phase 1: GET /health only)
src/bin/loadgen.rs       the acceptance suite's sender; NOT in the shipped image
tests/support/mod.rs     the scripted fake downstream (§12.3)
tests/rewrite_stability.rs  §6.6 as a proptest. It found two real bugs; keep it
tests/frequency.rs       §7.3 against real Postgres, and through the relay
tests/acceptance.rs      §12.3 against real mail servers; behind --ignored
simmer.acceptance.yaml   the acceptance stack's config (D-042)
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
