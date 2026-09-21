# Simmer — state of the build

**Snapshot taken 2026-09-21, at release `v0.6.0`.** (The previous snapshots were
2026-09-20 at `v0.3.1`, and 2026-09-11 at the end of phase 11.) §2's full-suite
numbers are the `v0.6.0` run; what was verified for `v0.4.0` and `v0.5.0` is
stated there separately. This is a session-handover document, not a
maintained one: `README.md` describes the service, `DECISIONS.md` records why it is
the way it is, and `docs/SPEC.md` is authoritative over both. If this file
disagrees with any of them, they win.

**What has landed since the phase 11 snapshot**, none of it a §13 phase — each was
put to the spec's author and recorded rather than assumed:

| | |
|---|---|
| **D-083** — the link proxy (§5.7) | An optional HTTP forwarder, after O-13 was answered; `SPEC.md` amended 2026-09-17 |
| **D-084** — a second image | `--no-default-features --features mssql` over SQL Server, published as `:<tag>-mssql`. One backend per binary, `migrations-mssql/` in T-SQL, `tests/store_conformance/` run against both |
| **D-085 / D-086** — the capture and `server replay` | An optional debugging capture of every accepted message, and a subcommand that replays a range. A recorded divergence, not a spec amendment (O-15) |
| **D-087** — the soak tier generalised | T4 against the mssql build on SQL Server Express, and against the capture, on any tier |
| **D-088** — the capture's flush policy | Ten buffered lines or 500 ms of quiet |
| **D-089** — `header_rewrites` | A regex over one named header's decoded value, between `remove_headers` and `set_headers` (§6.1 step 5a). After O-16; `SPEC.md` amended 2026-09-21 |
| **D-090** — thread affinity | An outbound reply into a thread Simmer started leaves via the route that started it — past that route's day cap once the cap is met, counted, and past its §7.3 threshold. Keyed statelessly on the emitted `Message-ID:` domain. After O-17; `SPEC.md` amended 2026-09-21 (§3.2 step 2a, and §7.4's one exception to "overshoot is not acceptable") |
| **D-091** — the partial ramp | `warmup.schedule.share`, one value per day index: on day `i` only `share[i]` of a warming route's traffic is offered to it, the rest steering to the next link (reason `partial_ramp`), so the cap fills later in the day. A keyed hash picks which messages, so dry run and every instance agree. Past the list's end every message is offered. Asked for by the spec's author; `SPEC.md` amended 2026-09-21 (§3.2 step 3c′, §7.2) |
| Releases | `v0.2.0`, `v0.3.0` (capture + replay), `v0.3.1` (the fixes below), `v0.4.0` (`header_rewrites`), `v0.5.0` (thread affinity), `v0.6.0` (the partial ramp) |

---

## 1. Where the build has got to

`SPEC.md` §13 lists ten phases. **Nine are done and one is void**, so the original
build order is complete. Phase 11, added to §13 after them, is done too. Nothing is
scheduled after it.

| Phase | | Status |
|---|---|---|
| 1 | Config loading, full validation, structured logging, container skeleton | **done** |
| 2 | SMTP ingress, AUTH, limits, buffering, downstream forwarding, reply mapping | **done** |
| 3 | Postgres, migrations, quota model, reservation protocol, day index, chain selection, sweepers | **done** |
| 4 | Rewriting engine: templates, header set/remove, auth-artefact stripping, idempotency property test | **done** |
| 5 | Body rewriting with decode/re-encode | **done** |
| 6 | Recipient frequency: hashing, normalisation, sweeper | **done** |
| 7 | Admin API, metrics exporter, dry-run | **done** |
| 8 | DNS preflight | **done** |
| 9 | Multi-recipient splitting and result collapse | **void** — D-047 refuses multi-recipient transactions outright; `docs/RECIPIENTS.md` |
| 10 | Hardening: pooling, graceful shutdown, acceptance suite, README | **done** — §8.3's pool (D-067, D-068), §10.4's drain, the README, and D-066's auth fix. The acceptance suite landed in phase 4 (D-032, D-042); real-certificate TLS is still untested |
| 11 | Listeners on 25/465/587, inbound TLS, sender ACL | **done** — D-070 (listeners, TLS), D-071 (grants), D-072 (pre-auth limits deferred). `docs/INGRESS.md` is the design; `SPEC.md` §2, §4, §5 and §13 are amended |

### What the service actually does today

Accepts a message on any configured listener — 25, 587 and 465 by convention, each
with its own `tls` and `auth` policy — from a client inside `allowed_cidrs`,
optionally over `STARTTLS` or implicit TLS, authenticates it against argon2id
hashes, checks the sender identities against the user's grants (D-071), refuses a
second `RCPT TO` (D-047), buffers the body
(memory to 1 MiB, then an unlinked tmpfs file), matches a sender rule, resolves the
recipient's domain group, **moves a thread-affinity reply's pinned route to the
front of the chain** (D-090, when `thread_affinity` is on), walks the chain — **skipping a route whose §6.7
preflight is failing under `strict`, or whose recipient-frequency window is
full** and then reserving quota under a row lock —
**rewrites the message to the selected route's identity**, forwards to that route's
downstream over TLS, and maps the downstream's verdict back on the same
connection — committing the quota, and recording the frequency event, only on a
`2xx` at the final dot.

**Thread affinity (D-090, since `v0.5.0`).** Off by default. With
`thread_affinity: true`, the message IDs in `In-Reply-To:` and `References:` —
most recent first, at most 256 — are matched against each chain route's literal
`Message-ID:` domain, and the first match pins that route: it is walked first,
the rest follow in configured order. For the pinned route alone the §7.3
threshold is not applied (the event is still recorded) and a spent cap does not
refuse: an ordinary reservation is taken first, so a reply within the cap spends
a slot like any message, and only when that is refused is the route reserved
**past its cap** (`ReserveRequest.over_cap`) — same row lock, counted, the
ceiling untouched. Pause, strict preflight and a future start still eliminate a
pin; §3.3's no-failover is unchanged. §5.4's early `RCPT TO` decision is off
while it is on. Nothing is stored: `src/routing/thread.rs`.

§7.3 is complete: the recipient is normalised (lowercase, `+tag` stripped, dots
folded at configured providers), keyed with an HMAC under a salt persisted in
`instance_config`, and counted against a rolling window. Over threshold makes the
route **ineligible**, so the message steers to the next link and is never dropped;
an hourly sweeper evicts events past the longest configured window plus a margin.

The rewrite is §6.1's order of operations in full: authentication artefacts
stripped unconditionally (§6.5), `remove_headers` then `set_headers` with
templates rendered against the message *as it arrived*, `body_rewrites` applied to
`text/*` parts (§6.4), a `Received:` header prepended, and the envelope sender
computed. §6.6's stability property is checked at startup by running the real
engine against a synthetic probe, and again as a proptest over generated messages.

Since phase 7 it also **carries a control plane** (§9): a Prometheus exporter on
`GET /metrics`, a read API over `/routes`, `/routes/{name}` and `/quota`, a write
API for pause, resume, graduate, allowance override and quota reset, and `POST
/dryrun`. `/health`, `/healthcheck` and `/metrics` are open; everything else
needs a bearer token, including the reads (D-055). Admin tokens can be named, so
§9.3's audit line names somebody (D-053, settling O-11).

Three things about it that are not obvious from §9:

- **The read API reports the schedule and the row, and flags the difference.**
  `quota_usage.allowance` is authoritative once written (D-026), so reporting
  only the configured schedule would mislead an operator at exactly the moment
  they are working out why mail is deferring.
- **The §7 gauges are recomputed from storage on every scrape** (D-056), because
  otherwise they are only ever set by a message that relayed and an idle route
  exports yesterday's ceiling under today's label set.
- **Every mutation reports which chains it has left with nothing eligible**
  (D-057). An allowance of zero and a chain-wide pause are both allowed — the
  reply stays §10.3's `451` either way, which is §14.1 satisfied — but they are
  the two things that can make every message on a chain fail without anyone
  meaning it.

**Since phase 10 it pools its downstream connections** (§8.3). Each route holds up
to `max_connections`, and that is a **bound** rather than a hint — it is held for
the whole time a connection is in use, so the pool protects the downstream from a
burst as much as it saves a handshake. A session that cannot get one within the
route's connect budget is answered `451 4.4.5` as its own error class (D-067).
`RSET` goes out on the way *back* to the pool, so nothing idle is ever
mid-transaction; a connection idle beyond five seconds is validated with `NOOP` on
the way out; and a connection that turns out to be dead anyway costs one reconnect
and not the message — retried once, never past the terminating dot, which is
§10.2's window (D-068). §10.4 drains it, after the grace period and the
reservation release.

**§6 is now complete.** Since phase 8 a route with a `preflight` block has its
outbound identity's domain checked for SPF, DKIM and DMARC at startup and every
fifteen minutes. A failure is a `WARN` and a `0` gauge and nothing else; only
`strict: true` makes the route ineligible, and then the message *steers* to the
next link exactly as §7.3 does. A route with no verdict yet is eligible — fail
open, so a slow resolver at boot cannot empty a chain (D-064). Since D-069 a route
with no constant domain is refused at startup rather than quietly left unchecked,
so preflight's own handling of that case is now defence rather than a live path.

**Since phase 11 it has listeners, inbound TLS and a sender ACL** (§5.1, §5.3).
`server.listeners` replaced `server.listen`; an entry naming only an address takes
its port's RFC defaults (25 `off`/`optional`, 587 `starttls_required`/`required`,
465 `implicit`/`required`). One PEM certificate is loaded at startup by the same
function §4.2 validation calls. `allow_insecure_auth` now means what it says and
defaults false: AUTH over plaintext is `538 5.7.11` and not advertised. Each user
carries `grants.send_as`; for an authenticated session the envelope sender at
`MAIL FROM` and the `From:` at the final dot must both fall inside it, or the
message is `550 5.7.1`. The ACL never routes — two users granted one identity send
byte-identical mail — and does not apply to unauthenticated sessions on an
`optional` listener, which startup warns about. `server hash-password` mints the
hashes.

Body rewriting decodes each `text/*` part's transfer encoding and charset, applies
the route's patterns in order, and writes the part back in the encoding and charset
it arrived with — never changing either (D-045). A part nothing matched is not
re-encoded at all, so a body with no match is forwarded as the bytes it arrived as
(D-043).

---

## 2. Verification status

Re-run on 2026-09-20 against the `v0.3.1` tree. **Both feature sets, because CI
gates both** (D-084):

```
cargo test                                                  1068 passed, 0 failed
cargo test --no-default-features --features mssql            937 passed, 0 failed
cargo clippy --all-targets -- -D warnings                    clean
cargo clippy --all-targets --no-default-features --features mssql   clean
cargo fmt --all -- --check                                   clean
docker compose up -d --build                                 both containers healthy
```

The mssql suite needs a SQL Server and `MSSQL_URL` naming a login that may
`CREATE DATABASE`; `cargo deny` is not installed in the current sandbox and was
not re-run (CI runs it).

For reference, the phase 11 snapshot read 830 passed on the default build alone.

**For `v0.6.0` (D-091) the full suite was re-run in the development jail on both
builds:** **1188 passed, 0 failed** on the default
build against Postgres, and **1037 passed, 0 failed** on the mssql build against
SQL Server 2022, with `cargo clippy --all-targets -D warnings` and `cargo deny
check` clean on both and `cargo fmt --check` clean. New: `src/routing/partial.rs`'s
5 unit tests, `tests/partial_ramp.rs` (6, through the real walk and dry run), and
the `schedule.share` cases in `tests/config_validation.rs`. A live run of the
fixed-share first draft on the acceptance stack (share 0.5) steered 38 of 67
messages, and `/metrics` and `/routes` agreed with the logged walks. **Not run:**
the acceptance tier (it cannot run from the jail), which has no partial-ramp case
anyway.

For `v0.5.0` (D-090), what *was* run, against Postgres on the default build:
`src/routing/thread.rs`'s 24 unit tests, `tests/thread_affinity.rs` (14),
`tests/admin_api.rs` (55), the new `tests/config_validation.rs` and
`tests/shipped_config.rs` cases, and the Postgres run of `tests/store_conformance/`'s
four new over-cap cases — all passing, with `cargo clippy --all-targets -D
warnings` and `cargo fmt --check` clean and the mssql build compiling. Two
mutations were checked by hand: disabling the pin fails the four tests that
depend on it, and reserving past the cap before the cap is met fails the two
ordering tests. **Not run locally:** the full default suite, the mssql suite
(including the new conformance cases on SQL Server), and the acceptance tier. CI
runs the first two.

**T4, the soak tier, has now run against the SQL Server build** — an hour on
Express with the capture on, 36,010 messages an instance with nothing deferred or
refused. `docs/SOAK.md` §11 is the run, what it found, and what it did not
establish.

Plus the acceptance tier, which needs its own stack and is not in `cargo test`:

```
docker compose -f docker-compose.yml -f test/compose/acceptance.yml --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1     5 passed, 0 failed
```

| Suite | Tests | What it covers |
|---|---:|---|
| `src/` unit tests | 509 | Everything logic-heavy, in place |
| `tests/admin_api.rs` | 55 | §9 against the real router and real Postgres |
| `tests/thread_affinity.rs` | 14 | D-090 end to end: pinning, past the cap only once it is met, counted, the race, pause, frequency, no failover (`v0.5.0`) |
| `tests/smtp_ingress.rs` | 44 | §5 ingress end to end |
| `tests/ingress_tls.rs` | 20 | §5.1 and §5.3: STARTTLS, implicit TLS, per-listener AUTH, the ACL. Every handshake verified |
| `tests/config_validation.rs` | 65 | §4.2, one test per rule |
| `tests/quota.rs` | 30 | §7 against real Postgres |
| `tests/preflight.rs` | 13 | §6.7 through the chain walk and onto the wire |
| `tests/pool.rs` | 11 | §8.3 from the downstream's side: connections, not intentions |
| `tests/quota_multi_instance.rs` | 5 | Two independent pools, one database (D-061) |
| `tests/relay_mapping.rs` | 28 | §10.1 against a scripted downstream, plus §6 through the relay |
| `tests/frequency.rs` | 21 | §7.3 against real Postgres, and through the relay |
| `tests/rewrite_stability.rs` | 11 | §6.6 as a proptest over generated messages, bodies included |
| `tests/metrics_endpoint.rs` | 11 | §9.1 against a real recorder — its own binary, deliberately |
| `tests/quota_relay.rs` | 8 | §7.4 through the whole stack |
| `tests/shipped_config.rs` | 6 | `simmer.yaml` round-trips, and is ready for `thread_affinity` |
| `tests/acceptance.rs` | 1 + 5 | Config drift guard; the rest behind `--ignored` |

`tests/metrics_endpoint.rs` is a separate binary because `metrics` permits
exactly one global recorder per process, and every test in it takes a mutex
first: a gauge is keyed only by its labels, so two tests scraping in parallel
read each other's series even though each has its own database. Every other suite
builds its admin state with `metrics: None` and exercises the no-op recorder
phases 2–6 ran against.

### The acceptance tier

`docs/ACCEPTANCE.md`, built in phase 4 rather than phase 10 (D-042). Two Mailpit
traps, a loadgen container and `simmer.acceptance.yaml`, driven by a host test
that walks the ramp by moving `warmup.started` and re-creating the container. It
proves five things nothing else can: the ramp carrying exactly its allowance
across three simulated days, the excess reaching a *different provider*, the
rewrite as a real mail server receives it — headers and, since phase 5, the body
link — **both arrangements of §1.1 producing byte-equal output**, and, since phase
11, a submission over 587 with `STARTTLS` whose certificate the loadgen *verifies*
against a CA the `tls-init` service mints per run, arriving recorded as `ESMTPSA`
— with a plaintext AUTH on 587 refused `530`.

### The container gate

`docker compose up -d --build` is not ceremony. It is the only thing that
exercises the privileged port-25 bind, the tmpfs the §8.1 buffer spills onto, and
the platform root store §8.2's `required_verify` needs. A full conversation was
driven through the running container on 2026-08-07: banner, `EHLO`, `AUTH PLAIN`,
envelope, `DATA`, and a `451 4.4.1 downstream unavailable` — with the quota row
showing `day_index 6`, `domain_group google`, `allowance 2000` (the google
override series at index 6) and `reserved` back to zero after the release.

Re-driven on 2026-08-10 for D-047 and §7.3: the second `RCPT TO` of a transaction
is answered `452 4.5.3 multiple recipients not permitted` by the shipped image,
the `recipient_event` table and its two indexes exist after migration, and
`instance_config` holds one row — the base64 salt, logged as
`recipient-frequency salt loaded`, with the sweeper reporting
`retention_secs 95040`.

**And once more for D-060**, which is the change with the most to lose from a
build that only *looks* right: the container's `HEALTHCHECK` is now simmer's own
code rather than `hs-utils`'s. The container comes up `healthy` — which is Docker
running it — and every documented invocation was driven inside the image:
`healthcheck` (0), `healthcheck deps` (0), `healthcheck localhost 8080` (0),
`healthcheck --deps localhost 8080` (0), and `healthcheck localhost 9999` (1).

**And once more for phase 10.** `/routes` reports a `pool` object for both routes
rather than `null`, `/metrics` carries `simmer_pool_connections` for both states of
both routes at zero in a process that has relayed nothing, and a full conversation
through the image ends at `451 4.4.1 downstream unavailable` with the pool
correctly reporting nothing opened — a connect failure never counts a connection.
The acceptance stack is the real evidence: after its four tests, the warming route
had delivered **five messages over one connection** (`opened: 1, reused: 4`) to a
real Mailpit server, with no discards and no retries. And a real `SIGTERM` produced
§10.4's four clauses in order — `all sessions drained`, sweepers stopped,
`drained pooled connections route=warming-newbrand connections=1`,
`shutdown complete`.

**And re-driven again for phase 7, which is where it earned its keep.** `/health`
reports the database up, `/routes` without a token is `401`, `/metrics` renders
without one, a pause and a resume round-trip with the audit line
`admin mutation applied … actor=default route=warming-newbrand`, and
`simmer_route_paused` tracks both. The dry run against the *shipped* `simmer.yaml`
is what found the `From:` display-name bug (D-059): every test in
`tests/admin_api.rs` used a fixture matching on the envelope, so nothing in
`cargo test` could have caught it. After the fix, the same request matches
`senders[1] (oldbrand.com)`, selects `warming-newbrand`, rewrites the envelope to
`bounce@newbrand.com` and the `From:` to `Jane <sales@newbrand.com>` — display
name preserved — and reports the body pattern matching once.

**And re-driven for phase 11**, dev stack and acceptance stack both. The dev image
logs `SMTP listener bound addr=0.0.0.0:25 tls=off auth=required`; through it, an
envelope outside the grant is `550 5.7.1 sender not permitted`, a `From:` outside
it is the same at the final dot, `STARTTLS` on the plaintext port is `502`, and
`simmer_sender_not_permitted_total` counts both refusals by stage. `hash-password`
in the image mints `$argon2id$v=19$m=19456,t=2,p=1$…` from stdin, exits `2` on an
argument and `1` on an empty password. The acceptance stack binds 25 and 587, logs
the certificate's not-after date, and drains both listeners on a real `SIGTERM`.
`tls-init`'s leaf verifies against its CA with `openssl verify`, and the key lands
`0600` owned by UID 1000.

### Running the tests

```sh
docker compose up -d simmer-db
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer
cargo test
```

`DATABASE_URL` is needed only by `tests/quota*.rs` and `tests/frequency.rs`, which
use `#[sqlx::test]` for a fresh database per test. Nothing about the Docker build depends on it — the
queries are checked at runtime, never by `query_as!` (D-031).

---

## 3. Repository layout

```
src/config/          §4.1 schema, ${ENV_VAR} interpolation, §4.2 validation
src/smtp/            §5 ingress
  command.rs           the grammar, as a pure function
  reply.rs             EVERY reply Simmer can emit, in one file
  auth.rs              §5.3 PLAIN/LOGIN over argon2id
  acl.rs               §5.3's grants (D-071). Refuses; never routes
  tls.rs               §5.1's certificate, loaded once; notAfter read by hand
  buffer.rs            §8.1 transient buffer, dot transparency
  session.rs           the §5.2 state machine, STARTTLS and its reset
  mod.rs               listeners, their Policy, CIDR check, session cap, shutdown
src/downstream/      §8 outbound leg
  stream.rs            the four TLS modes over rustls
  client.rs            the SMTP conversation, per-stage timeouts, D-068's retry
  pool.rs              §8.3 — max_connections is a semaphore, not a cache (D-067)
  outcome.rs           §10.1 + D-008, as data
src/frequency/       §7.3 recipient frequency
  mod.rs               normalisation, the keyed hash, the rolling window
  sweeper.rs           hourly eviction past the longest window plus a margin
src/quota/           §7
  day.rs               §7.2 elapsed-duration day index
  store.rs             §11's storage trait
  postgres.rs          the §7.4 three-phase protocol
  mssql.rs             the same over SQL Server: UPDLOCK/SERIALIZABLE, never MERGE (D-084)
  mod.rs               allowance resolution, reservation expiry
  registry.rs          §10.4 in-flight reservations
  sweeper.rs           §7.4 expiry release
src/db/              the backend switch — exactly one per binary (D-084)
  mod.rs               a compile_error! if both features or neither
  postgres.rs          sqlx pool + migrations
  mssql.rs             tiberius over bb8; the migration lock covers its own DDL
src/capture/         D-085's optional debugging capture. WRITE-ONLY by design
  mod.rs               the handle and the bounded queue; off unless configured
  record.rs            the JSONL schema: no outcome, no derived state
  writer.rs            one task, one file; D-088's flush policy; the disk gauge (F17)
  bucket.rs            the ten-minute filename a record's `at` is always inside
  sweeper.rs           retention; its pass is also the disk gauge's resync
  replay.rs            `server replay` (D-086) — a duplicate-delivery machine
  client.rs            the replay's SMTP client: no pool, no route, no misbehaviour
src/link_proxy/      §5.7's optional HTTP forwarder (D-083)
  mod.rs               the listener, no-store, query-free logging
  rewrite.rs           Location / Set-Cookie back to the public name
src/rewrite/         §6 the rewriting engine
  template.rs          §6.3's variable table as a parsed grammar
  encode.rs            sanitising, RFC 2047, phrase quoting, folding
  headers.rs           the header block, edited without disturbing the rest (D-039)
  mime.rs              §6.4's structure as byte ranges into the body (D-043)
  body.rs              §6.4 itself; a part that matched nothing is not re-encoded
  transfer.rs          quoted-printable and base64, each with a matching encoder
  charset.rs           UTF-8, US-ASCII, ISO-8859-1, Windows-1252 (D-044)
  mod.rs               §6.1's order of operations
  stability.rs         §6.6's property, against a synthetic probe
src/preflight/       §6.7 the DNS preflight
  mod.rs               the three checks, the registry, the interval loop
  resolver.rs          the DNS leg behind a trait; TXT strings concatenated
src/models/          runtime sqlx over &PgPool, house pattern (Postgres build only)
  quota.rs             the §7.4 statements
  route_state.rs       §9.3 admin state
  instance_config.rs   §7.3's salt, get-or-insert (D-050)
  recipient_event.rs   §7.3's events: count, record, evict
src/routing/         §5.4 sender match, §3.2.2 domain group, §3.2.3 chain walk
  thread.rs            §3.2 step 2a's thread affinity: IDs in, a pinned route out (D-090)
src/relay.rs         decide -> reserve -> rewrite -> relay -> commit/release
src/healthcheck.rs   the `healthcheck` subcommand. Stdlib only (D-060)
src/hash_password.rs `server hash-password`: argon2id from stdin
src/metrics.rs       §9.1 counters, and the Prometheus recorder
src/admin/           §9 control plane
  view.rs              §9.2's projections, pure. D-026's drift flag lives here
  auth.rs              §9.3's bearer token, constant-time, as an extractor
  error.rs             one error shape; a storage failure is 503, not 500
  mutate.rs            the four mutations, the audit line, §14.1's warnings
  dryrun.rs            §9.4, over the real engine
  mod.rs               the router, /health, /metrics, the §9.2 reads
src/bin/loadgen.rs   the acceptance suite's bulk sender; not in the shipped image
tests/support/       the scripted fake downstream (§12.3)
tests/admin_api.rs   §9 against the real router and real Postgres
tests/thread_affinity.rs  D-090 end to end, including replies past the cap
tests/quota_multi_instance.rs  two independent pools against one database (D-061)
tests/preflight.rs   §6.7 through the walk, and 451 on the wire
tests/metrics_endpoint.rs  §9.1 against a real recorder; its own binary
tests/ingress_tls.rs §5.1/§5.3 against the real listener, verified handshakes
tests/capture.rs     D-085 through a real session: off by default, no outcome
                     field, `continue` cannot stop mail, exact recorded bytes
tests/capture_replay.rs  capture and replay end to end
tests/soak.rs        T4 — `soak_run` drives, `soak_analyze` judges the files
tests/store_mssql.rs §11's contract against a real SQL Server, a fresh database
                     per test
tests/harness_selftest.rs  the tier machinery checked against planted defects
migrations/          three: baseline (instance_config), quota (three tables),
                     recipient_event (D-048)
migrations-mssql/    the same schema in T-SQL, BIN2 collation on every key (D-084)
test/compose/        the tiers' compose overlays: acceptance, matrix, stress,
                     mssql (Express), capture. Layered, never assembled by hand
test/config/         the tiers' configs, plus capture.block.yaml — appended to
                     each of them at image build time to make the capture twins
simmer.acceptance.yaml   the acceptance stack's config (D-042)
```

Roughly 27,000 lines including tests and comments at the phase 11 snapshot; more
now, with `src/db/`, `src/capture/`, `src/link_proxy/` and the soak tier.

---

## 4. Schema

```
instance_config(key, value, …)                        phase 1
quota_usage(route, domain_group, day_index,           phase 3
            allowance, allowance_override,
            committed, reserved, …)                   PK on the first three
quota_reservation(id, route, domain_group,            phase 3
                  day_index, count,
                  correlation_id, expires_at, …)      indexed on expires_at
route_state(route, paused, graduated, …)              phase 3
recipient_event(recipient_hash, route, sent_at)       phase 6 (D-048)
                                                      no PK; two indexes
```

Two things about `quota_usage` that are not obvious:

- **`allowance IS NULL` means no ceiling** — an overflow route, which §3.1 says is
  never quota-limited but which still accounts, so that "how much is spilling to
  overflow" is answerable (D-024).
- **`allowance_override` is where §9.3's per-group override lives**, not on
  `route_state` as §11 suggests. Because the row is keyed by `day_index`, the
  override expires at the day boundary by construction (D-025).

And two about `recipient_event`:

- **`recipient_hash` is `BYTEA`, 16 bytes** — HMAC-SHA256 under the salt in
  `instance_config`, truncated. §7.3 asks the key to bound row size, and this is
  §11's high-cardinality table (D-048).
- **`route` is part of the key and part of the lookup index.** The constraint is
  per route, so a send via overflow does not count against the warming route's
  window (D-051).

### Which index serves which query

Identical definitions on both backends — verified from `pg_indexes` and
`sys.index_columns`, same columns in the same order — and verified against the
Postgres planner at 480k `recipient_event` rows rather than read off the DDL.

| Query | Index | Plan |
|---|---|---|
| §7.4's read, reserve, release and commit on `quota_usage` | PK `(route, domain_group, day_index)` | exact key; `LockRows → Index Scan` for the `FOR UPDATE` read |
| §7.3's window count — `recipient_hash = ? AND route = ? AND sent_at >= ?` | `recipient_event_lookup_idx (recipient_hash, route, sent_at)` | **`Index Only Scan`** — fully covering |
| reservation insert, and its delete on commit or release | PK `id` | exact key |
| the day-rollover reconcile's `SUM(count)` over reservations | `quota_reservation_route_idx (route, domain_group, day_index)` | `Index Scan`, all three columns as Index Cond |
| §7.4's expiry sweep — `expires_at < now()` | `quota_reservation_expires_at_idx (expires_at)` | `Index Scan` |
| §7.3's eviction — `sent_at < ?` | `recipient_event_sent_at_idx (sent_at)` | `Bitmap Index Scan` at realistic selectivity |

**No query on any path is unindexed.** What is worth knowing instead:

- **That last eviction flips to a `Seq Scan` when the predicate matches most of the
  table** — correct planning, but it means a sweeper that has been off, or a
  retention shortened suddenly, table-scans its first catch-up pass.
- **On SQL Server `quota_reservation` is CLUSTERED on a random
  `UNIQUEIDENTIFIER`**, one insert per message landing on a random page: page
  splits and fragmentation, with no Postgres equivalent because its PK is a plain
  btree over an append-only heap. Rows are short-lived, so this is churn rather
  than growth. A nonclustered PK with the cluster on `expires_at` — also the
  sweeper's own predicate, and roughly monotonic — is the fix if volume grows.
- **On SQL Server `recipient_event` is a HEAP** (two nonclustered indexes, no
  clustered index), and it is the table that actually grows: one row per commit,
  ~864k a day at 10 msg/s. A clustered index on `sent_at` would make the eviction a
  clustered range delete and keep the table ordered by the predicate that prunes it.
- **On Postgres the answer at volume is partitioning, not another index.** The
  table is a rolling window pruned hourly; declarative partitioning by `sent_at`
  turns eviction into `DROP PARTITION` — O(1) rather than O(rows), no bloat and no
  vacuum debt. SQL Server gets the same via partition switching.

None of this has been measured under load: the §11 hour moved ~72k rows, which is
megabytes. These bite in the tens of millions, and a schema change should follow a
measurement rather than this table.

---

## 5. Decisions on record

90 entries, `D-001` to `D-090`. The ones a new reader most needs:

| | |
|---|---|
| **D-008** | §10.1's `5xx` mapping is split by stage: `550` only at `RCPT TO`, `451` everywhere else. Catches the §6.5 provisioning risk without suppressing deliverable recipients. |
| **D-018** | `SMTPUTF8` is advertised only when every reachable route declares `downstream.smtputf8: true`. Diverges from §5.2's "advertises exactly". |
| **D-019** | Route selection and reservation are separated (O-1). No connection pool until phase 10. |
| **D-024** | Overflow routes account fully, with a synthetic Unix-epoch day origin. |
| **D-025** | The allowance override is a column on `quota_usage`, not a `route_state` field. |
| **D-026** | `quota_usage.allowance` is authoritative once written — a config change applies from the next day boundary, not retroactively. |
| **D-031** | Database tests use `#[sqlx::test]`, so `cargo test` needs a Postgres. |
| **D-084** | Two builds and two images, one storage backend compiled into each. A change behind `QuotaStore` goes into both stores and into `tests/store_conformance/` first. |
| **D-085** | The capture is not a spool, and structurally cannot become one: it is write-only, carries no outcome, and is written *before* the relay — which is what lets `on_error: defer` answer `451` honestly. |
| **D-086** | `server replay` delivers mail twice on purpose and spends the target's quota. `--confirm` and `--host` have no defaults. |
| **D-087** | The backend is a property of a *stack*; the capture is a property of a *run*. Hence `SOAK_BACKEND=mssql` selects a stack and `SIMMER_CAPTURE=on` is layered onto any of them. |
| **D-088** | The capture flushes on ten lines or 500 ms of quiet, and each flush reports its bytes to the disk gauge. |
| **D-089** | `header_rewrites`: a regex over a named header's decoded value. Never on an identity field; stable or fatal. |
| **D-090** | **Thread affinity.** A reply pins the route whose `Message-ID:` domain it refers to. The pinned route alone skips §7.3's threshold and, once its cap is met, is reserved **past** it — counted, the ceiling unchanged. The only exception to §7.4's "overshoot is not acceptable". |
| **D-034** | An unknown template variable is a fatal startup error — a §4.2 rule the spec does not list. |
| **D-035** | The null sender is never rewritten, so a bounce stays a bounce. |
| **D-036** | **`SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2.** A finding for the spec's author; `simmer.yaml` was corrected instead. |
| **D-037** | `Received:` is the only header the engine adds unbidden. No `X-Simmer-*` unless configured. |
| **D-039** | The header block is edited in place and the body is opaque — no MIME round trip, so untouched bytes stay untouched. |
| **D-040** | A stale `unstable_headers` declaration is a `WARN`, not a violation. Loosens phase 1. |
| **D-043** | §6.4 works in byte *ranges*: a `text/*` part nothing matched is never re-encoded, so it keeps its original bytes. This is what is left of D-039 for the body. |
| **D-045** | A part's transfer encoding and charset are never changed. A rewrite that would need one does not happen — it is skipped and counted. |
| **D-046** | Unstable `body_rewrites` are a fatal startup error with no override. §6.6's two field classes do not cover the body; this adds a third at the identity fields' severity. |
| **D-047** | **Multi-recipient transactions are refused outright.** `single_recipient_only` is deleted, §13 phase 9 is void, and §5.6's collapse table is never built. `docs/RECIPIENTS.md` is the long form; it needs the spec's author. |
| **D-048** | `recipient_event`'s row shape, which D-029 deferred to phase 6: a 16-byte HMAC key, `route` in the lookup index, no primary key. |
| **D-049** | The §7.3 check reads *outside* the reservation transaction. A race over-delivers by one on a route under its quota; holding the ramp's row lock across a high-cardinality read would cost more. |
| **D-051** | Frequency is counted **per route**, so a message that fell through to overflow does not count against the warming route's window. The other reading needs the spec. |
| **D-053** | Admin tokens can be **named** — `admin.tokens` alongside `auth_token`, which is the token named `default`. Settles O-11 and is what makes §9.3's audit line mean anything. |
| **D-054** | `simmer_messages_total` gained the `domain_group` §9.1 specifies. The one call site D-021 predicted would not move. |
| **D-055** | §9.2's **reads need the bearer token** too, which §9.3's wording does not require. `/metrics` and `/health` stay open. |
| **D-056** | The §7 gauges are recomputed from storage on every scrape, or an idle route exports yesterday's ceiling under today's label set. |
| **D-057** | An allowance of zero, and a chain-wide pause, are **allowed and warned about** rather than refused. The reply stays `451` either way. |
| **D-058** | `POST /quota/reset` recomputes `reserved` from the live reservations rather than zeroing it — an in-flight send still owns its headroom. |
| **D-059** | §9.4's dry run runs the real engine and the real chain order, and increments no counter. `tests/admin_api.rs` pins it against `walk_and_reserve`. |
| **D-060** | `hs-utils` is not a dependency. There are no git dependencies at all, and adding one is a decision rather than a Cargo.toml line. |
| **D-063** | §6.7's 15-minute interval is a constant, not config: §4.1 defines no key for it. |
| **D-064** | A route whose identity domain is not a literal is **not preflighted** — a startup `WARN` that says outright that `strict` has no effect there, never a startup error. |
| **D-065** | Preflight is evaluated **above** §7.3 in the walk: an in-memory read before a high-cardinality database one. Displaces §3.2 3b's "first" by a position. |
| **D-069** | A route's `envelope_from` **domain must be a literal**. Stability is necessary and not sufficient: a route whose domain varies per message warms nothing while its quota row reads like a healthy ramp. |
| **D-066** | The unknown-user decoy hash borrows the ACL's costliest argon2 parameters. Closes the timing oracle a fixed decoy only appeared to close. |
| **D-067** | `max_connections` is a **semaphore**, not a socket cache — §8.3's last sentence is the clause that decides the shape. An exhausted pool is `451 4.4.5`, its own class. |
| **D-068** | A dead pooled connection is retried **once**, only on a reused connection, only on a protocol error, and **never past the final dot** — which is §10.2's window. |
| **D-070** | **Listeners, inbound TLS, and the `allow_insecure_auth` inversion.** Per-port policy with RFC defaults; one certificate loaded by the function §4.2 also calls; `530` before a required handshake, `538` for plaintext AUTH; plaintext behind `STARTTLS` drops the connection; sessions end with `close_notify`. |
| **D-071** | **The sender ACL gates acceptance, never routing.** `grants.send_as` in §5.4's grammar, required, default deny; envelope at `MAIL FROM`, `From:` at the dot; unauthenticated sessions exempt, and warned about. |
| **D-072** | Pre-authentication limits are **not built**. Nothing forces them while `allowed_cidrs` is tight. |
| **D-061** | **Quota *is* safe across instances**, by the row lock — D-007's stated reason was wrong from phase 3 onward. The real constraints are config skew (D-026) and §7.3's race, whose bound is `threshold + (C - 1)`, not one message. |

**All twelve original open questions are now closed.** O-8 and O-9 were
*dissolved* rather than answered — D-047 removed the case each was about — and
O-11 was settled in phase 7 by D-053.

---

## 6. Known gaps

Not bugs — scope that has not been reached, or coverage deliberately deferred.

**Functional**

- ~~No connection pool~~ — built in phase 10. `max_connections` is a bound rather
  than a hint, and an exhausted pool is `451 4.4.5` (D-067).
- ~~No DNS preflight~~ — built in phase 8. `/routes` still reports
  `"preflight": null` for a route that is not checked, which is now a statement
  about the route rather than the phase (D-064).
- **No scopes on admin tokens.** Every token can do everything; a token that
  could read but not mutate is a plausible next ask and is not built.
- **No pre-authentication limits** (D-072). A client that never authenticates is
  bounded by `max_concurrent_sessions` and the timeouts, with no tighter budget of
  its own.
- **The ACL does not cover unauthenticated sessions** on an `auth: optional`
  listener — by design (D-071), and warned about at startup.
- **Multi-recipient transactions are refused**, by policy rather than by
  omission — D-047, and `docs/RECIPIENTS.md`.
- **Thread affinity recognises only Simmer's own emitted IDs** (D-090). An
  application that threads from its own sent log, rather than carrying
  `References:` forward from the inbound reply, is never pinned. The table of
  original IDs that would cover it was considered and rejected.

**Test coverage**

- **No test resolves real DNS.** Every preflight test drives
  `preflight::resolver::Fake`, which is the right call for the record-parsing
  logic — a test depending on somebody else's zone file is slow and flaky — but it
  means `resolver::Hickory` itself, the one piece that talks to a network, is
  exercised only by `docker compose up`. That gate does exercise it, against the
  shipped config's placeholder `newbrand.com`: three checks, three `WARN`s, three
  `0` gauges, container healthy.
- **The preflight interval loop is not tested** — `check_once` is, `run` is not.
  Exactly the gap the quota sweeper's `run` has; the §7.3 sweeper's loop *is*
  covered, so these two are now the odd ones out together.
- **No real-certificate TLS test on the *outbound* leg.** `required_verify` is
  asserted only through its failure modes; the acceptance traps are plaintext.
  The **inbound** half closed in phase 11: `tests/ingress_tls.rs` verifies every
  handshake against a per-test CA, and the acceptance loadgen verifies the
  container's certificate. `docs/ACCEPTANCE.md` §5's trap-side design still
  stands for the outbound half, and `tls-init` now provides the CA it needs.
- **The STARTTLS and implicit-TLS handshake timeouts are not driven.** Both are
  bounded by `timeouts.command`; the failure paths driven are a bad certificate
  and plaintext on the implicit port, not a peer that stalls mid-handshake.
- **The injection test depends on one write arriving as one read.** It sends
  `STARTTLS` and a `MAIL FROM` in a single write on loopback, which the kernel
  delivers together in practice; were it ever split, the server would correctly
  answer `220` and the test would fail rather than pass wrongly.
- ~~**No clock movement.**~~ Closed: the acceptance suite walks three simulated
  days by moving `warmup.started` and re-creating the container.
- ~~**No test proves a message reaches a real mail server under the rewritten
  identity**, or that the two arrangements of §1.1 produce byte-equivalent
  output.~~ Closed: both are acceptance tests, and the second is the suite's
  centrepiece.
- **No rewrite of a message above the §8.1 spill threshold.** The spill test
  covers the buffer, not a rewritten 25 MiB message. Sharper since phase 5: when
  any part matches, §6.4 copies the whole body, so a 25 MiB message with one
  matching part allocates 25 MiB and nothing measures it.
- **No message in any tier carries a non-UTF-8 charset.** ISO-8859-1 and
  Windows-1252 are unit-tested byte by byte and through `body.rs`, but no relay or
  acceptance test sends one.
- **No test observes `simmer_body_rewrite_skipped_total` *from the relay*.** The
  `SkipReason` is asserted at the `body.rs` boundary, and phase 7's
  `tests/metrics_endpoint.rs` asserts the counter reaches a scrape — but by
  calling `metrics::body_rewrite_skipped` directly, not by relaying a message
  with a part the engine will not touch.
- **No `Received:` chain long enough to fold.** `fold_if_needed` is unit-tested;
  nothing drives it through the relay.
- **`fail_closed`** — the `451 4.3.0` reply is unit-tested, but no test drives it
  with an actually-unreachable database.
- **§10.4 under a real `SIGTERM`** — `release_by_ids`, the registry and
  `Pool::drain` are each tested directly; the wiring in `main` that orders them is
  exercised only by `docker compose kill -s SIGTERM`, which was done for phase 10
  and is recorded above.
- **The pool's bound under real concurrency.** The saturation test uses one permit
  and two sessions. Nothing drives sixteen sessions at four permits and asserts the
  count never exceeded four — the invariant is enforced by the semaphore and
  argued in `pool.rs`'s module comment rather than measured.
- **No pooled connection over TLS.** Every test in `tests/pool.rs` runs `tls: off`.
  The `Stream` is the same object either way, but nothing carries a *reused*
  connection through a completed handshake. The acceptance stack's traps are
  plaintext too, so this shares its fate with the real-certificate gap below.
- **The `NOOP` validation path where the connection passes.** `VALIDATE_AFTER` is
  five seconds, so driving it costs five seconds of wall clock per test; the
  `idle_ttl` path is driven instead, at one second.
- **The quota sweeper's interval loop** (`sweep_once` is tested, `run` is not).
  The §7.3 sweeper's loop *is* covered, so this is now the odd one out.
- **§7.3 is absent from the acceptance tier.** `simmer.acceptance.yaml` declares
  no `recipient_frequency`, so the ramp assertions measure quota alone. The
  relay-level coverage is against real Postgres and a real downstream; what is
  missing is only the real-mail-server leg.
- ~~**No concurrent-check race test for §7.3.**~~ Closed by D-061:
  `tests/quota_multi_instance.rs` demonstrates it at two concurrency levels. The
  bound D-049 assumed was wrong in a way worth knowing — it is
  `threshold + (C - 1)` for peak concurrency `C` against one recipient key, not a
  flat one extra message. Two concurrent sends is the case that costs one.
- **Thread affinity is absent from the acceptance tier.** Its relay-level
  coverage is against real Postgres and a fake downstream; no test sends a reply
  through a real mail server, and `simmer_thread_affinity_total` is asserted at a
  scrape only by calling it directly, as with the body-rewrite counter above.
- **The new over-cap conformance cases have not run on SQL Server** from the
  development jail; CI runs them.
- **No test drives the admin listener over a socket.** `tests/admin_api.rs` goes
  through `oneshot` against the router, so the bind, the graceful-shutdown wiring
  in `main` and the real TCP path are exercised only by `docker compose up`.
- **`/metrics` under a failing store** logs and renders anyway; that branch is
  reasoned about, not driven.
- **Nothing asserts the §9.3 audit line itself.** No test captures a `tracing`
  subscriber, so what is proven is that the mutation happened and what the
  response said, not what was logged.
- **No test of concurrent admin mutations.** Two operators pausing and resuming
  one route interleave as last-writer-wins, and the audit line's "previous" is
  read before the write. Correct, and unproven.

---

## 6a. Known defects

**Two fixed at `v0.3.1`, and both worth reading as patterns rather than incidents:**

- **F17 — `simmer_capture_disk_bytes` could not do its documented job.** It was
  written only by the retention sweeper, on a one-hour interval whose first pass
  runs at startup against an empty directory, so the one gauge `docs/CAPTURE.md`
  offered for "will this capture fill the volume" read 0 for the whole first hour.
  Measured over a 1-hour soak: 0 for 59.9 of the 60 minutes while 904 MB
  accumulated. The writer now adds what each flush pushes and the sweeper's pass
  still sets the count from the directory. **The check had to be fixed before the
  defect could be** — its first version read the final scrape, which on an
  hour-long run is the one sample taken after the tick, so a broken gauge XPASSed.
- **The mssql migration lock did not cover the table it creates.**
  `IF OBJECT_ID(...) IS NULL CREATE TABLE` is not atomic and ran *before*
  `sp_getapplock`, so two replicas starting together both ran the DDL and the loser
  refused to start. Pre-existing in 0.3.0. `replicas_migrating_together_both_succeed`
  was written for exactly this and only failed when the full suite ran the server
  hard enough to widen the window.

**Open, and not defects so much as measured limits:**

- **The one-hour memory gate cannot resolve a small leak.** `docs/SOAK.md` §10's
  planted-defect control: a 64-byte-per-message leak (~2.2 MiB/h) was **not**
  caught, and the gate's standard error is near 3 MiB/h. So every "no leak" verdict
  from a one-hour run means "no leak much above about 6 MiB/h". §11's hour met this
  again from the other side — `app` failed the gate at +4.28 MiB/h while its twin
  read -7.29 MiB/h on an identical stream. The limit has never been adjusted to make
  a run pass.
- **F7** — `simmer_unmatched_sender_total{domain}` is client-controlled and
  unbounded; capping it diverges from §9.1 and needs the spec's author.
- **F2's cost is understood and accepted**, fixed by D-081 and now V4's regression
  check.

`test/known-findings.json` is the machine-readable list, and the rule is that an
entry cannot outlive its defect: a check that passes while listed is an XPASS and
fails the tier, so the fixing commit must delete the entry.

### Fixed earlier

The one this section carried since phase 8 — timing-based
username enumeration in `smtp/auth.rs`, where the decoy was minted at fixed argon2
parameters while verification re-derives at whatever the *stored* hash says — was
fixed in phase 10. The decoy now takes its parameters and salt from the costliest
hash the configured ACL holds, so the unknown-user path runs the derivation a real
login runs. Four tests pin it. See `DECISIONS.md` D-066.

---

## 7. Outstanding non-code items

**Phase 11 closed the one scheduled item below**, and `SPEC.md` is amended to match
(D-070, D-071). O-16 and O-17 were answered on 2026-09-21 and the spec amended for
each (D-089, D-090). Still open with the author: O-14 and O-15, both "amend, or
stay a recorded divergence" questions with a working answer in place.

A dependency note from the same day: `chacha20 0.10.1`, reached through
`hickory-resolver`'s `rand`, was yanked upstream after 2026-08-11, so
`cargo deny check` failed on the committed phase 10 lockfile. Bumped to `0.10.2`
in its own commit, ahead of phase 11.

**Everything that was waiting on the spec's author was answered on 2026-08-11**, and
`SPEC.md` was amended for the first time. `DECISIONS.md`, "The spec settlements
(after phase 10)", is the record; the short version:

| Question | Answer | Landed as |
|---|---|---|
| §4.1's example config fails §4.2 (D-036) | The rule is right; the example was wrong | §4.1 corrected, §4.2 carries the note |
| Should a non-constant `envelope_from` domain be refused? (D-064) | Yes, refuse it | **D-069** — a new §4.2 rule, and code |
| One recipient per transaction (D-047) | Confirmed | §5.6 rewritten; phase 9 struck out |
| §2.2's ownership wording (D-061) | Reword, and record the two real constraints | §2.2 and §2.3 amended |
| Inbound TLS, ports 465/587, the sender ACL (D-033) | Yes — build it | **The next phase.** See below |

**The one open item then — built since, as phase 11:**

- **`docs/INGRESS.md` (D-033) is approved to build.** Two of its five open
  questions were settled at the same time: `server.listen` is **replaced** by
  `server.listeners` rather than kept as shorthand (nothing is deployed, and
  `deny_unknown_fields` makes a stale key fail loudly), and port 25 keeps
  `auth: optional` — the RFC-shaped default — while 465 and 587 require it. A third
  is settled by the document's own reasoning: the ACL grants `send_as` and nothing
  else, because a capability nobody uses is a capability nobody tests.
  **The four `SPEC.md` passages this reverses are deliberately not yet amended** —
  amending them before the capability exists would make the spec describe something
  that is not there. They are amended by the phase that builds them.

Carried since phase 1, none blocking:

- ~~**Decide whether to keep the cargo-deny licence gate.**~~ Closed 2026-08-11 by
  D-062: it stays, and the bar is that nothing incompatible with Apache-2.0 enters
  the graph. Already in that shape and enforced in CI; verified, including the
  `r-efi` `OR`-expression that makes `cargo deny list` print `LGPL-2.1-or-later`
  without any LGPL obligation being taken.
- ~~**`hs-utils` declares no `license` field**, and nor does any hikari-systems
  Rust service.~~ No longer simmer's problem: D-060 removed the dependency, and
  with it `deny.toml`'s `ignore-sources` exemption *and* its `allow-git` list.
  **Still worth fixing upstream** — one line in `hs-utils-rs/Cargo.toml` benefits
  every other service in the estate.
- **The five older data services call `prepare_config` before
  `apply_env_overrides`**, which is the wrong order — a `[SECRET]:` supplied via
  env is never resolved. `load_layered_value()` does it correctly. Worth a sweep.

---

## 8. Git

On `main`, with an `origin` at `git@github.com:Hikari-Systems/simmer.git`.
Committed so far:

| | |
|---|---|
| `Phase 1: configuration, validation, structured logging, container` | 74 tests |
| `Phases 2 and 3: SMTP relay and the warm-up quota model` | 323 tests |
| *(then five commits of planning and CI work — D-032, D-033)* | |
| `Phase 4: the rewriting engine, and the acceptance harness` | 452 tests + 4 acceptance |
| `Phases 5 and 6: body rewriting, one recipient, recipient frequency` | 593 tests + 4 acceptance |
| `Phase 7 and the multi-instance correction: the control plane, and D-007's reason` | 714 tests + 4 acceptance |
| `Phase 8: the DNS preflight, and the licence gate settled` | 747 tests + 4 acceptance |
| `Phase 10: the connection pool, the drain, and the auth timing defect` | 771 tests + 4 acceptance |
| `The spec settlements: five open questions answered, and SPEC.md amended` | 772 tests + 4 acceptance |
| `Cargo.lock: bump chacha20 0.10.1 -> 0.10.2, which was yanked` | 772 tests + 4 acceptance |
| `Phase 11: listeners on 25/465/587, inbound TLS, and the sender ACL` | 830 tests + 5 acceptance |

Since phase 11 the history is one commit per change, each a `Feature:`, `Fix:`,
`Tests:`, `Docs:` or `Spec:` commit, with `release: X.Y.Z` commits between. The
most recent: `Feature: header_rewrites …` (D-089), `release: 0.4.0`, `Spec: amend
for header_rewrites …`, then thread affinity (D-090) and `release: 0.5.0`.
Releases are tagged; the tag is pushed only after `build.yml` is green on the
tagged commit.

**Phase 10 is one commit**, for the same reason phase 8 was: it was built from a
clean tree, so there is no interleaving to untangle. It carries D-066's auth fix
alongside the pool, which is a separable change and was flagged as such in the
plan — it goes here because phase 10 is the hardening phase and that defect had
been carried, documented and unfixed since phase 2. Splitting it would produce a
one-line commit whose reason lives in the other one.

**Phase 8 is one commit and did not need to be more.** Unlike every other
boundary in this table it was built from a clean tree — phase 7 and the
multi-instance correction went in first — so there was no interleaving to
untangle. It carries D-062 (the licence gate) alongside D-063..D-065 because
D-062 is what authorised the `hickory-resolver` adoption the phase depends on:
the gate was verified *before* the dependency was added, and separating them would
put the dependency in a commit whose licence policy was still an open question.

**Why phase 7 and the multi-instance correction share a commit.** The same reason
as phases 5 and 6. The correction changed no code — it added
`tests/quota_multi_instance.rs` and `docs/MULTI_INSTANCE.md`, and edited
`DECISIONS.md`, `README.md`, `CLAUDE.md` and this file — and every one of those
documents had been rewritten by phase 7 first, in the working tree, without an
intermediate snapshot. Splitting them would mean hand-reverting some twenty edits
across five documents to reconstruct a phase-7 tree that exists nowhere, which is
the fabricated-intermediate-state problem recorded twice below. The correction is
`D-061` and is self-contained in the history that matters.

**Why phases 5 and 6 share a commit**, along with D-047 which sits between them:
phase 5 was still uncommitted when phase 6 began, and phase 6's edits sit on top
of it in `src/config/validate.rs`, `src/rewrite/mod.rs`, `src/relay.rs` and every
document. Splitting them would mean reconstructing intermediate trees that were
never tested in that form — the same reasoning as phases 2 and 3 below.

Phase 4 is one commit covering both the engine and the acceptance suite. They are
separable in principle, but the suite's §4.3 and §4.4 assertions only mean
anything against the engine, and the engine's most valuable evidence is those
assertions — splitting them would produce one commit whose tests do not yet test
what they claim to.

**Why phases 2 and 3 share a commit.** The work was never snapshotted between
them, and several files — `src/relay.rs`, `src/smtp/session.rs`, `src/main.rs`,
`Cargo.toml` — were written in phase 2 and then rewritten in phase 3. Their
current content is phase 3's. Committing the phase-2 files alongside a phase-3
`relay.rs` would produce a commit that does not build, and reconstructing an
intermediate tree would mean fabricating a state that never existed and was never
tested. The phase 1 boundary was real (its tree was still intact in the index) and
was verified to build and pass its 74 tests before being committed.
