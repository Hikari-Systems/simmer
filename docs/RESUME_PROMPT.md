# Resume prompt — Simmer, phase 7

> Paste everything after the horizontal rule into a fresh Claude Code session
> opened in `/home/rickk/git/hs/simmer`.

---

We are building `simmer` in this directory — a Rust SMTP relay facade
(`bookworm-slim` container) that applies a domain reputation warm-up ramp between
an application and real SMTP providers. Phases 1–6 of ten are done, and phase 9 is
void. You are picking up at phase 7.

## Read these first, in order

1. `docs/SPEC.md` — **authoritative**. Never amend it; record divergences in
   `DECISIONS.md`. §9 is phase 7's subject, all five subsections. Read §9.3
   alongside §14.1: a write API that can pause a route can also, through an
   `allowance_override` of zero, make every message `451`.
2. `CLAUDE.md` — the four constraints that will bite you, and the key-file map.
3. `docs/STATE.md` — where the build has got to, what is tested, what is not.
4. `DECISIONS.md` — 52 decisions (D-001..D-052) and **one** open question, O-11,
   which is phase 7's: §9.3 says mutations are logged "with the acting token's
   identifier", but `admin.auth_token` is a single scalar with no identity.
   Settle it before writing the write API rather than after.

## Where we are

593 tests pass, plus 4 acceptance tests against a real stack.
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and
`cargo deny check` are clean. `docker compose up -d --build` comes up healthy.

Simmer accepts a message, authenticates the client, **refuses a second `RCPT TO`**
(D-047), buffers the body, matches a sender rule, resolves the recipient's domain
group, walks the chain — skipping a route whose §7.3 recipient-frequency window is
full, then reserving quota under a row lock — rewrites the message to the selected
route's identity (headers and body both), forwards to that route's downstream over
TLS, and maps the verdict back, committing quota **and recording the frequency
event** only on a downstream `2xx`.

**§6 is finished** apart from §6.7's DNS preflight, which §13 puts in phase 8.
**§7 is finished.**

Committed and pushed on `main` (remote `origin`): phase 1, then phases 2–3, then
five commits of planning and CI work, then phase 4. **Phases 5 and 6 are in the
working tree and uncommitted.** Do not commit or push unless asked.

## Phase 7 — the control plane

`SPEC.md` §13.7 and §9 in full. Four pieces:

1. **The metrics exporter.** `src/metrics.rs` already records every counter and
   gauge §9.1 asks for, behind named functions, with no recorder installed —
   every call is currently a no-op (D-021). Phase 7 adds
   `metrics-exporter-prometheus` and `GET /metrics`, and **no call site should
   need to change**. If one does, that is worth understanding before changing it.
2. **The read API** (§9.2): `/routes`, `/routes/{name}`, `/quota`. `GET /health`
   exists. The data is all in `quota_usage`, `route_state` and the config.
3. **The write API** (§9.3): pause, resume, graduate, allowance override. The
   database honours all four already — `chain::walk_and_reserve` reads
   `route_states()` per message and `quota_usage.allowance_override` wins over the
   schedule (D-025) — so this is the HTTP surface and the audit log, not new
   semantics.
4. **Dry run** (§9.4).

### What will bite you

- **O-11 is the phase's open question.** Settle it first and record it.
- **An admin mutation must not become a §14.1 violation.** An override of zero, or
  pausing every route in a chain, produces `451` on every message — which is the
  correct reply and a serious operational event. Whatever the write API does, the
  answer to the client stays temporary.
- **`allowance_override` lives on `quota_usage`, not `route_state`** (D-025).
  §11 suggests otherwise. Because the row is keyed by `day_index`, the override
  expires at the day boundary by construction, which is §9.3's stated behaviour.
- **`quota_usage.allowance` is authoritative once written** (D-026). A config
  change does not retroactively raise today's ceiling, so a read API that reports
  the *configured* schedule where the row says something else will mislead an
  operator at exactly the wrong moment.
- **No plaintext recipient anywhere in the control plane.** §7.3's hashing exists
  so the container does not accumulate a record of who was mailed; a `/quota` or
  `/routes` response that exposed recipient keys would undo it. The counters are
  already shaped for this — `simmer_recipient_events_evicted_total` is
  deliberately unlabelled.
- **The admin listener is not authenticated today.** `GET /health` is open by
  design; §9.3's write API is not, and `admin.auth_token` is already in the config
  and already interpolated from the environment.

## Non-negotiables

- **Not an MTA**: no spool, queue, retry scheduler or DSN. Only quota state and
  recipient-frequency events persist. The `DATA` buffer is tmpfs and not durable.
- **Cutover invariant (§1.1)**: rewrites are absolute assignments, never relative,
  and stable under repetition. Enforced at startup by the real engine against a
  synthetic probe, for headers (D-036) and for `body_rewrites` (D-046).
- **Never emit a reply that makes a client record permanent state** (§14.1).
  `src/smtp/reply.rs` holds every reply Simmer can emit and has a test that fails
  when an undocumented `5xx` is added. Keep it that way.
- **One recipient per transaction** (D-047). A second `RCPT TO` is `452`,
  unconditionally; §5.6's splitting and §13's phase 9 are void, not deferred.
- **A `text/*` part no `body_rewrites` pattern matched keeps its original bytes**
  (D-043), and a part's transfer encoding and charset are never changed (D-045).
- Quota increments on downstream `2xx` only, via §7.4 — and §7.3's events with it,
  in the same transaction (D-051). No failover between routes (§3.3). Fail closed
  when Postgres is unavailable (§7.5).
- **Building the shipped image needs `--target runtime`.** The Dockerfile's last
  stage is the acceptance loadgen.

## How to work

Build in the phases of `SPEC.md` §13. **Write a short plan at the start of the
phase and wait for confirmation before writing code.** Test-first for the
logic-heavy parts — the §9.2 projections and the §9.3 mutation semantics are
exactly that. At the end, summarise what changed, what is tested, what is not, and
anything the spec did not cover, appending to `DECISIONS.md`. Raise ambiguities
rather than defaulting past them.

Storage follows the hikari-systems data-service pattern (`hs-rust-data-service`
skill): runtime sqlx, never `query_as!`; `models/<entity>.rs` free functions over
`&PgPool`; plain-SQL idempotent migrations applied at startup. Simmer diverges on
config, HTTP and logging — see D-003..D-006.

## Gate before pushing

```sh
docker compose up -d simmer-db
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
docker compose up -d --build      # not optional
```

And the acceptance tier, which is not in `cargo test`. Phase 7 does not touch the
relay path, so it is only worth running if something in `src/relay.rs`,
`src/routing/` or `src/rewrite/` changes:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

**Every** compose invocation must carry `SIMMER_WARMUP_STARTED` (D-042), or
compose silently re-creates `app` at the default warm-up instant mid-test and the
suite measures the wrong day while appearing to pass.

`DATABASE_URL` is needed only by `tests/quota*.rs` and `tests/frequency.rs`
(D-031). It does not affect the Docker build.

## Outstanding non-code items

- **`docs/RECIPIENTS.md` needs the spec's author** (D-047). It deletes §13's phase
  9 and reverses §5.6; either the spec absorbs it or the divergence stands.
- **`SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2** (D-036). The
  spec author's call; `simmer.yaml` was corrected and `SPEC.md` was left alone.
- **`docs/INGRESS.md`** (D-033) — listeners, inbound TLS and a sender ACL,
  designed and not built. Reverses four `SPEC.md` passages.
- The `smtp/auth.rs` timing defect, still unfixed and still separable.
- Decide whether to keep the cargo-deny licence gate (analysis in `LICENSES.md`).
- `hs-utils` — and every hikari-systems Rust service — declares no `license`
  field. One line upstream fixes it estate-wide.
- The five older data services call `prepare_config` before `apply_env_overrides`,
  which is the wrong order: a `[SECRET]:` supplied via env is never resolved.
