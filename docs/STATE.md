# Simmer — state of the build

**Snapshot taken 2026-08-11, at the end of phase 7 and the multi-instance
correction that followed it.** This is a session-handover
document, not a maintained one: `README.md` describes the service, `DECISIONS.md`
records why it is the way it is, and `docs/SPEC.md` is authoritative over both. If
this file disagrees with any of them, they win.

---

## 1. Where the build has got to

`SPEC.md` §13 lists ten phases. Seven are done, and one is void.

| Phase | | Status |
|---|---|---|
| 1 | Config loading, full validation, structured logging, container skeleton | **done** |
| 2 | SMTP ingress, AUTH, limits, buffering, downstream forwarding, reply mapping | **done** |
| 3 | Postgres, migrations, quota model, reservation protocol, day index, chain selection, sweepers | **done** |
| 4 | Rewriting engine: templates, header set/remove, auth-artefact stripping, idempotency property test | **done** |
| 5 | Body rewriting with decode/re-encode | **done** |
| 6 | Recipient frequency: hashing, normalisation, sweeper | **done** |
| 7 | Admin API, metrics exporter, dry-run | **done** |
| 8 | DNS preflight | next |
| 9 | Multi-recipient splitting and result collapse | **void** — D-047 refuses multi-recipient transactions outright; `docs/RECIPIENTS.md` |
| 10 | Hardening: pooling, graceful shutdown, acceptance suite, README | acceptance suite **built in phase 4** (D-032, D-042); pooling, shutdown and real-certificate TLS outstanding |
| 11 | *(new, beyond §13)* Listeners on 25/465/587, inbound TLS, sender ACL | **designed** — `docs/INGRESS.md`, D-033. Reverses four SPEC.md passages; needs the spec author |

### What the service actually does today

Accepts a message on port 25 from a client inside `allowed_cidrs`, authenticates
it against argon2id hashes, refuses a second `RCPT TO` (D-047), buffers the body
(memory to 1 MiB, then an unlinked tmpfs file), matches a sender rule, resolves the
recipient's domain group, walks the chain — **skipping a route whose
recipient-frequency window is full** and then reserving quota under a row lock —
**rewrites the message to the selected route's identity**, forwards to that route's
downstream over TLS, and maps the downstream's verdict back on the same
connection — committing the quota, and recording the frequency event, only on a
`2xx` at the final dot.

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

**§6 is now complete apart from §6.7's DNS preflight**, which §13 puts in phase 8.
Body rewriting decodes each `text/*` part's transfer encoding and charset, applies
the route's patterns in order, and writes the part back in the encoding and charset
it arrived with — never changing either (D-045). A part nothing matched is not
re-encoded at all, so a body with no match is forwarded as the bytes it arrived as
(D-043).

---

## 2. Verification status

Everything below was run on 2026-08-11 against the phase 7 tree.

```
cargo test                                    714 passed, 0 failed
cargo clippy --all-targets -- -D warnings     clean
cargo fmt --all -- --check                    clean
cargo deny check                              advisories ok, bans ok, licenses ok, sources ok
docker compose up -d --build                  both containers healthy
```

Plus the acceptance tier, which needs its own stack and is not in `cargo test`:

```
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1     4 passed, 0 failed
```

| Suite | Tests | What it covers |
|---|---:|---|
| `src/` unit tests | 458 | Everything logic-heavy, in place |
| `tests/admin_api.rs` | 47 | §9 against the real router and real Postgres |
| `tests/smtp_ingress.rs` | 44 | §5 ingress end to end |
| `tests/config_validation.rs` | 46 | §4.2, one test per rule |
| `tests/quota.rs` | 30 | §7 against real Postgres |
| `tests/quota_multi_instance.rs` | 5 | Two independent pools, one database (D-061) |
| `tests/relay_mapping.rs` | 28 | §10.1 against a scripted downstream, plus §6 through the relay |
| `tests/frequency.rs` | 21 | §7.3 against real Postgres, and through the relay |
| `tests/rewrite_stability.rs` | 11 | §6.6 as a proptest over generated messages, bodies included |
| `tests/metrics_endpoint.rs` | 10 | §9.1 against a real recorder — its own binary, deliberately |
| `tests/quota_relay.rs` | 8 | §7.4 through the whole stack |
| `tests/shipped_config.rs` | 5 | `simmer.yaml` round-trips |
| `tests/acceptance.rs` | 1 + 4 | Config drift guard; the rest behind `--ignored` |

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
proves four things nothing else can: the ramp carrying exactly its allowance
across three simulated days, the excess reaching a *different provider*, the
rewrite as a real mail server receives it — headers and, since phase 5, the body
link — and **both arrangements of §1.1 producing byte-equal output**.

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
  buffer.rs            §8.1 transient buffer, dot transparency
  session.rs           the §5.2 state machine
  mod.rs               listener, CIDR check, session cap, shutdown token
src/downstream/      §8 outbound leg
  stream.rs            the four TLS modes over rustls
  client.rs            the SMTP conversation, per-stage timeouts
  outcome.rs           §10.1 + D-008, as data
src/frequency/       §7.3 recipient frequency
  mod.rs               normalisation, the keyed hash, the rolling window
  sweeper.rs           hourly eviction past the longest window plus a margin
src/quota/           §7
  day.rs               §7.2 elapsed-duration day index
  store.rs             §11's storage trait
  postgres.rs          the §7.4 three-phase protocol
  mod.rs               allowance resolution, reservation expiry
  registry.rs          §10.4 in-flight reservations
  sweeper.rs           §7.4 expiry release
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
src/models/          runtime sqlx over &PgPool, house pattern
  quota.rs             the §7.4 statements
  route_state.rs       §9.3 admin state
  instance_config.rs   §7.3's salt, get-or-insert (D-050)
  recipient_event.rs   §7.3's events: count, record, evict
src/routing/         §5.4 sender match, §3.2.2 domain group, §3.2.3 chain walk
src/relay.rs         decide -> reserve -> rewrite -> relay -> commit/release
src/healthcheck.rs   the `healthcheck` subcommand. Stdlib only (D-060)
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
tests/quota_multi_instance.rs  two independent pools against one database (D-061)
tests/metrics_endpoint.rs  §9.1 against a real recorder; its own binary
migrations/          three: baseline (instance_config), quota (three tables),
                     recipient_event (D-048)
simmer.acceptance.yaml   the acceptance stack's config (D-042)
```

Roughly 25,000 lines including tests and comments.

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

---

## 5. Decisions on record

61 entries, `D-001` to `D-061`. The ones a new reader most needs:

| | |
|---|---|
| **D-008** | §10.1's `5xx` mapping is split by stage: `550` only at `RCPT TO`, `451` everywhere else. Catches the §6.5 provisioning risk without suppressing deliverable recipients. |
| **D-018** | `SMTPUTF8` is advertised only when every reachable route declares `downstream.smtputf8: true`. Diverges from §5.2's "advertises exactly". |
| **D-019** | Route selection and reservation are separated (O-1). No connection pool until phase 10. |
| **D-024** | Overflow routes account fully, with a synthetic Unix-epoch day origin. |
| **D-025** | The allowance override is a column on `quota_usage`, not a `route_state` field. |
| **D-026** | `quota_usage.allowance` is authoritative once written — a config change applies from the next day boundary, not retroactively. |
| **D-031** | Database tests use `#[sqlx::test]`, so `cargo test` needs a Postgres. |
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
| **D-061** | **Quota *is* safe across instances**, by the row lock — D-007's stated reason was wrong from phase 3 onward. The real constraints are config skew (D-026) and §7.3's race, whose bound is `threshold + (C - 1)`, not one message. |

**All twelve original open questions are now closed.** O-8 and O-9 were
*dissolved* rather than answered — D-047 removed the case each was about — and
O-11 was settled in phase 7 by D-053.

---

## 6. Known gaps

Not bugs — scope that has not been reached, or coverage deliberately deferred.

**Functional**

- No connection pool — one downstream connection per message (D-019, phase 10).
- No DNS preflight (phase 8), so `/routes` reports `"preflight": null` and
  `SkipReason::Preflight` is still never constructed.
- **No scopes on admin tokens.** Every token can do everything; a token that
  could read but not mutate is a plausible next ask and is not built.
- **Multi-recipient transactions are refused**, by policy rather than by
  omission — D-047, and `docs/RECIPIENTS.md`.

**Test coverage**

- **No real-certificate TLS test.** `required_verify` is asserted only through its
  failure modes; the acceptance traps are plaintext. Still open, and still what
  `docs/ACCEPTANCE.md` §5 describes — the stack it needs now exists.
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
- **§10.4 under a real `SIGTERM`** — `release_by_ids` and the registry are tested;
  the wiring in `main` is not.
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

## 6a. Known defect

**Timing-based username enumeration in `smtp/auth.rs`.** The unknown-username decoy
hash is minted at fixed argon2 parameters, but verification is parameter-agnostic —
so an operator minting with different parameters silently reopens the oracle the
decoy exists to close. Found by modelling D-033's ACL on Slater, which fixes the
same bug by borrowing the costliest hash the ACL actually holds. Live in phase 2
code, independent of D-033, and worth fixing on its own. See `DECISIONS.md`
"Defects found, not yet fixed".

---

## 7. Outstanding non-code items

**Done since phase 7, and now needing the spec's author:**

- **`docs/MULTI_INSTANCE.md`** (D-061). The multi-instance claim the repository
  made was wrong: quota is cross-instance safe by the row lock `INSERT … ON
  CONFLICT DO UPDATE` takes, and `tests/quota_multi_instance.rs` evidences it by
  racing two independent pools against one database. D-007's *decision* stands —
  simmer stays single-instance and off the spot fleet — but on the two constraints
  that are real: config skew during a roll under D-026, and D-049's §7.3 race. The
  document asks three questions, the first of which is whether §2.2's "one Simmer
  instance **owns** its quota state" wants rewording, since Postgres owns it.
  **No code changed.**

**New in phase 4, and the one that needs the spec's author:**

- **`SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2.** Its
  `envelope_from: "bounce+{{original.envelope_from.local}}@newbrand.com"` is a
  relative transformation, which §1.1 constraint 1 prohibits by name and which
  §6.6 makes a non-overridable startup error for an identity field. The stability
  check built in this phase refuses to start on it. `simmer.yaml` was corrected
  and `SPEC.md` was left alone — see D-036, which also notes what a VERP-shaped
  intent *can* be expressed as.

Carried since phase 1, none blocking:

- **Decide whether to keep the cargo-deny licence gate.** `LICENSES.md` concludes
  copyleft is a low risk for an internal container and no crate we want is
  copyleft. The advisories check is worth keeping regardless.
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

`main` is pushed through the phase 7 commit.

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
