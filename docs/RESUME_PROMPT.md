# Resume prompt — Simmer, after phase 8

> **Historical.** This was the handover into phase 9 and is kept for the record
> only: the build is long past it — all phases done, plus the link proxy (D-083),
> the SQL Server build (D-084), the capture and `server replay` (D-085/D-086), and
> releases up to `v0.3.1`. **`docs/STATE.md` is the current snapshot**; read that
> instead. The path below is not this repository's path any more either.

> Paste everything after the horizontal rule into a fresh Claude Code session
> opened in `/home/rickk/git/hs/simmer`.

---

We are building `simmer` in this directory — a Rust SMTP relay facade
(`bookworm-slim` container) that applies a domain reputation warm-up ramp between
an application and real SMTP providers. Phases 1–8 of ten are done and phase 9 is
void, so **§§4–7, §9 and now all of §6 are complete**. The multi-instance
correction between phases 7 and 8 is done too (D-061, `docs/MULTI_INSTANCE.md` —
no code changed). **What is left is phase 10: connection pooling, graceful
shutdown under a real SIGTERM, and a real-certificate TLS test.**

## Read these first, in order

1. `docs/SPEC.md` — **authoritative**. Never amend it; record divergences in
   `DECISIONS.md`. §2.2 and §11 are what `docs/MULTI_INSTANCE.md` diverges from;
   §13's phase 10 is what is left.
2. `CLAUDE.md` — the five constraints that will bite you, and the key-file map.
3. `docs/STATE.md` — where the build has got to, what is tested, what is not.
   §6 ("Known gaps") and §7 ("Outstanding non-code items") are the honest list.
4. `DECISIONS.md` — 65 decisions (D-001..D-065). **There are no open questions
   left**: all twelve are closed or dissolved, O-11 last, by D-053.

## Where we are

747 tests pass, plus 4 acceptance tests against a real stack.
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

**§6 is finished, §6.7 included.** §7 and §9 are finished.

`hs-utils` is no longer a dependency (D-060) — there are **no git dependencies at
all**, and `deny.toml` has no `allow-git` entry. Do not reintroduce one to reach
for a helper; copy what you need and record why.

Committed and pushed on `main` (remote `origin`): phase 1, then phases 2–3, then
five commits of planning and CI work, then phase 4, then phases 5–6, then phase 7
together with D-060 and the multi-instance correction. **Phase 8 is in the working
tree, uncommitted.** Do not commit or push unless asked.

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

## Done in phase 8 — the DNS preflight (§6.7), and what it left open

§6 is finished. `src/preflight/` holds the three checks, a results registry the
chain walk reads, and a 15-minute interval task on the shutdown token; the three
sites that were shaped for it since phase 3 are filled in — `SkipReason::Preflight`
is constructed, `/routes` reports a real block, and `simmer_preflight_ok` has an
emitter. `tests/preflight.rs` is 13 tests.

**The design is non-blocking and stays that way.** A failing check is a `WARN` and
a `0` gauge. Only `strict: true` makes a route ineligible, and then the message
*steers* to the next link; a chain with none left is §10.3's `451` on the wire,
which `tests/preflight.rs` asserts through a real client rather than through the
walk's return value. **A route with no verdict yet is eligible** — fail open, so a
slow resolver at boot cannot empty a chain (D-064).

Four things to know before touching it:

- **The interval is a constant** (D-063). §4.1 defines no key for it and every
  config struct is `deny_unknown_fields`, so adding one is a schema divergence.
- **Preflight is evaluated above §7.3** in both walks (D-065) — an in-memory read
  before a high-cardinality database one. It displaces §3.2 3b's "first" by a
  position. Change `walk_and_reserve` and `dry_walk` together, always;
  `tests/admin_api.rs` pins them against each other step for step.
- **A TXT record's character-strings are concatenated** before matching. Every
  real DKIM key is longer than 255 bytes and therefore arrives split.
- **An empty `p=` fails DKIM.** That is how a *revoked* key is published, and it
  resolves — so "the selector exists" would pass while every message went
  unsigned, which is §6.5's failure precisely.

`hickory-resolver` is adopted at plain DNS (no TLS/QUIC/DNSSEC features). Licence
checked across all 21 published versions: `MIT OR Apache-2.0` throughout, no AGPL
ever. It brings 34 packages, all permissive — recorded in `LICENSES.md` §5. Note
the resume prompt used to say "§8" of that file; there is no §8, the sections run
1, 2, 4, 3.

**What it left open, and it is the more interesting half.** §6.3's grammar lets
`identity.envelope_from` have a non-constant domain — `bounce@{{original.envelope_from.domain}}`
— and §6.6 does not rule it out, because applied twice it is stable. Phase 8
declines to preflight such a route and warns, saying outright that `strict` has no
effect there (D-064). But the deeper point is that **a warming route whose identity
domain varies per message is incoherent regardless of preflight**: the ramp, the
allowance and reputation accrual all exist to build reputation for *one* domain, so
such a route warms nothing and its quota counts a mixture. That argues for a §4.2
rule on `envelope_from` for every route — beyond phase 8, and the spec author's
call.

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
in `src/relay.rs`, `src/routing/` or `src/rewrite/` changes:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

**Every** compose invocation must carry `SIMMER_WARMUP_STARTED` (D-042), or
compose silently re-creates `app` at the default warm-up instant mid-test and the
suite measures the wrong day while appearing to pass.

`DATABASE_URL` is needed only by `tests/quota*.rs`, `tests/frequency.rs`,
`tests/preflight.rs`, `tests/admin_api.rs` and `tests/metrics_endpoint.rs`
(D-031). It does not affect
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
- **A non-constant `identity.envelope_from` domain should probably be a §4.2
  error for every route**, not just declined by preflight (D-064). See the phase 8
  section above for why the ramp makes no sense otherwise.
- The `smtp/auth.rs` timing defect, still unfixed and still separable: the
  unknown-username decoy hash is minted at fixed argon2 parameters while
  verification is parameter-agnostic, so an operator minting with different
  parameters silently reopens the oracle the decoy exists to close.
- ~~Decide whether to keep the cargo-deny licence gate.~~ Settled by D-062: it
  stays, at Apache-2.0 compatibility, enforced by an allow-list so copyleft fails
  by omission. Any new dependency must clear `cargo deny check` — and note the
  `r-efi` `OR`-expression noise documented in `LICENSES.md` before reporting a
  false positive.
- `hs-utils` — and every hikari-systems Rust service — declares no `license`
  field. One line upstream fixes it estate-wide. No longer affects simmer, which
  dropped the dependency in phase 7 (D-060).
- The five older data services call `prepare_config` before `apply_env_overrides`,
  which is the wrong order: a `[SECRET]:` supplied via env is never resolved.
