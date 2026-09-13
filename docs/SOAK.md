# The soak tier (T4) — what it does, and what it has found

**Status: V2 and V3 built and pushed (`e88c7be`, `c554f2e`, `a1b25c3`); V4 and the
burst/idle variants outstanding.** This document records what the soak tier is,
what two runs have established, and — at least as usefully — what they have *not*
established. `tests/soak.rs` is the build; `test/config/simmer.soak.yaml` is the
configuration; the test programme's step 5 is the plan.

The tier exists for the failures that only appear over hours: memory that creeps,
descriptors never given back, threads and tasks that accumulate, rows written and
never pruned. Peak load is T3's job. Everything here is about whether the system
returns to where it started.

```sh
# the stress stack's services; the soak differs only in SIMMER_CONFIG
SIMMER_CONFIG=/config/simmer.soak.yaml docker compose \
  -f docker-compose.yml -f test/compose/acceptance.yml -f test/compose/stress.yml \
  --profile acceptance --profile stress up -d --wait app app2 sink

SOAK_DURATION=20m cargo test --test soak -- --ignored --test-threads=1 --nocapture soak_run
SOAK_WARMUP=5m   cargo test --test soak -- --ignored --test-threads=1 --nocapture soak_analyze
```

---

## 1. Shape of the tier

`soak_run` drives the load and writes every sample to `target/soak/*.csv` **on the
host** as it goes. `soak_analyze` reads those files and judges them. They are
separate tests, and that separation has already paid for itself once (§3): a
twenty-four hour run that dies in its twenty-third hour still leaves everything it
learned on disk, and a verdict can be recomputed — with a different warm-up, say —
without paying for the run again. They cannot share a cargo invocation, because
libtest sorts test names and `soak_analyze` would run first.

Two instances, `app` and `app2`, carry identical streams against one database and
one sink. **`app` is scraped every 30 s and `app2` never is**, so a difference
between their slopes is the exporter's own doing (F8) rather than the message
path's. Without that asymmetry the difference between the instances means nothing
at all, which is why the A/B pair was worth building before any variant.

| Variant | What it drives | Status |
|---|---|---|
| **V2** | `app` scraped, `app2` never — isolates F8 | built, exercised |
| **V3** | one message in twenty from a fresh `u<n>.soak.test` sender — F7 | built, **drives the defect** |
| **V4** | relays cancelled by the session timeout — F2 | outstanding |
| bursts | 60 s at 70 clients every 15 min | outstanding |
| idle gaps | 3 min past `idle_ttl`, sink closing idle at 45 s — F10/CLOSE_WAIT | outstanding |

---

## 2. The 20-minute run — green, and F7 driven

The run that matters, on a freshly rebuilt stack with the V3 configuration.

| | `app` | `app2` |
|---|---|---|
| messages | 12,010 | 12,010 |
| accepted | **12,010** | **12,010** |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| sustained rate | 9.98 msg/s | 9.98 msg/s |
| p50 / p90 | 13.0 ms / 44.6 ms | 12.7 ms / 43.4 ms |
| p99 / max | 5,143.6 ms / 9,747.5 ms | 5,141.5 ms / 9,743.9 ms |
| peak established | 10 | 10 |
| peak CLOSE_WAIT | **0** | **0** |

**F7 reproduced and XFAILed correctly:**

```
metrics: 210 series (150 unmatched-sender) at 300s -> 660 (600) at 1201s, across 31 scrapes
XFAIL F7 soak/V3/metric-series: metric series grew by 450 (unmatched-sender 150 -> 600)
  over 15 minutes; the domain label is client-controlled
```

`simmer_unmatched_sender_total{domain}` takes its label from a domain the client
chose, so a sender nobody has seen before mints a series that is never reclaimed.
The count **quadrupled in fifteen minutes with no ceiling**. The XFAIL is the
intended result: F7 is a known, unfixed defect listed in `test/known-findings.json`,
and an unexpected *pass* would have failed the run, which is what forces the entry
to be deleted in whatever commit fixes it.

Three things that run settled which had been argued rather than demonstrated:

- **The 5% rate is right.** One id in twenty carries a fresh domain, so 12,010
  messages produce 601 of them, and the scraped series count had reached 600 by the
  last scrape 2.6 s before the run ended. At 100% it would have been 12,010 series
  in twenty minutes — the soak's own scrape time under test rather than Simmer's.
- **The ACL wildcard covers the fresh senders.** 12,010 accepted, **0 refused**, so
  `*.soak.test` in `send_as` does grant `u<n>.soak.test`. Had it not, every
  twentieth message would have been refused at `MAIL FROM` and F7 would have read
  as "not reproducing" for precisely the wrong reason.
- **The label comes from the `From:` header first.** `relay.rs` reads
  `from_header`, falling back to the envelope, so V3 varies **both**. Varying the
  envelope alone emits one series forever.

The routing side corroborates it exactly: `simmer_messages_total` counts **601
delivered on `overflow-established`** against 11,409 on the warming route, and the
loadgen sent precisely 601 fresh senders (ids `0..12009` where `n % 20 == 0`). The
fresh senders match no `senders:` rule, so every one of them falls through to
`default_chain: [overflow-established]` — a one-to-one correspondence, which is the
fall-through observed rather than inferred from the metric count alone.

---

## 3. The 1-hour run — killed, salvaged, and still useful

The first full-length attempt was killed by host memory pressure at **52.7 of 60
minutes**. Nothing was lost that mattered: 310 samples per instance were already on
the host, and the entire verdict was recomputed without re-running.

**Accounting over 56.2 minutes of load (U1 and U3):**

| | |
|---|---|
| messages | 67,443 (33,723 + 33,720) |
| final reply | **every one `250` at the dot** — no 4xx, no 5xx, no transport failure |
| sink records | 67,444, all `delivered`, all `mismatch:false` |
| duplicate deliveries | **0** |
| ids answered 250 but never delivered | **0** — no lost mail |
| unmatched ids | 1 (`soak-app-33723`) |

The single unmatched id is the message in flight when the orphaned loadgen was
stopped by hand: the sink logged the delivery, the loadgen died before writing its
record. It is not F2's duplicate signature, which is what it would otherwise look
like.

**Return to baseline, with load stopped:** `reservations_in_flight` 0,
`quota_reserved` 0 on both routes, pool `active` 0, DB pool `in_use` 0 (8 idle),
CLOSE_WAIT 0. `app` held 22 descriptors and `app2` 23 — 16 and 17 sockets, 9 and 10
ESTABLISHED, being the DB pool plus pooled connections living out their `idle_ttl`.
Nothing stranded.

**Log hygiene (U7):** 70,840 lines, **6 WARN and no ERROR**. One is the startup
`strict_senders` notice, two are rejected admin requests (unauthenticated `curl`s
made during the investigation), and three are §7.4 `slow statement` warnings —
`INSERT INTO quota_usage … ON CONFLICT` and `UPDATE quota_usage SET reserved …`,
all within one two-second cluster. Those three are worth noting against **F4**:
`src/db.rs` sets only `acquire_timeout`, with no `lock_timeout` or
`statement_timeout`, and this shows the quota statements going slow under ordinary
soak load rather than under S3's deliberate 25-second lock.

---

## 4. What these runs do **not** establish

**No leak verdict has been produced.** `leak::verdict` needs eight post-warm-up
floors, a floor being the minimum over a five-minute window — so it takes forty
minutes of *steady state* on top of the warm-up before the slope gate means
anything. The 20-minute run yields three floors; the killed run yields about eight
and a half, right on the boundary. Both are correctly reported `inconclusive`, and
the slopes printed alongside (`+22.59 MiB/h` anon, `+30.00 threads/h`) are
start-up artefacts of a short window, not measurements. **A clean 1-hour run is
still owed.**

**`fds LEAKING` was an artefact, and is recorded here so it is not rediscovered as
a finding.** The salvaged run's analysis reported `app: fds +6.00 fds/h, quartile
step +4.00 LEAKING`. It is not a leak, on three independent grounds:

1. With load stopped, descriptors sat at 22/23 — sockets accounted for by the DB
   and downstream pools, with CLOSE_WAIT at 0. Nothing was unreclaimed.
2. The slope *grew* as the warm-up window shrank — +6.00 at 10 min, +10.00 at
   20 min, +15.50 at 30 min. A real leak's slope is stable under window choice; one
   that moves with the window **is** the window.
3. `app2` shows an identical +6.00 fds/h with an identical +4.00 quartile step —
   and `app2` is never scraped, so a curve present in both cannot be the exporter's.

Point 3 only became visible after `c554f2e`: the analyser used to `assert!` inside
its per-instance loop, so a failing `app` meant `app2` was never analysed — losing
exactly the comparison the pair exists to provide. Failures are now collected and
raised once at the end.

---

## 5. Open observation: a 6–12 second latency tail

Unexplained, reproduced on two independent stacks, and **not a correctness
defect** — every message was accepted exactly once, with no duplicates and no loss.

The distribution is bimodal rather than long-tailed. Over the 20-minute run's
12,010 messages on `app` (figures reproducible from the results volume, unlike the
killed run's — see §6):

| latency | count |
|---|---|
| < 50 ms | 11,299 |
| 50–200 ms | 581 |
| 200 ms – 1 s | 2 |
| 1–3 s | **0** |
| 3–6 s | 50 |
| **> 6 s** | **78** (min 6,028 ms, mean 7,645 ms, max 9,747 ms) |

A queueing tail decays; this one has a hole in it — two messages between 200 ms and
one second, none at all between one and three. The killed run showed the same shape
at the same proportion (239 of 33,723, 0.71%, against 78 of 12,010, 0.65%), and the
same p99 ≈ 5.1 s against a p50 ≈ 13 ms reappeared on a rebuilt stack with a fresh
configuration — which rules out the host memory pressure as a confound.

Eight explanations were proposed and each was refuted by a test:

| Hypothesis | What killed it |
|---|---|
| Downstream (connect, or the sink) | `simmer_downstream_latency_seconds`: **every message in the `le="0.05"` bucket** on both routes — warming 13.77 s over 11,409, overflow 0.82 s over 601, means of 1.21 ms and 1.36 ms |
| Pool retirement at `max_messages_per_connection` | Gaps between slow messages cluster at 13–29, nowhere near 100; retirement happens on the way back, charging the *next* checkout |
| Head-of-line blocking within a session | Slow messages are spread evenly across all ten session positions (8–19 each, no enrichment after position 0), against a uniform ~1,201 messages per position |
| D-079's 4-permit argon2 bound | Only `k == 0` pays AUTH, and with `--per-session 10` that is `id % 10 == 0`; `mod0` carries **11** of the 128 slow messages, *below* the per-position mean of 12.8 |
| A fixed timer | No repeated spacing among the >6 s events |
| §7.4 slow statements | Three WARNs in one 2-second cluster, against a tail uniform across the whole run |
| OOM / host pressure | Reproduced identically on a rebuilt stack with a fresh config |
| "1% ≈ a tenth of AUTH-paying messages" | Numerology — a ratio fitted to a story, refuted by the position partition above |

**The blocker on going further is a harness gap:** Simmer logs a `correlation_id`
per message, and the loadgen writes an `X-Test-Id`, and **nothing links them**. A
known-slow id cannot be traced through the server log, which is what forced the
work above into population inference instead of direct observation. Closing that
gap should come before T5.

---

## 6. Operational traps

Each of these cost real time during this work, and will cost it again.

**The config volume reseeds from a stale image.** `stress-config` is a one-shot
that does `rm -rf /config/* && cp -r /files/.` from *its own image*. Rebuilding
`app`, `loadgen` and `sink` does not rebuild it, so it will faithfully rewrite the
**old** configuration and exit 0. During this work it silently restored the
`*.soak.test` sender rule that V3 exists to remove; had that not been caught, every
fresh sender would have matched a rule, reached no fall-through, minted no series,
and the run would have reported "F7 did not reproduce" against a defect nobody had
touched. **Verify the config by reading it inside the container** — `app`'s startup
line reports `"senders": N`, which is the cheapest check — not by the one-shot's
exit status.

**`--ignored` hides the tier's own config test.** `tests/soak.rs` has three tests;
only `soak_run` and `soak_analyze` are `#[ignore]`. Running with `--ignored` and a
name filter silently matches nothing and reports `ok. 0 passed`, which reads as
green. `the_soak_config_is_valid_and_cannot_run_out_of_allowance` must be run
*without* `--ignored`.

**A cached `cargo clippy` reads as a pass.** Repeated runs return exit 0 in under a
second without recompiling, including immediately after an edit. Where it matters,
plant a deliberate warning and confirm the gate *fails* before trusting that it
passes.

**`soak_run` deletes the previous run's JSONL.** It begins with
`rm -f /results/soak-*.jsonl`, which is right — two runs' records interleaved in one
file would make the accounting meaningless — but it means a salvaged run's evidence
survives only until the next one starts. The host-side CSVs in `target/soak/` are
wiped the same way. **Copy both out of the volume before re-running** if the run is
one you will want to cite; §3's accounting figures are recorded here precisely
because their source no longer exists.

And one worth stating plainly: **the soak must not share a machine with heavy
builds.** The 1-hour run was killed while concurrent `cargo clippy` invocations ran
alongside two loadgens, two Simmer instances, a sink and Postgres.

---

## 7. Outstanding

- A clean 1-hour run — the shortest that yields a real leak verdict. Nothing so far
  has produced one.
- **V4**: relays cancelled by the session timeout, driving F2.
- Bursts every 15 minutes; idle gaps every 30 minutes past `idle_ttl` with the sink
  closing idle connections at 45 s (CLOSE_WAIT, F10).
- Return-to-baseline assertions, currently verified by hand (§3) rather than by the
  analyser.
- The `correlation_id` ↔ `X-Test-Id` link (§5).
- The 24-hour variant, and whether the CI runner permits a job that long.
