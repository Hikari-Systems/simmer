# Resume prompt — Simmer, after phase 7 and the multi-instance correction

> Paste everything after the horizontal rule into a fresh Claude Code session
> opened in `/home/rickk/git/hs/simmer`.

---

We are building `simmer` in this directory — a Rust SMTP relay facade
(`bookworm-slim` container) that applies a domain reputation warm-up ramp between
an application and real SMTP providers. Phases 1–7 of ten are done and phase 9 is
void, so §§4–7 and §9 are complete. The multi-instance correction that followed
phase 7 is also done (D-061, `docs/MULTI_INSTANCE.md` — no code changed).
**Phase 8, the DNS preflight, is next.**

## Read these first, in order

1. `docs/SPEC.md` — **authoritative**. Never amend it; record divergences in
   `DECISIONS.md`. §6.7 is phase 8's subject. §2.2 and §11 are what
   `docs/MULTI_INSTANCE.md` diverges from.
2. `CLAUDE.md` — the five constraints that will bite you, and the key-file map.
3. `docs/STATE.md` — where the build has got to, what is tested, what is not.
   §6 ("Known gaps") and §7 ("Outstanding non-code items") are the honest list.
4. `DECISIONS.md` — 61 decisions (D-001..D-061). **There are no open questions
   left**: all twelve are closed or dissolved, O-11 last, by D-053.

## Where we are

714 tests pass, plus 4 acceptance tests against a real stack.
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and
`cargo deny check` are clean. `docker compose up -d --build` comes up healthy.

Simmer accepts a message, authenticates the client, **refuses a second `RCPT TO`**
(D-047), buffers the body, matches a sender rule, resolves the recipient's domain
group, walks the chain — skipping a route whose §7.3 recipient-frequency window is
full, then reserving quota under a row lock — rewrites the message to the selected
route's identity (headers and body both), forwards to that route's downstream over
TLS, and maps the verdict back, committing quota **and recording the frequency
event** only on a downstream `2xx`.

Since phase 7 it also carries the §9 control plane: a Prometheus exporter, a read
API, a write API with an audit log, and `POST /dryrun`. `/health`, `/healthcheck`
and `/metrics` are open; everything else needs a bearer token, **including the
reads** (D-055).

**§6 is finished apart from §6.7's DNS preflight.** §7 and §9 are finished.

`hs-utils` is no longer a dependency (D-060) — there are **no git dependencies at
all**, and `deny.toml` has no `allow-git` entry. Do not reintroduce one to reach
for a helper; copy what you need and record why.

Committed and pushed on `main` (remote `origin`): phase 1, then phases 2–3, then
five commits of planning and CI work, then phase 4, then phases 5–6, then phase 7
together with D-060 and the multi-instance correction. The working tree is clean.
Do not commit or push unless asked.

---

## Done since phase 7 — the multi-instance correction (D-061)

Recorded here because the next reader will meet its output, not because anything
is outstanding. **No code changed**; it added `tests/quota_multi_instance.rs`,
`docs/MULTI_INSTANCE.md`, D-061, and corrections to D-007 and `README.md`.

The claim that two Simmers against one database is "precisely the window in which
quota overshoot occurs" was **wrong**, and had been since phase 3. §7.4's
reservation does its headroom check and its write inside one transaction holding a
row lock — `models::quota::lock_usage` uses `INSERT … ON CONFLICT DO UPDATE`
precisely because `DO UPDATE` locks a row that already exists — and Postgres
serialises contenders for that row across processes as readily as across tasks.

D-007's decision stands (simmer stays single-instance, off the spot fleet); only
its reason changed. The real constraints are config skew during a roll under
D-026, and D-049's §7.3 race — **whose bound is `threshold + (C - 1)` for peak
concurrency `C`, not the flat "one extra message" the plan assumed.**

Two things worth carrying forward if you write race tests here:

- All five passed first time, so `lock_usage` was temporarily replaced with an
  unlocked read to check they could fail. Two did. Do this.
- That exercise found the mechanism test passing for the wrong reason **twice**:
  it raced for a row that did not yet exist (where the unique index serialises
  contenders regardless), and its "still blocked" assertion holds even against an
  unlocked read. Both are documented in the test.

`docs/MULTI_INSTANCE.md` asks the spec's author three questions, the first being
whether §2.2's "one Simmer instance **owns** its quota state" wants rewording,
since Postgres owns it.

---

## Phase 8 — the DNS preflight (§6.7)

The last of §6. `SPEC.md` §6.7 is one table and two paragraphs; read it whole.

Three checks against the outbound identity's domain, per route with
`preflight.enabled: true`: SPF (a `v=spf1` TXT containing the configured
`spf_include`), DKIM (`<selector>._domainkey.<domain>` resolving to a TXT with a
non-empty `p=`), and DMARC (`_dmarc.<domain>` with `v=DMARC1`, only when
`require_dmarc: true`). At startup and on an interval, default 15 minutes.

**Everything phase 8 needs is already shaped for it:**

- `config::Preflight` exists with `enabled`, `spf_include`, `dkim_selector`,
  `require_dmarc` and `strict`. An absent block means disabled, and when the block
  is present `spf_include` and `dkim_selector` are required — D-009.
- `chain::SkipReason::Preflight` has existed **unconstructed** since phase 3, with
  the metric label `"preflight"` already pinned by a test.
- `src/admin/view.rs` reports `"preflight": null` per route, deliberately, so an
  operator can see the field exists and has no answer. Phase 8 fills it in.
- `simmer_preflight_ok{route,check}` is the one §9.1 metric with no emitter;
  `src/metrics.rs`'s module comment lists it as absent by dependency.
- §12.1 suggests `hickory-resolver`. **Check its licence across every published
  version before adopting it** and record the finding in `LICENSES.md` §8 — that
  is a standing requirement, not a formality.

### What will bite you

- **The default is non-blocking, and that is the whole design.** A failing check
  is a `WARN` and a `0` gauge; it does **not** prevent startup or block mail,
  "because a DNS blip would otherwise become an outage". Only `strict: true` makes
  a route ineligible — and then the message *steers to the next link*, exactly like
  §7.3, and a chain with none left is §10.3's `451`. Never a drop, never a `5xx`.
- **`strict: true` on the last link of a chain has nothing to steer to**, so it
  stops steering and starts refusing. D-052 made exactly this a startup `WARN` for
  `recipient_frequency`; preflight deserves the same treatment and the same
  reasoning.
- **The interval loop needs the shutdown token**, like `quota::sweeper::run` and
  `frequency::sweeper::run`, and should not be started at all when no route
  enables preflight — the phase 6 sweeper's precedent.
- **Resolution happens against the *rewritten* identity's domain**, not the
  incoming one. The domain to check comes from the route's `identity`, which is a
  template — so it may not be a constant. Decide what to check when it is not, and
  record it.

---

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
- **The control plane must not lie and must not leak** (§9): the row wins over the
  schedule and drift is flagged (D-026), gauges are recomputed on scrape (D-056),
  no read endpoint emits a recipient or a recipient key — there is a test asserting
  none of them contains so much as an `@`.
- **Building the shipped image needs `--target runtime`.** The Dockerfile's last
  stage is the acceptance loadgen.

## How to work

Build in the phases of `SPEC.md` §13. **Write a short plan at the start of each
piece and wait for confirmation before writing code.** Test-first for the
logic-heavy parts. At the end, summarise what changed, what is tested, what is
not, and anything the spec did not cover, appending to `DECISIONS.md`. Raise
ambiguities rather than defaulting past them.

Storage follows the hikari-systems data-service pattern (`hs-rust-data-service`
skill): runtime sqlx, never `query_as!`; `models/<entity>.rs` free functions over
`&PgPool`; plain-SQL idempotent migrations applied at startup. Simmer diverges on
config, HTTP and logging (D-003..D-006), and no longer shares any code with the
estate (D-060) — the pattern is followed by convention.

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

And the acceptance tier, which is not in `cargo test`. Worth running if anything
in `src/relay.rs`, `src/routing/` or `src/rewrite/` changes — **which phase 8
does**, since preflight adds a skip reason to the chain walk:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

**Every** compose invocation must carry `SIMMER_WARMUP_STARTED` (D-042), or
compose silently re-creates `app` at the default warm-up instant mid-test and the
suite measures the wrong day while appearing to pass.

`DATABASE_URL` is needed only by `tests/quota*.rs`, `tests/frequency.rs`,
`tests/admin_api.rs` and `tests/metrics_endpoint.rs` (D-031). It does not affect
the Docker build.

`tests/metrics_endpoint.rs` is its own binary and every test in it takes a mutex:
`metrics` permits one global recorder per process, and a gauge is keyed only by
its labels. If you add a metrics test, take the mutex.

## Outstanding non-code items

- **`docs/RECIPIENTS.md` needs the spec's author** (D-047). It deletes §13's phase
  9 and reverses §5.6; either the spec absorbs it or the divergence stands.
- **`SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2** (D-036). The
  spec author's call; `simmer.yaml` was corrected and `SPEC.md` was left alone.
- **`docs/INGRESS.md`** (D-033) — listeners, inbound TLS and a sender ACL,
  designed and not built. Reverses four `SPEC.md` passages.
- **`docs/MULTI_INSTANCE.md`** (D-061) — quota is cross-instance safe and §2.2's
  wording assumes it is not. Three questions at the end, for the same author.
- The `smtp/auth.rs` timing defect, still unfixed and still separable: the
  unknown-username decoy hash is minted at fixed argon2 parameters while
  verification is parameter-agnostic, so an operator minting with different
  parameters silently reopens the oracle the decoy exists to close.
- Decide whether to keep the cargo-deny licence gate (analysis in `LICENSES.md`).
- `hs-utils` — and every hikari-systems Rust service — declares no `license`
  field. One line upstream fixes it estate-wide. No longer affects simmer, which
  dropped the dependency in phase 7 (D-060).
- The five older data services call `prepare_config` before `apply_env_overrides`,
  which is the wrong order: a `[SECRET]:` supplied via env is never resolved.
