# CLAUDE.md — simmer

Guidance for AI assistants working on this codebase.

## What this is

An SMTP relay facade that applies a domain reputation warm-up ramp. It sits
between an application and real SMTP providers, selects an outbound route by
quota state, rewrites the message identity to match, forwards synchronously, and
maps the downstream reply back on the same connection.

`docs/SPEC.md` is **authoritative**. Read the relevant section before changing
anything, and **do not amend it on your own initiative** — record divergences in
`DECISIONS.md` and put the question to the spec's author.

It *has* been amended once, on 2026-08-11, when five such questions were answered:
§2.2, §2.3, §4.1, §4.2, §5.6, §6.3, §9.1 and §13 all carry the results, each with a
marker saying what it used to say. The rule that came out of it: **a divergence
becomes a spec amendment only when its question has been put to the author and
answered.** Everything else stays a `DECISIONS.md` entry. See "The spec
settlements (after phase 10)" for what changed and what was deliberately left
alone. The same rule produced the 2026-09-17 amendment for the link proxy
(§5.7, and the sections D-083 lists), after O-13 was put to the author, and the
2026-09-21 one for `header_rewrites` (§6.1 step 5a, §6.2, and the sections D-089
lists), after O-16 was, and the same day's for thread affinity (§3.2 step 2a,
§7.4's one exception to "overshoot is not acceptable", and the sections D-090
lists), after O-17 was. And the same day's for the partial ramp (§3.2 step 3c′,
§7.2, and the sections D-091 lists), which the author asked for directly. And the
2026-09-23 one for the end-of-data rule (§5.5, §9.1 and §10.3), after O-18 was —
see D-095.

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
   configuration, which the spec's author has since confirmed was the example's
   fault and not the rule's — the example is now corrected (D-036).

   Stability is necessary and **not sufficient** for `envelope_from`: its domain
   must additionally be a **literal** (D-069). `bounce@{{original.envelope_from.domain}}`
   is perfectly stable and still refused, because a route whose domain varies per
   message warms nothing while its quota row reads like a healthy ramp. The rule is
   about the domain only — a templated local part is §6.6's business, not this
   rule's, and widening it would ban legitimate constructions.
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

   It has exactly one carve-out, and §10.3 now spells out why: §5.5's end-of-data
   rule answers `554` on a smuggling-shaped payload (D-095). The test's literal
   form — "were Simmer removed, the client would never see this reply" — is true
   of it. What the rule protects is Simmer's own *transient* state escaping into
   systems that outlive it, and a malformed message is not transient. Apply that
   distinction, not the literal sentence, and if a new path seems to need the same
   carve-out, say so out loud rather than assuming it.

4. **A transaction carries exactly one recipient** (D-047). §5.6's
   `single_recipient_only` switch and §13's phase 9 splitting are gone, not
   deferred: collapsing several per-recipient outcomes into SMTP's single reply
   either drops mail silently or records one recipient's failure against the
   others. A second `RCPT TO` is `452`, unconditionally. Confirmed by the spec's
   author and **now what §5.6 itself says**; `docs/RECIPIENTS.md` is the long-form
   reasoning. O-8 and O-9 remain struck through as *dissolved* — D-047 removed the
   case each was about, so neither was ever answered.

Also: quota increments on downstream `2xx` only, via the §7.4 reserve/send/commit
protocol — and so do §7.3's recipient-frequency events, in the same transaction.
There is no failover between routes on downstream failure — falling through would
emit under the wrong identity and corrupt the ramp. §7.3 is the opposite: over
threshold makes a route ineligible so the message *steers* to the next link, and a
chain with none left is §10.3's `451`, never a drop.

And since phase 7, a fifth that is really the third applied to §9: **the control
plane must not lie and must not leak.** A read that reported the configured
schedule where the row says otherwise misleads an operator at the moment they are
diagnosing (D-026, and `admin/view.rs` reports both plus a `drift` flag). A gauge
only written by a relayed message reports yesterday under today's labels (D-056,
so `/metrics` recomputes them). A `/routes` or `/quota` response carrying a
recipient or a recipient key would undo §7.3's whole reason for hashing — there is
a test asserting no read endpoint emits so much as an `@`. And a mutation cannot
change the *class* of reply: a pause and a zero allowance both end at §10.3's
`451`, which is the point, but they can make every message on a chain get it, so
every mutation reports which chains it just emptied (D-057).

And a sixth, which is the third applied to §8.3: **a pooled connection must not
turn Simmer's optimisation into somebody's deferral — or into a duplicate.** A
downstream that reaped an idle connection gives EOF on the first command of the
reused conversation, and reporting that like any other failure manufactures a
`451` for a perfectly deliverable recipient. So `client::relay` retries **once**,
and the three conditions on it are each load-bearing (D-068): only on a *reused*
connection, only on a *protocol* error — not a timeout, not a rejection — and
**never at `FinalDot`**, which is §10.2's window where a retry sends the message
twice. If you widen any of the three, work out which of those two failures you
have just chosen. Relatedly, `max_connections` is a bound held for the whole
checkout rather than a socket cache (D-067): a cache satisfies three of §8.3's
four clauses and leaves the downstream unprotected, which is the clause worth
having.

And a seventh, from phase 11, which is §5.3 and §1.1 together: **the sender ACL
gates acceptance and never routing** (D-071). `smtp::acl` answers "may this user
present this identity?" and can only refuse; nothing it knows reaches
`routing::`. If a grant ever picked a chain, the outbound identity would depend on
who authenticated, which no application-side config can express. There is a test
that two users granted one identity produce byte-identical output — keep it
passing. Alongside it, the `STARTTLS` injection check in `session::starttls` is the
**one** place the pipelining rule above is overridden: bytes buffered behind
`STARTTLS` drop the connection rather than being answered. Do not "fix" it to keep
the session in step, and do not reset `auth_failures` in the handshake reset
(D-070).

And an eighth, from D-085, because it is exactly what a future reader will
misjudge: **the capture is not a spool, and the way it is not is structural.**
If you add an outcome field, a retry count, a "pending" state, or any read path
from `capture::` into `relay::`, you have built the thing §2.2 forbids. The
record is written *before* the relay precisely so it cannot know what happened —
which is also what lets `on_error: defer` answer `451` honestly, since nothing
has been relayed yet. Move the capture after the relay and that `451` becomes a
duplicate delivery on the client's retry (§10.2, D-068); that was the first
draft, and D-085 records why it was wrong. `server replay` delivers mail twice on
purpose; never point it at production. Two more things about it that are easy to get wrong. **The flush policy is a
bound on records, not on bytes** (D-088): ten buffered lines or 500 ms of quiet,
with the writer's one-second tick as the backstop for the trickle the idle rule
cannot bound. Widen any of the three and `tail -f` on the current bucket — the
first thing anyone does with this — stops keeping up. And
**`simmer_capture_disk_bytes` has two writers on purpose** (F17): the writer adds
what each flush pushed, so the gauge is live, and the retention sweeper *sets* it
from the directory, so eviction shows and the increments' drift is bounded by one
interval. Keep both. An increment-only gauge climbs and never comes down; a
sweeper-only gauge reads 0 for the first hour, which is what F17 was.

`docs/CAPTURE.md` is the long form.

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

**There are two builds, and CI checks both** (D-084). The default is Postgres.
The SQL Server build is `--no-default-features --features mssql` on every cargo
command, against a SQL Server named by `MSSQL_URL`:

```sh
docker compose --profile mssql up -d simmer-mssql-db
export MSSQL_URL='server=tcp:127.0.0.1,1434;user id=sa;password=Simmer-dev-1!;TrustServerCertificate=true'
cargo test --no-default-features --features mssql
cargo clippy --all-targets --no-default-features --features mssql -- -D warnings
cargo deny --no-default-features --features mssql check
```

A change behind `QuotaStore` goes into **both** stores, and into
`tests/store_conformance/` first. A race test there must warm its pools and
start behind a barrier: contenders that each open a fresh connection never
overlap, and the test then passes with the lock removed. That happened once
already (D-084).

**Anything that builds the shipped image must pass `--target runtime`.** The
Dockerfile's last stage is `acceptance` (it builds on `runtime`, so it has to come
after it), and an unpinned `docker build` produces the *last* stage — which is the
loadgen, not the server. `docker-compose.yml`'s `app` service pins it; a
deployment pipeline would have to as well.

`SIMMER_CONFIG` overrides the config path (default `simmer.yaml`).

**The test tiers can all run with the capture on**, and it is not the soak's:
`SIMMER_CAPTURE=on` makes `tests/compose/stack.rs` layer `test/compose/capture.yml`
onto whatever stack is running and point `app` at a generated config twin.
`test/config/Dockerfile` builds those twins by appending the single
`test/config/capture.block.yaml` to every tier config, so none of them is
committed and none can drift. A stack reading its config from the image rather
than the config volume — the acceptance tier — has no twin, and asking to capture
it fails saying so rather than running uncaptured.

The T4 soak also runs against the SQL Server build with `SOAK_BACKEND=mssql`
(`test/compose/mssql.yml`, against Express). `docs/SOAK.md` §11 is what that
found.

The `server` binary has three subcommands, each a no-op unless it is `argv[1]`
and each running before the config is read: `hash-password`, `healthcheck`, and
`replay` (D-086). `replay` is the only one that needs the runtime.

The §12.3 acceptance suite runs against its own stack and is **not** in
`cargo test` — it needs Docker and a couple of minutes of container restarts:

```sh
docker compose -f docker-compose.yml -f test/compose/acceptance.yml --profile acceptance up -d --build
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
src/smtp/session.rs      §5.2 state machine; PIPELINING means never drop buffered input —
                         except behind STARTTLS, where it means drop the connection (D-070)
src/smtp/mod.rs          §5.1 listeners: one per port, one shared session bound and CIDR check
src/smtp/tls.rs          §5.1's certificate. `load` is what §4.2 AND the listener call —
                         one function, so they cannot disagree. notAfter read by hand
src/smtp/acl.rs          §5.3's grants (D-071). Refuses; never routes
src/smtp/reply.rs        EVERY reply Simmer can emit. Adding a 5xx here is a decision
src/smtp/buffer.rs       §8.1 — transient, tmpfs above 1 MiB. Not a spool
src/downstream/outcome.rs  §10.1 + D-008, as data. The §14.1 test lives in its tests
src/downstream/client.rs   the outbound conversation; §10.2's ambiguity is in `deliver`.
                         The ONE retry lives here and is bounded by three conditions
src/downstream/pool.rs   §8.3 — max_connections is a SEMAPHORE, not a cache (D-067)
src/rewrite/mod.rs       §6.1's order of operations. The order is not arbitrary
src/rewrite/template.rs  §6.3's variables, parsed. An unknown one is fatal (D-034)
src/rewrite/encode.rs    RFC 2047/5322 conformance. EVERY function is idempotent
src/rewrite/headers.rs   an untouched header keeps its ORIGINAL BYTES (D-039)
src/rewrite/mime.rs      §6.4's structure, as byte RANGES into the body (D-043)
src/rewrite/body.rs      §6.4 itself. No match means no re-encode — that is the point
src/rewrite/header_rules.rs  §6.2's header_rewrites (D-089). Matched on
                         the RFC 2047-DECODED value; a header no rule changes keeps its bytes
src/rewrite/transfer.rs  quoted-printable and base64, each with a matching encoder
src/rewrite/charset.rs   four charsets, by hand. Anything else is "unknown" (D-044)
src/rewrite/stability.rs §6.6's property; `validate.rs` runs it at startup. The
                         body half is `body::Rules::fixed_point_violation` (D-046)
src/relay.rs             decide → reserve → rewrite → relay → commit/release (§7.4)
src/preflight/mod.rs     §6.7 — the three checks, the registry, the interval loop.
                         A route with NO report is eligible: fail open (D-064)
src/preflight/resolver.rs  the DNS leg, behind a trait. A TXT record's strings are
                         CONCATENATED — every real DKIM key arrives split
src/frequency/mod.rs     §7.3 — normalisation, the keyed hash, the rolling window
src/frequency/sweeper.rs §7.3's eviction. Hourly; not started if nothing needs it
src/quota/postgres.rs    the §7.4 protocol. The row lock is what makes it correct
                         `commit` also records §7.3's events, in ONE transaction
src/quota/day.rs         §7.2 elapsed-duration day index; NEVER calendar arithmetic
src/routing/domain_group.rs  §3.2 step 2 — literal, then MX suffix (D-100). DNS NEVER
                         defers: a failed or slow lookup is the catch-all. One shared
                         `Grouper` (Engine::groups) so walk, early check and dry run agree
src/routing/chain.rs     §3.2 step 3 — the walk. Headroom check and reserve are ONE op
src/routing/thread.rs    §3.2 step 2a (D-090) — thread affinity. A REORDERING plus two
                         exemptions for the pinned route only: no §7.3 threshold, and
                         an ordinary reservation first, then `over_cap` past the cap
src/routing/partial.rs   §3.2 step 3c′ (D-091, D-097) — `schedule.share`. A KEYED
                         HASH, not a dice roll, so dry run and every instance agree.
                         Past a list's end the share is 1, NOT a cap's repeated last
                         value. `mode: auto` computes the share from the day's row
                         and the clock instead: `auto_share` is the arithmetic and
                         has no clock or config lookup in it, `share_for_group` is
                         the ONE entry point the walk, the dry run and /routes all
                         call. `fill_by` is under 1 on purpose and `tail` is what
                         makes a ramp finish — a floor cannot
src/metrics.rs           §9.1 counters, the recorder, and every `# HELP` line. The
                         recorder exists only with `admin.metrics` (D-093): off, every
                         `metrics::` call is a no-op and /metrics is a 404
src/alloc_stats.rs       D-092 — jemalloc's counters to a file for the soak. Behind the
                         `alloc-stats` feature, which NO published image enables
src/models/recipient_event.rs  §7.3's rows. A key is 16 bytes and never plaintext
src/models/instance_config.rs  §7.3's salt: insert-if-absent, then read (D-050)
src/db/mod.rs            the backend switch: one per build, never both (D-084)
src/db/postgres.rs       sqlx pool + migrations
src/db/mssql.rs          tiberius over bb8. A connection is `broken` for the
                         whole of every call and cleared only on success
src/quota/mssql.rs       §7.4 over SQL Server. UPDATE WITH (UPDLOCK, SERIALIZABLE)
                         + IF @@ROWCOUNT = 0 INSERT is the row lock; never MERGE
migrations-mssql/        the same schema in T-SQL. BIN2 collation on every key
tests/store_conformance/ §11's contract, run against both backends
src/hash_password.rs     `server hash-password`. Reads stdin, never argv
src/capture/mod.rs       D-085 — the optional debugging capture. WRITE-ONLY: it
                         exposes no read method, so nothing in the delivery path
                         can consult it. Off unless `capture:` is configured
src/capture/record.rs    the JSONL schema. It carries NO outcome and no derived
                         state, and a test asserts the key set exactly
src/capture/writer.rs    one task behind a bounded queue. `try_send`, never
                         `send().await` — awaiting it would make a slow disk into
                         backpressure on the relay. D-088's flush policy and the
                         bytes each flush reports to the disk gauge (F17)
src/capture/bucket.rs    the 10-minute filename. A record's `at` is ALWAYS inside
                         the bucket its file names; that is what replay rests on
src/capture/replay.rs    `server replay` (D-086). A DUPLICATE-DELIVERY machine by
                         design (§10.2) that spends the target's quota a second
                         time (§7.4). `--confirm` is not optional
src/capture/client.rs    the replay's SMTP client. NOT `downstream/client.rs` — no
                         pool, no route, no D-068 retry — and NOT loadgen's: it
                         cannot express a misbehaviour mode, which is why it may
                         ship in the runtime image
src/link_proxy/mod.rs    §5.7 (D-083) — the optional HTTP link forwarder. The
                         forwarding is axum-reverse-proxy's; the listener, the
                         no-store rule and the query-free logging are ours
src/link_proxy/rewrite.rs  Location / Set-Cookie back to the public name, and the
                         path prefix undone. Exact upstream match only
src/admin/view.rs        §9.2's projections. The row wins over the schedule (D-026)
src/admin/auth.rs        §9.3's token. Reads need it too (D-055); named (D-053)
src/admin/mutate.rs      §9.3. Every mutation says which chains it just emptied
src/admin/dryrun.rs      §9.4 over the REAL engine. It reserves and counts nothing
src/bin/loadgen.rs       the acceptance suite's sender; NOT in the shipped image
tests/support/mod.rs     the scripted fake downstream (§12.3)
tests/rewrite_stability.rs  §6.6 as a proptest. It found two real bugs; keep it
tests/frequency.rs       §7.3 against real Postgres, and through the relay
tests/quota_multi_instance.rs  TWO pools, one database. Why D-007's reason was
                         wrong, and what the §7.3 race actually costs (D-061)
tests/preflight.rs       §6.7 through the walk. A strict failure STEERS; a chain
                         with none left is 451 on the wire, never a 5xx
tests/pool.rs            §8.3 counted from the DOWNSTREAM's side — accepted
                         connections and command lines, never the pool's own view
tests/ingress_tls.rs     §5.1/§5.3 end to end. Every handshake VERIFIES against a
                         per-test CA (support::TestPki); an unverified one proves little
tests/partial_ramp.rs    D-091 through the real walk; dry run agrees per recipient
tests/admin_api.rs       §9. Pins dry run against the REAL walk, step for step
tests/metrics_endpoint.rs  §9.1. Its own binary — one global recorder per process
tests/metrics_idle.rs    D-093's counter expiry. Its own binary, for the same reason
tests/link_proxy.rs      D-083 on the wire: raw client, recording upstream
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
layering, axum rather than actix, JSON logging rather than the house `logging::init`,
the pool built from a URL rather than a `DbConfig`. Each divergence is a numbered
entry in `DECISIONS.md` with its reason. See the `hs-rust-data-service` skill for
the unmodified pattern.

**No `unsafe` in this crate** (D-094): `[lints.rust] unsafe_code = "forbid"` in
`Cargo.toml` covers the library, every binary and every test. If something seems
to need `unsafe`, it belongs in a dependency or not at all — ask first.

**`hs-utils` is not a dependency** (D-060). It was one, for a single stdlib-only
function, which now lives in `src/healthcheck.rs` behaving identically. So the
pattern above is followed by *convention*, not by shared code: there are **no git
dependencies at all**, `deny.toml` has no `allow-git` entry, and adding either is
a decision rather than a Cargo.toml line. Do not reintroduce `hs-utils` to reach
for a helper — copy what is needed and say so in `DECISIONS.md`.

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
