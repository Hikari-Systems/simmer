# Simmer — state of the build

**Snapshot taken 2026-08-08, at the end of phase 3.** This is a session-handover
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
| 4 | Rewriting engine: templates, header set/remove, auth-artefact stripping, idempotency property test | next |
| 5 | Body rewriting with decode/re-encode | |
| 6 | Recipient frequency: hashing, normalisation, sweeper | |
| 7 | Admin API, metrics exporter, dry-run | |
| 8 | DNS preflight | |
| 9 | Multi-recipient splitting and result collapse | |
| 10 | Hardening: pooling, graceful shutdown, acceptance suite, README | |

### What the service actually does today

Accepts a message on port 25 from a client inside `allowed_cidrs`, authenticates
it against argon2id hashes, buffers the body (memory to 1 MiB, then an unlinked
tmpfs file), matches a sender rule, resolves the recipient's domain group, walks
the chain reserving quota under a row lock, forwards to the selected route's
downstream over TLS, and maps the downstream's verdict back on the same
connection — committing the quota only on a `2xx` at the final dot.

**It does not rewrite anything.** The message is forwarded byte for byte under the
identity it arrived with, so a route's *outbound identity* is currently
configuration that nothing reads. That is phase 4, and it is what makes the whole
component mean anything.

---

## 2. Verification status

Everything below was run on 2026-08-08 against the current working tree.

```
cargo test                                    323 passed, 0 failed
cargo clippy --all-targets -- -D warnings     clean
cargo fmt --all -- --check                    clean
cargo deny check                              advisories ok, bans ok, licenses ok, sources ok
docker compose up -d --build                  both containers healthy
```

| Suite | Tests | What it covers |
|---|---:|---|
| `src/` unit tests | 190 | Everything logic-heavy, in place |
| `tests/smtp_ingress.rs` | 43 | §5 ingress end to end |
| `tests/quota.rs` | 30 | §7 against real Postgres |
| `tests/config_validation.rs` | 26 | §4.2, one test per rule |
| `tests/relay_mapping.rs` | 21 | §10.1 against a scripted downstream |
| `tests/quota_relay.rs` | 8 | §7.4 through the whole stack |
| `tests/shipped_config.rs` | 5 | `simmer.yaml` round-trips |

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
src/models/          runtime sqlx over &PgPool, house pattern
src/routing/         §5.4 sender match, §3.2.2 domain group, §3.2.3 chain walk
src/relay.rs         decide -> reserve -> relay -> commit/release
src/metrics.rs       §9.1 counters; no exporter until phase 7
src/admin/           §9 control plane; GET /health only
tests/support/       the scripted fake downstream (§12.3)
migrations/          two: baseline (instance_config), quota (three tables)
```

Roughly 13,500 lines including tests and comments.

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

31 entries, `D-001` to `D-031`. The ones a new reader most needs:

| | |
|---|---|
| **D-008** | §10.1's `5xx` mapping is split by stage: `550` only at `RCPT TO`, `451` everywhere else. Catches the §6.5 provisioning risk without suppressing deliverable recipients. |
| **D-018** | `SMTPUTF8` is advertised only when every reachable route declares `downstream.smtputf8: true`. Diverges from §5.2's "advertises exactly". |
| **D-019** | Route selection and reservation are separated (O-1). No connection pool until phase 10. |
| **D-024** | Overflow routes account fully, with a synthetic Unix-epoch day origin. |
| **D-025** | The allowance override is a column on `quota_usage`, not a `route_state` field. |
| **D-026** | `quota_usage.allowance` is authoritative once written — a config change applies from the next day boundary, not retroactively. |
| **D-031** | Database tests use `#[sqlx::test]`, so `cargo test` needs a Postgres. |

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

- No rewriting of any kind (phases 4–5). `identity.set_headers`,
  `envelope_from`, `remove_headers` and `body_rewrites` are parsed, validated for
  §6.6 stability at startup, and then ignored.
- No connection pool — one downstream connection per message (D-019, phase 10).
- No admin API. `paused` / `graduated` / `allowance_override` are honoured from
  the database, but nothing writes them except tests (phase 7).
- No metrics endpoint. Counters are recorded through `src/metrics.rs`; installing
  a recorder is phase 7.
- No DNS preflight (phase 8), no recipient-frequency constraint (phase 6), no
  multi-recipient splitting (phase 9).

**Test coverage**

- **No real-certificate TLS test.** `required_verify` is asserted only through its
  failure modes. Phase 10's acceptance suite is where a real chain belongs.
- **No clock movement.** Day-index tests are pure; storage tests set `day_index`
  directly. Nothing walks a ramp across a real boundary in a running process
  (§12.3 acceptance, phase 10).
- **`fail_closed`** — the `451 4.3.0` reply is unit-tested, but no test drives it
  with an actually-unreachable database.
- **§10.4 under a real `SIGTERM`** — `release_by_ids` and the registry are tested;
  the wiring in `main` is not.
- The sweeper's interval loop (`sweep_once` is tested, `run` is not).

---

## 7. Outstanding non-code items

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

Local repository on `main`, **no remote**. Two commits:

| | |
|---|---|
| `Phase 1: configuration, validation, structured logging, container` | 74 tests |
| `Phases 2 and 3: SMTP relay and the warm-up quota model` | 323 tests |

**Why phases 2 and 3 share a commit.** The work was never snapshotted between
them, and several files — `src/relay.rs`, `src/smtp/session.rs`, `src/main.rs`,
`Cargo.toml` — were written in phase 2 and then rewritten in phase 3. Their
current content is phase 3's. Committing the phase-2 files alongside a phase-3
`relay.rs` would produce a commit that does not build, and reconstructing an
intermediate tree would mean fabricating a state that never existed and was never
tested. The phase 1 boundary was real (its tree was still intact in the index) and
was verified to build and pass its 74 tests before being committed.

Working tree is clean.
