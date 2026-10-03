# Spool and per-segment rate limits — implementation plan and handover

Branch: `feature/segment-rate-limits-and-spool`. Written 2026-10-03 so a fresh
session can continue without the conversation that produced it.

**Authority.** Everything below was decided by the spec's author on
2026-10-03. That includes the instruction to **amend `docs/SPEC.md` on this
branch** for rate limits and the opt-in spool. `CLAUDE.md` rule #1 ("not an
MTA, no spool") and rule #8 still stand for `main` and for every synchronous
ramp. This plan is the sanctioned exception, and amending those two rules is
part of Phase 2. Do not treat rule #1 as a reason to refuse this work.

**Working agreement for this plan.** The author asked for the phases to run
straight through and land as **one PR at the end of Phase 3**. Do not stop for
per-phase approval. Every decision still gets a `DECISIONS.md` entry.

---

## 1. Status

| Phase | State | Commits / entries |
| --- | --- | --- |
| 0 — groundwork, defect fixes | **Done**, pushed | `ca60b07`…`be0f6f3`, D-101–D-110, O-19 |
| 1 — per-segment rate limits, synchronous mode | **Done**, pushed | `476d6e4`…`29c94ed`, D-111–D-115, O-20, O-21 |
| 2 — the spool core + spec amendment | **In progress**: config types, §4.2 checks, redaction, D-116–D-121 (see §1.1) | — |
| 3 — operating the spool: control plane, metrics, drain, PR | Not started | — |

### 1.1 Exactly what Phase 2 has so far

There is one commit of work in progress, in `src/config/mod.rs` only. It compiles, but nothing reads it yet:

- `Config.spool: Option<Spool>` and `Ramp.delivery: Delivery { Synchronous (default), Spool }`.
- `Spool { body_store, max_hold (6h), cross_day_boundary (false), retry, max_messages (10 000), max_bytes (1 GiB), dispatch, dead_letter }`.
- `BodyStoreConfig::{Volume { path }, Object(ObjectStoreConfig)}`, `ObjectProvider::{S3, Azure}`.
- `SpoolRetry { initial 1m, max 30m, factor 2.0 }`, `SpoolDispatch { poll_interval 1s, batch 32 }`.
- `DeadLetter { retention 7d, keep_body 0s, webhook }`, `Webhook { url, timeout 5s, include_addresses true }`.
- The `OnLimit::Wait` doc comment now describes spool semantics.

**Step 0 — done (2026-10-03, second session).** All three items below are fixed: `check_spool` in `config/validate.rs`, redacting `Debug` on `ObjectStoreConfig` and `Webhook`, and D-116–D-121 written. D-118 also lets a waiting route be last in a spooling chain and exempts spooling ramps from D-115's client-budget rule.

**Were to be fixed before anything else in Phase 2:**

1. **Unvalidated opt-in.** `delivery: spool` currently loads and is silently ignored, so the ramp stays synchronous. Add the §4.2 checks in §3.2 below *before* any other Phase 2 work, so that a config cannot claim to spool without spooling.
2. **Credentials leak through `Debug`.** `ObjectStoreConfig` derives `Debug`, but it holds `secret_access_key` and `access_key`. Give it a redacting `Debug`, as the rest of `config/mod.rs` does for credentials, and do the same for `Webhook.url` if it can carry a token.
3. **Missing decision entries.** The comments cite D-116, D-117, D-118, D-119, D-120 and D-121, and none of them exist yet. Write them; the intended numbering is in §4.

---

## 2. The author's decisions (2026-10-03)

| # | Question | Decision |
| --- | --- | --- |
| Q1 | Spool at all? | **Yes, disabled by default, enabled by configuration** per ramp. With no `spool:` block and no ramp opting in, behaviour is byte-for-byte today's. |
| Q2 | How is a permanent failure after `250` reported? | **Dead-letter list**: table + metric + optional webhook. simmer never composes mail (no DSNs). |
| Q3 | May a retry change route? | **No.** Retries stay on the route of the first downstream attempt; re-walk only if that route is paused or not started. |
| Q4 | Max hold? | **6 h default, configurable**; never past the route's next day boundary unless `cross_day_boundary: true`. |
| Q5 | Multi-instance? | **Built safe for several instances** (shared DB + shared volume or object store); single instance stays the *supported* deployment. |
| Q6 | Plaintext at rest? | **Delete bodies promptly** on delivery or dead-letter; encryption is left to the volume / object store. |
| Q7 | Rate shape? | **Its own per-day schedule**, indexed like the caps (built in Phase 1, D-111). Fixed `per_hour` only for overflow routes. |
| O-20 | Amend the spec? | **Yes, on this branch.** |
| Body store | Volume or object store? | **Both, by configuration**: local volume (single instance), shared volume or object store (multi-instance). |

**Q2 vs Q6 conflict.** A dead letter with no body cannot be retried. The default follows Q6: the body is deleted at dead-letter. `dead_letter.keep_body: <duration>` keeps the body that long so `POST /spool/dead/{id}/retry` can requeue the message. This was put to the author and not overridden; record it as D-121.

---

## 3. Phase 2 — the spool core

### 3.1 Design principles (non-negotiable)

- **State lives in the database; bodies live in the body store.** Bodies never go in the database: 25 MiB writes would share the connection pool with the §7.4 reservations, double-write through WAL/TOAST, churn vacuum, and bloat backups.
- **Atomicity comes from ordering, not from co-location.**
  - Accept: `put` body (durable) → insert row → reply `250 2.0.0 queued as <id>`.
  - After commit: delete the body. The orphan sweeper is the backstop.
- **Claims use short row locks plus a lease. Never hold a lock across a delivery.**
- **Completion is fenced by `lease_token` and is one transaction with the quota commit** (`commit_and_complete`).
- **Delivery is at-least-once**, the same window as §10.2. Count it; never hide it.
- **The capture is not the spool, and must not feed it** (rule #8 still holds). The spool is a new module, `src/spool/`.
- **No spool condition is ever a 5xx to the client** (§14.1). Admission refusals are `451 4.7.1`.

### 3.2 Config and §4.2 validation

Config shape (types already exist, §1.1):

```yaml
spool:
  body_store: { kind: volume, path: /var/lib/simmer/spool }
  # or: { kind: object, provider: s3, bucket: simmer-spool, region: eu-west-2,
  #       access_key_id: ${S3_KEY}, secret_access_key: ${S3_SECRET} }
  max_hold: 6h
  cross_day_boundary: false
  retry: { initial: 1m, max: 30m, factor: 2.0 }     # full jitter
  max_messages: 10000
  max_bytes: 1073741824   # bytes, like capture.max_queue_bytes
  dispatch: { poll_interval: 1s, batch: 32 }
  dead_letter:
    retention: 7d
    keep_body: 0s
    webhook: { url: https://…, timeout: 5s, include_addresses: true }
ramps:
  main:
    delivery: spool        # default: synchronous
```

Violations:

- `delivery: spool` without a top-level `spool:` block.
- `rate.on_limit: wait` on a route not reachable from a `delivery: spool` ramp. This replaces Phase 1's blanket refusal of `wait`.
- `share` (partial ramp) together with `on_limit: wait` on the same route. D-097 exists only because simmer could not wait.
- Zero durations; `retry.factor < 1`; `retry.initial > retry.max`; `max_hold` of zero.
- Object store field mismatches:
  - S3 needs `bucket`; Azure needs `container` and `account`.
  - Credentials must be both or neither.
  - An `http://` endpoint needs `allow_http`.
- A `spool:` block that no ramp uses is a **warning**, not a violation.

The volume path is checked at startup by writing, `fsync`ing, reading back and deleting a probe file, and startup is refused if that fails. Also log a WARN that the probe proves writability, not `fsync` durability, on network filesystems.

### 3.3 Storage — both backends, conformance suite first

Tables go in both `migrations/` and `migrations-mssql/`, with BIN2 collation on keys.

```sql
CREATE TABLE spool_message (
  id              UUID PRIMARY KEY,
  ramp            TEXT NOT NULL,
  domain_group    TEXT NOT NULL,
  group_basis     TEXT NOT NULL,          -- literal | mx:<host> | fallback…
  state           TEXT NOT NULL,          -- queued | leased | delivered | dead
  next_attempt_at TIMESTAMPTZ NOT NULL,
  lease_owner     TEXT,
  lease_until     TIMESTAMPTZ,
  lease_token     UUID,
  attempts        INT NOT NULL DEFAULT 0,
  pinned_route    TEXT,                   -- Q3: set at the first downstream attempt
  booked_route    TEXT, booked_group TEXT, booked_tat TIMESTAMPTZ,  -- a deferred rate slot
  received_at     TIMESTAMPTZ NOT NULL,
  expires_at      TIMESTAMPTZ NOT NULL,   -- min(received_at + max_hold, day boundary unless crossing)
  envelope        JSONB NOT NULL,         -- mail_from, rcpt, smtputf8, body_8bitmime, helo, peer,
                                          -- authenticated, tls, correlation_id, ramp_source
  body_ref        TEXT NOT NULL,
  body_bytes      BIGINT NOT NULL,
  body_sha256     BYTEA NOT NULL,
  uuid_seed       UUID NOT NULL,
  dead_reason     TEXT,                   -- rejected | expired
  last_code       INT,
  last_error      TEXT,
  dead_at         TIMESTAMPTZ
);
CREATE INDEX spool_due ON spool_message (state, next_attempt_at);
```

New `QuotaStore` (or sibling `SpoolStore`) methods, each written in **both** stores, with tests in `tests/store_conformance/` first:

- `enqueue(row)`
- `claim_due(owner, now, batch, lease)`
  - One short transaction.
  - Postgres: `FOR UPDATE SKIP LOCKED`. SQL Server: `WITH (UPDLOCK, READPAST, ROWLOCK)` + `OUTPUT`.
  - Sets a fresh `lease_token`.
  - Due means `next_attempt_at <= now` AND (`queued` OR (`leased` AND `lease_until < now`)).
- `renew_lease(id, token, until)`
- `reschedule(id, token, next_attempt_at, error, booked slot?)`, fenced.
- `dead_letter(id, token, reason, code, error)`, fenced.
- `commit_and_complete(reservation, recipient_keys, spool_id, token)`
  - One transaction: §7.4 commit, §7.3 events, and the row set to `delivered` (or deleted).
  - Fenced. Zero rows → roll back everything, log the lost lease, and count it.
- `expire_due(now)`, `purge_dead(older_than)`, `lane_depth(ramp, group)`, `totals()`.

Conformance tests:

- Claim exclusivity race: two pools, warmed, behind a barrier. It must fail with `SKIP LOCKED` / `READPAST` removed.
- Fenced completion after lease loss.
- `commit_and_complete` atomicity, with a failure injected between steps.
- Reschedule, expiry, and dead-letter retention.

### 3.4 Body store — `src/spool/body.rs`

The interface is free functions over an enum of implementations: `put(id, bytes) -> BodyRef`, `get(ref) -> bytes` (verify sha256), `delete(ref)`, `list_orphans(older_than)`.

- **Volume:**
  - Write to a temp name, `fsync` the file, rename, `fsync` the directory.
  - Mode `0600` inside a `0700` directory, owned by UID 1000.
  - `docker-compose.yml`: add a named volume for the path; the root stays `read_only`.
- **Object:** S3-compatible and Azure Blob.
  - Pick a crate whose licence is Apache-2.0-compatible across all versions.
  - Use `default-features = false`, ring (not aws-lc-rs) and the platform root store.
  - No git dependencies; prove it with `cargo deny check` on both builds.
  - If no crate qualifies cleanly, implement S3 SigV4 over the existing hyper / hyper-rustls client. Test it against the published AWS SigV4 vectors and a local fake HTTP server.
  - Azure may then be deferred, with a D-entry and a clear config violation.
- **Orphan sweeper:** delete bodies with no row, older than 10 minutes.

### 3.5 Accept path — `src/smtp/session.rs`, at the final dot, `delivery: spool` ramps

1. **Synchronous checks first.** Run every existing one unchanged: ACL, strict senders (550), malformed `From:`, smuggling (554), size, ramp selection. The capture writes before, as today.
2. **Admission.** Answer `451 4.7.1` (exact text in `reply.rs`) when any of these holds:
   - `max_messages` or `max_bytes` is exceeded.
   - The forecast wait for this lane, `(ramp, domain_group)`, exceeds the message's hold.
     - Forecast = queued messages in the lane ÷ today's hourly rate of the first rate-limited route in the resolved chain for that group.
     - If an unrated route comes before it in the chain, the forecast is 0.
     - D-entry.
3. **Enqueue.** Generate `uuid_seed`, `put` the body, insert the row, then reply `250 2.0.0 queued as <id>`.

### 3.6 Rewrite stability across attempts

- `{{uuid}}` becomes **one value per message from a seed**. Today it yields a fresh value per occurrence (`rewrite/template.rs` ~:401). Synchronous mode gets a fresh seed per message.
- The `Received:` line uses `received_at`. `{{now.*}}` also uses `received_at`, so every attempt's output is byte-identical (D-entry).
- The §6.6 startup probe and `tests/rewrite_stability.rs` must cover the seeded `{{uuid}}`.
- Add a test: two attempts of one message produce byte-identical output.

### 3.7 Dispatch — `src/spool/dispatch.rs`

- **The task.** One task polls on `dispatch.poll_interval`, claims up to `batch`, and runs attempts concurrently.
  - Concurrency per route is bounded by the existing pool's `max_connections`.
  - `PoolExhausted` → requeue about 1 s later. That is not a failure and not an attempt.
- **Leases.** Lease = the route's downstream budget + margin, as `quota::reservation_expiry` computes it; renew at half-time.
- **Shared relay path.** Refactor `relay.rs` into a shared `attempt(owned message, mode) -> AttemptOutcome`.
  - The session maps the outcome to a reply exactly as today; `tests/relay_mapping.rs` stays green.
  - The dispatcher maps it to complete, reschedule or dead-letter.
- **Walk in spool mode** (`on_limit: wait`):
  - If a slot books within the message's remaining hold, return `Walk::Deferred { until }`.
    - Take **no** reservation. Store `booked_route`, `booked_group` and `booked_tat`, and set `next_attempt_at = until`.
    - The deferred attempt **uses its booked slot** rather than booking again.
    - If its reservation then finds no headroom: unbook (only-if-last, D-114) and continue under normal walk rules.
  - Beyond the hold: steer, exactly as `steer`.
- **Q3 pinning.**
  - Set `pinned_route` at the first downstream attempt.
  - Later attempts walk only that route, unless it is paused or not started, in which case they do a normal walk.
  - If the pinned route has no headroom or no slot, reschedule with backoff until expiry.
- **Outcomes:**
  - `2xx` → `commit_and_complete`, then delete the body (best effort).
  - `4xx`, connect, timeout or protocol error → release the reservation, unbook if possible, and reschedule with jittered exponential backoff (fenced).
  - §10.2 ambiguous final dot → keep the slot spent (D-114) and reschedule. This is at-least-once; increment `simmer_ambiguous_delivery_total`.
  - `5xx` → dead-letter `rejected` (fenced).
  - `expires_at` passed → dead-letter `expired`.
- **Dead-letter:**
  - Delete the body unless `keep_body` is set.
  - POST the webhook with: id, ramp, domain_group, route, code, text, attempts, received_at, and the envelope addresses when `include_addresses` is on.
  - Bound the timeout and retries; a webhook failure never changes the message's state.

### 3.8 Shutdown (§10.4)

1. Stop claiming.
2. Let in-flight attempts finish within the grace period; they hold leases.
3. Never drop an attempt past the final dot (same rule as D-106).
4. Release reservations of anything cut.
5. Leases simply expire for the next instance to pick up.

### 3.9 Spec and doc amendments (marked like earlier amendments)

`docs/SPEC.md` — amend each section below and mark it the way earlier amendments are marked:

- §1.1: removal requires a drain when a ramp spools.
- §2.1, §2.2: the spool carve-out; still no DSN.
- §2.3: rate limiting now exists; multi-instance is safe but single is supported.
- §3.2 step 3: c″ rate and `Deferred`.
- §4.1 / §4.2: rate and spool config, and their rules.
- §7: new §7.6 rate limits and §7.7 spool lifecycle.
- §8.1: spooled bodies versus the tmpfs DATA buffer.
- §9.1: metrics.
- §10.1–10.4: `250 queued`, admission `451`, at-least-once delivery, shutdown.
- §11: the tables and the body store.
- §13: the phases.

`README.md`:

- Update "What it is not".
- Add a "Spooling" section covering configuration, drain-before-removal, and data protection.

`CLAUDE.md`:

- Rewrite rules #1 and #8 to say exactly what is now allowed (opt-in spool, `src/spool/`) and what is still forbidden: synchronous mode never persists, and the capture never becomes or feeds the spool.
- Update Key files.
- Remove the pointer to this plan once Phase 3 merges.

`DECISIONS.md`:

- Write D-116 onwards.
- Close O-20.

### 3.10 Phase 2 tests

- **Unit:** pure functions with fixed instants; tokio paused time for the dispatcher loop only where no real sockets are involved. Phase 1 found that paused time plus TCP fires the downstream timeouts.
- **Conformance:** §3.3, against both stores.
- **End to end, in-process, with `FakeDownstream`'s per-message scripting and per-command timestamps:**
  - Accept → `250` → delivery paced per domain group at the rate schedule.
  - A `4xx`, then success on the same route (Q3).
  - A `5xx` → dead-letter, and the webhook is received by a local HTTP server.
  - Expiry.
  - Crash simulation: a second Engine and dispatcher on the same database claims after lease expiry, and exactly one delivery is recorded downstream when the first instance never reached the dot.
  - Admission `451`.
  - Byte-identical retries.
  - Synchronous ramps unaffected: every existing suite stays green.

---

## 4. Planned decision numbering

| Entry | Subject |
| --- | --- |
| D-116 | The spool: opt-in per ramp, state in the database, bodies in a body store, at-least-once |
| D-117 | Body store: volume or object store; ordering-based atomicity; the orphan sweeper |
| D-118 | `on_limit: wait` and `Walk::Deferred`; a booked slot reused by the deferred attempt |
| D-119 | Admission bounds and the lane forecast; always `451 4.7.1` |
| D-120 | Dead-letter list and webhook (Q2) |
| D-121 | Q2 against Q6: body deleted at dead-letter by default; `keep_body` |
| D-122+ | Leases and fencing; `commit_and_complete`; the seeded `{{uuid}}` and `received_at`; Q3 pinning; Q4 hold; Q5 multi-instance; crate choice; shutdown |

---

## 5. Phase 3 — operating the spool, then the PR

- **Control plane** (`src/admin/`). Bearer-token protected, every mutation audited (D-053). No read emits an `@`; the existing test must keep passing.
  - `GET /spool`: depth, bytes, oldest age, next slot per `(ramp, domain_group)` lane.
  - `GET /spool/dead`: id, ramp, group, route, code, reason, attempts and times; no addresses.
  - `POST /spool/dead/{id}/retry`: only while the body is retained.
  - `DELETE /spool/{id}`.
  - `POST /ramps/{ramp}/spool/pause` and `/resume`.
  - `POST /ramps/{ramp}/spool/drain`: stop accepting for that ramp, deliver the backlog, and report when it is empty. This is the cutover step.
- **Metrics:**
  - `simmer_spool_depth{ramp,domain_group}`, `simmer_spool_bytes`, `simmer_spool_oldest_seconds`.
  - `simmer_spool_attempts_total{ramp,route,result}`, `simmer_spool_dead_total{ramp,reason}`, `simmer_spool_admission_refused_total{ramp,reason}`.
  - `simmer_spool_lease_lost_total`, `simmer_spool_orphans_swept_total`, `simmer_spool_webhook_total{result}`.
  - Gauges are recomputed on scrape (D-056).
- **Dry run:** for a spool ramp, report the admission verdict and the expected wait.
- **Tests:**
  - The control-plane endpoints, against the real router.
  - Drain to empty.
  - Retry from dead-letter with `keep_body`.
  - Metrics on `/metrics`.
- **PR:**
  - `git fetch origin main`, rebase, push.
  - Open **one** PR to `main` with `gh pr create`.
  - The body summarises phases 0–3, lists every D-entry and spec section changed, states plainly what was not run (below), and ends with the attribution in §7.

---

## 6. Environment notes (from the session that wrote this)

- Docker Hub and `mcr.microsoft.com` were blocked. Postgres suites ran against a **local Postgres 16 on `127.0.0.1:5433`**, with `DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer`.
- **Not yet run anywhere:** the SQL Server suites (`MSSQL_URL`), `cargo deny`, the compose gate (`docker compose up -d --build`), and the acceptance suite. Run all four before merging. The rate check runs on every message, so the acceptance suite matters.
- **Known pre-existing failure**, also on `main`: `capture::on_error_defer_refuses_before_anything_is_relayed`. The tests run as root, so `chmod 0500` does not block writes. Do not "fix" it here.
- Phase 1 counted `max_wait` in the reservation expiry and the shutdown drain bound. O-21 records that the README's "Timeout budget" undercounts the per-command budget: 370 s counted per command, against a 300 s client data timeout.

## 7. Commit and PR attribution

Every commit message ends with:

```
Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_016hoTU6VMjPw8s1UTmYzE9E
```

The PR description ends with:

```
🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_016hoTU6VMjPw8s1UTmYzE9E
```
