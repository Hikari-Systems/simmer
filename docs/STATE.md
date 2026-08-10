# Simmer — state of the build

**Snapshot taken 2026-08-10, at the end of phase 4.** This is a session-handover
document, not a maintained one: `README.md` describes the service, `DECISIONS.md`
records why it is the way it is, and `docs/SPEC.md` is authoritative over both. If
this file disagrees with any of them, they win.

---

## 1. Where the build has got to

`SPEC.md` §13 lists ten phases. Three are done.

| Phase | | Status |
|---|---|---|
| 1 | Config loading, full validation, structured logging, container skeleton | **done** |
| 2 | SMTP ingress, AUTH, limits, buffering, downstream forwarding, reply mapping | **done** |
| 3 | Postgres, migrations, quota model, reservation protocol, day index, chain selection, sweepers | **done** |
| 4 | Rewriting engine: templates, header set/remove, auth-artefact stripping, idempotency property test | **done** |
| 5 | Body rewriting with decode/re-encode | next |
| 6 | Recipient frequency: hashing, normalisation, sweeper | |
| 7 | Admin API, metrics exporter, dry-run | |
| 8 | DNS preflight | |
| 9 | Multi-recipient splitting and result collapse | |
| 10 | Hardening: pooling, graceful shutdown, acceptance suite, README | acceptance suite **built in phase 4** (D-032, D-042); pooling, shutdown and real-certificate TLS outstanding |
| 11 | *(new, beyond §13)* Listeners on 25/465/587, inbound TLS, sender ACL | **designed** — `docs/INGRESS.md`, D-033. Reverses four SPEC.md passages; needs the spec author |

### What the service actually does today

Accepts a message on port 25 from a client inside `allowed_cidrs`, authenticates
it against argon2id hashes, buffers the body (memory to 1 MiB, then an unlinked
tmpfs file), matches a sender rule, resolves the recipient's domain group, walks
the chain reserving quota under a row lock, **rewrites the message to the selected
route's identity**, forwards to that route's downstream over TLS, and maps the
downstream's verdict back on the same connection — committing the quota only on a
`2xx` at the final dot.

The rewrite is §6.1's order of operations less step 7: authentication artefacts
stripped unconditionally (§6.5), `remove_headers` then `set_headers` with
templates rendered against the message *as it arrived*, a `Received:` header
prepended, and the envelope sender computed. §6.6's stability property is checked
at startup by running the real engine against a synthetic probe, and again as a
proptest over generated messages.

**Body rewriting (§6.4) is the one part of §6 still missing.** The body is carried
through as an opaque slice, so it arrives byte for byte; `body_rewrites` is parsed
and its regexes are compiled at startup, and then ignored. That is phase 5.

---

## 2. Verification status

Everything below was run on 2026-08-08 against the current working tree.

```
cargo test                                    452 passed, 0 failed
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
| `src/` unit tests | 298 | Everything logic-heavy, in place |
| `tests/smtp_ingress.rs` | 43 | §5 ingress end to end |
| `tests/config_validation.rs` | 34 | §4.2, one test per rule |
| `tests/quota.rs` | 30 | §7 against real Postgres |
| `tests/relay_mapping.rs` | 24 | §10.1 against a scripted downstream, plus §6 through the relay |
| `tests/rewrite_stability.rs` | 9 | §6.6 as a proptest over generated messages |
| `tests/quota_relay.rs` | 8 | §7.4 through the whole stack |
| `tests/shipped_config.rs` | 5 | `simmer.yaml` round-trips |
| `tests/acceptance.rs` | 1 + 4 | Config drift guard; the rest behind `--ignored` |

### The acceptance tier

`docs/ACCEPTANCE.md`, built in phase 4 rather than phase 10 (D-042). Two Mailpit
traps, a loadgen container and `simmer.acceptance.yaml`, driven by a host test
that walks the ramp by moving `warmup.started` and re-creating the container. It
proves four things nothing else can: the ramp carrying exactly its allowance
across three simulated days, the excess reaching a *different provider*, the
rewrite as a real mail server receives it, and **both arrangements of §1.1
producing byte-equal output**.

### The container gate

`docker compose up -d --build` is not ceremony. It is the only thing that
exercises the privileged port-25 bind, the tmpfs the §8.1 buffer spills onto, and
the platform root store §8.2's `required_verify` needs. A full conversation was
driven through the running container on 2026-08-07: banner, `EHLO`, `AUTH PLAIN`,
envelope, `DATA`, and a `451 4.4.1 downstream unavailable` — with the quota row
showing `day_index 6`, `domain_group google`, `allowance 2000` (the google
override series at index 6) and `reserved` back to zero after the release.

### Running the tests

```sh
docker compose up -d simmer-db
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer
cargo test
```

`DATABASE_URL` is needed only by `tests/quota*.rs`, which use `#[sqlx::test]` for
a fresh database per test. Nothing about the Docker build depends on it — the
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
  mod.rs               §6.1's order of operations
  stability.rs         §6.6's property, against a synthetic probe
src/models/          runtime sqlx over &PgPool, house pattern
src/routing/         §5.4 sender match, §3.2.2 domain group, §3.2.3 chain walk
src/relay.rs         decide -> reserve -> rewrite -> relay -> commit/release
src/metrics.rs       §9.1 counters; no exporter until phase 7
src/admin/           §9 control plane; GET /health only
src/bin/loadgen.rs   the acceptance suite's bulk sender; not in the shipped image
tests/support/       the scripted fake downstream (§12.3)
migrations/          two: baseline (instance_config), quota (three tables)
simmer.acceptance.yaml   the acceptance stack's config (D-042)
```

Roughly 18,000 lines including tests and comments.

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
recipient_event(…)                                    phase 6 (D-029)
```

Two things about `quota_usage` that are not obvious:

- **`allowance IS NULL` means no ceiling** — an overflow route, which §3.1 says is
  never quota-limited but which still accounts, so that "how much is spilling to
  overflow" is answerable (D-024).
- **`allowance_override` is where §9.3's per-group override lives**, not on
  `route_state` as §11 suggests. Because the row is keyed by `day_index`, the
  override expires at the day boundary by construction (D-025).

---

## 5. Decisions on record

42 entries, `D-001` to `D-042`. The ones a new reader most needs:

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

Nine of the twelve original open questions are settled. Three remain, each due in
a later phase:

- **O-8** (phase 9) — §5.6's collapse table returns `550` when *any* split failed
  permanently, which records permanent state about recipients that did not fail.
  Working assumption: `550` only when *all* failures are permanent.
- **O-9** (phase 9) — §5.6 splits by route; §6.3 implies per-recipient splitting
  when a template references `recipient.*`.
- **O-11** (phase 7) — §9.3 logs mutations "with the acting token's identifier",
  but `admin.auth_token` is a single scalar with no identity.

---

## 6. Known gaps

Not bugs — scope that has not been reached, or coverage deliberately deferred.

**Functional**

- **No body rewriting (§6.4, phase 5).** `body_rewrites` is parsed and its
  regexes are compiled at startup, and then ignored. Header rewriting, templating,
  auth-artefact stripping and the envelope sender all landed in phase 4.
- `recipient.*` templates render empty above one recipient — §5.6 splitting is
  phase 9 (O-9).
- No connection pool — one downstream connection per message (D-019, phase 10).
- No admin API. `paused` / `graduated` / `allowance_override` are honoured from
  the database, but nothing writes them except tests (phase 7).
- No metrics endpoint. Counters are recorded through `src/metrics.rs`; installing
  a recorder is phase 7.
- No DNS preflight (phase 8), no recipient-frequency constraint (phase 6), no
  multi-recipient splitting (phase 9).

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
  covers the buffer, not a rewritten 25 MiB message.
- **No `Received:` chain long enough to fold.** `fold_if_needed` is unit-tested;
  nothing drives it through the relay.
- **`fail_closed`** — the `451 4.3.0` reply is unit-tested, but no test drives it
  with an actually-unreachable database.
- **§10.4 under a real `SIGTERM`** — `release_by_ids` and the registry are tested;
  the wiring in `main` is not.
- The sweeper's interval loop (`sweep_once` is tested, `run` is not).

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
- **`hs-utils` declares no `license` field**, and nor does any hikari-systems Rust
  service. One line in `hs-utils-rs/Cargo.toml` fixes it estate-wide and would let
  the `ignore-sources` line in `deny.toml` be deleted.
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

Working tree is clean. **`main` is one commit ahead of `origin/main`** — phase 4
is committed but not pushed.

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
