# The soak tier (T4) — what it does, and what it has found

**Status: V2 and V3 built and pushed (`e88c7be`, `c554f2e`, `a1b25c3`); a clean
1-hour run has produced the tier's first leak verdict — no leak on either instance
(§3a) — and F16, the latency tail that run explained, is fixed by D-080 and
confirmed by a second clean hour in which no message took longer than 264 ms
(§3b); V4 built and driving F2 in two 20-minute runs and an hour (§8) — F2 exactly
as predicted, one stranded registry entry per cut, eleven per instance an hour —
but the hour failed the threads gate on a single bounded step that the gate could
not tell from a climb (§8). Step 5c (§9) fixed that gate with a reading at rest,
made the return to baseline an assertion, and linked every message to the id Simmer
logs it under; a fresh 20-minute run passed all three, and §8's hour, re-judged,
now passes. A clean hour then passed them again, with conclusive no-leak verdicts,
and the link traced that hour's 9–16 s tail to database stalls both instances
waited out, cause on the host not established (§9). F2 has since been fixed by
D-081. V4 and stress S9 are now its regression checks, and both pass on the
stack (§8). In the planted-defect controls (§10), duplicate delivery and run B fail exactly
as planted, but run A, a 64-byte-per-message leak, was **not** caught: the
one-hour memory gate lacks the power its self-test claims. The burst/idle variants are
outstanding. The tier then ran for the first time against something other than the
Postgres build: the `mssql` build on SQL Server **Express**, with D-085's capture
on, for an hour (§11) — 36,010 messages an instance with nothing deferred or
refused, V4 and the ledger behaving exactly as on Postgres, an exact capture
accounting, and one defect found and fixed (F17, the disk gauge) plus §10's memory
limitation met again on a different backend. An hour with D-091's partial ramp
on (§12) sent 49.45% and 50.34% of the warming route's traffic to it at a share of
0.5, delivered all 36,230 messages an instance, and met the same limitation a
third time: `app2` failed `anon` at +2.84 MiB/h on a flat series, against a twin
at −3.40. The same hour with jemalloc's own counters sampled (§13) settled it:
simmer's live heap is 1.5–2.0 MiB while `anon` swings across 60, so the anon gate
has been judging the allocator. Its one real growth is F7, measured at about 204
bytes per unmatched-sender series, which D-093 now expires. With `/metrics` off
(§14), simmer's live heap was flat over 30 minutes and F7 passed for the first
time. With D-101's OTLP export on (§18), on SQL Server, the first
hour found a latency tail, F7 again on the export side and a baseline the harness
measured too early; after the fixes, 20 minutes matched the export-off control to
within 14 ms of p99.** A one-page summary is at the end of `DECISIONS.md`, "Test programme
step 5 summary". This document records what
the soak tier is, what ten runs have established, and — at least as usefully —
what they have *not* established. `tests/soak.rs` is the build; `test/config/simmer.soak.yaml` is the
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
| **V4** | the session timeout against relays in flight — F2, and since D-081 its regression check | built (§8) |
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

## 3a. The clean 1-hour run — no leak

2026-09-13, 20:05–21:05 UTC, the V3 configuration, `app`, `app2` and `sink`
re-created by `soak_run` itself (`--no-deps`, so the stale-config trap in §6 cannot
fire — `"senders": 1` confirmed on both). 10 msg/s per instance for 60 minutes.

| | `app` | `app2` |
|---|---|---|
| messages | 36,010 | 36,010 |
| accepted | **36,010** | **36,010** |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| p50 / p90 | 11.0 ms / 39.9 ms | 11.4 ms / 41.7 ms |
| p99 / max | 4,902.4 ms / 17,673.2 ms | 4,921.6 ms / 18,032.6 ms |
| peak established | 28 | 27 |
| peak CLOSE_WAIT | 0 | 1 (one sample; 0 once load stopped) |

**Accounting.** The sink holds 72,020 records, every one `delivered` and
`mismatch:false`. Joined against the loadgens' JSONL by id: **0 duplicate
deliveries, 0 ids answered `250` and never delivered, 0 deliveries with no
loadgen record.** `simmer_messages_total` shows 1,801 on `overflow-established` and
34,209 on the warming route on *each* instance — and ids `0..36009` with
`n % 20 == 0` are exactly 1,801 fresh senders.

**The leak verdict — the tier's first.** The default warm-up of 10 minutes leaves
50 minutes of steady state, ten five-minute floors against the eight
`leak::verdict` needs. `soak_analyze` passed:

| | anon | fds | threads |
|---|---|---|---|
| `app` | +0.57 MiB/h, step +0.30 MiB | −4.00 /h, step −0.50 | 0.00 /h, step −0.50 |
| `app2` | −6.08 MiB/h, step −0.66 MiB | −6.00 /h, step −0.50 | 0.00 /h, step −0.50 |

Nothing trends up on either instance, scraped or not. §4's `fds +6.00/h` did not
reappear here, which fits it having been the window.

**Return to baseline**, scraped at 21:06 with the load stopped:

| | `app` | `app2` |
|---|---|---|
| `reservations_in_flight` | 0 | 0 |
| `quota_reserved`, both routes | 0 | 0 |
| pool `active` (idle) | 0 (2) | 0 (1) |
| DB pool `in_use` (idle) | 0 (4) | 0 (4) |
| `sessions_active` / `tasks_alive` / threads | 0 / 11 / 3 | 0 / 11 / 3 |
| open fds / RSS | 21 / 25.9 MiB | 20 / 30.8 MiB |
| CLOSE_WAIT | 0 | 0 |

**F7 XFAILed as intended:** 360 series (300 unmatched-sender) at 600 s to 1,846
(1,786) at 3,572 s, across 121 post-warm-up scrapes.

**Log hygiene:** about 79,280 lines per instance, **no ERROR**. The WARNs are
3,602 `sender matched no rule` per instance (two per fresh sender, 2 × 1,801), the
startup `strict_senders` notice, and §7.4 slow statements — 44 on `app`, 34 on
`app2` plus one slow pool acquire — every one of them but a single `app` line at
21:00 inside **20:25:27–20:25:46**.

**That 19-second window was a stall of the shared database, not of Simmer.** Both
instances hit it at the same instant, with statements taking up to 11.0 s. The
replies over one second jump to about 60 in run-minute 19 against 5–9 in every
other minute, and it is where both maxima (17.7 s and 18.0 s, against 9.7 s in §2)
come from. It was not a Postgres checkpoint — those ran 20:21:49–20:22:25 and
20:26:49–20:27:33, on either side. It coincides with another session on the same
host: two `postgres:18` containers belonging to it were created at 20:26:00,
fourteen seconds after the stall ended, and its jail was at 7.4 GiB and 20% CPU
minutes later. That is circumstantial, and it is recorded as such. Correctness did
not move: nothing was deferred, refused, lost or duplicated. It does bear on F4 —
with no `statement_timeout`, an 11-second stall is simply 11 seconds of waiting —
and on §6's shared-host warning.

**§5's tail reproduced a third time.** With run-minutes 19–20 excluded, 215 of
34,812 messages on `app` took over six seconds (0.62%) and 214 on `app2` (0.61%),
with a maximum of 10.9 s — the same proportion and ceiling as §2 — and the hole
between one and three seconds is back: 3 messages on `app`, 0 on `app2`. This run
is also what explained it: the tail is every 4 MiB message (§5, F16).

## 3b. After D-080 — twenty minutes, then a clean hour

Both runs are on the fixed build, with the V3 configuration and the loadgen's
deterministic stream, exactly as §3a — the same ids carry the same bodies — so
every comparison below is like for like.

**The 20-minute run** (2026-09-13, 21:33–21:54 UTC) was the gate for the hour.
12,010 of 12,010 accepted per instance; the sink's 24,020 records all `delivered`,
`mismatch:false`, none lost and none duplicated; no ERROR; everything back at
baseline. 4 MiB bodies ran at p50 36 ms, max 85 ms. Its only outliers — 19 on
`app` and 16 on `app2` over one second, 30 of the 35 of them small messages — all
fell in one ten-second window, run-seconds 294–304 (21:39:00–21:39:10), on both
instances at once, while both logged slow statements taking up to 7.8 s: a shared
database stall again, like §3a's. The window closed as a Postgres checkpoint
finished whose sync touched 61 files in 0.160 s, where every other checkpoint in
either post-fix run touched 18–36 in at most 0.027 s and stalled nothing. §3a's
stall matched no checkpoint at all, so this is an observation, not a cause.

**The 1-hour run** (21:55–22:55 UTC), `app`, `app2` and `sink` re-created by
`soak_run` from the fixed images (image ids checked against the build), `"senders": 1`
on both:

| | `app` | `app2` |
|---|---|---|
| messages | 36,010 | 36,010 |
| accepted | **36,010** | **36,010** |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| p50 / p90 | 8.3 ms / 37.0 ms | 8.1 ms / 35.9 ms |
| p99 / max | **46.8 ms / 263.4 ms** | **44.6 ms / 252.1 ms** |
| peak established | 7 | 6 |
| peak CLOSE_WAIT | 0 | 0 |

**Accounting.** The sink holds 72,020 records, every one `delivered` and
`mismatch:false`: **0 duplicate deliveries, 0 ids answered `250` and never
delivered, 0 deliveries with no loadgen record.** `simmer_messages_total` shows
1,801 on `overflow-established` and 34,209 on the warming route per instance —
identical to §3a, as a deterministic stream should be.

**The leak verdict — no leak, a second hour running.** Default warm-up of 10
minutes, ten floors:

| | anon | fds | threads |
|---|---|---|---|
| `app` | +0.28 MiB/h, step +1.72 MiB | 0.00 /h, step 0.00 | 0.00 /h, step 0.00 |
| `app2` | +2.09 MiB/h, step −1.45 MiB | 0.00 /h, step 0.00 | 0.00 /h, step 0.00 |

Descriptors and threads are exactly flat, where §3a's drifted slightly downward.

**Return to baseline**, scraped at 22:55 with the load stopped:

| | `app` | `app2` |
|---|---|---|
| `reservations_in_flight` | 0 | 0 |
| `quota_reserved`, both routes | 0 | 0 |
| pool `active` (idle) | 0 (2) | 0 (2) |
| DB pool `in_use` (idle) | 0 (2) | 0 (3) |
| `sessions_active` / `tasks_alive` / threads | 0 / 11 / 3 | 0 / 11 / 3 |
| open fds / RSS | 19 / 28.3 MiB | 20 / 27.4 MiB |
| CLOSE_WAIT | 0 | 0 |

**F7 XFAILed as intended**, and identically to §3a: 360 series (300
unmatched-sender) at 600 s to 1,846 (1,786) at 3,572 s.

**Log hygiene:** 79,237 lines per instance, **no ERROR**, and the WARNs are only
the 3,602 `sender matched no rule` and the startup `strict_senders` notice.
**No slow statement at all** — against 44 and 34 in §3a — across twelve
checkpoints, none of which stalled anything.

**Latency by body size, before and after** (`app`; `app2` agrees to within a few
milliseconds):

| body | messages | §3a p50 | §3a max | §3b p50 | §3b p99 | §3b max |
|---|---|---|---|---|---|---|
| 4 KiB | 28,786 | 10.7 ms | 8,339.6 ms | 8.2 ms | 46.0 ms | 263.4 ms |
| 100 KiB | 5,397 | 11.1 ms | 4,908.7 ms | 8.4 ms | 46.9 ms | 108.5 ms |
| 1 MiB | 1,453 | 17.2 ms | 1,914.3 ms | 14.0 ms | 50.8 ms | 58.4 ms |
| 4 MiB | 374 | **6,579.3 ms** | **17,673.2 ms** | **35.9 ms** | 71.5 ms | **82.6 ms** |

Across the whole run on `app`: 35,821 under 50 ms, 188 at 50–200 ms, 1 between
200 ms and one second, and **none over one second** — against 235 over six
seconds in §3a.

Two things this settles beyond F16 itself:

- **§5 called the slowed small messages "likely, not measured".** With nothing
  changed but D-080, no small message took longer than 263 ms, against 21 over
  three seconds on `app` in §3a. Some of those were §3a's stall; the rest are
  consistent with having been F16's collateral, and nothing here argues otherwise.
- **Peak established connections fell from 28 to 7.** A 4 MiB session used to hold
  its connection for seconds; now it holds it for tens of milliseconds.

## 4. What the first two runs did **not** establish

**Neither produced a leak verdict** — §3a's run is the first that did. `leak::verdict` needs eight post-warm-up
floors, a floor being the minimum over a five-minute window — so it takes forty
minutes of *steady state* on top of the warm-up before the slope gate means
anything. The 20-minute run yields three floors; the killed run yields about eight
and a half, right on the boundary. Both are correctly reported `inconclusive`, and
the slopes printed alongside (`+22.59 MiB/h` anon, `+30.00 threads/h`) are
start-up artefacts of a short window, not measurements. The clean 1-hour run this
called for is §3a.

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

## 5. The 6–12 second latency tail — explained after §3a: finding F16

Reproduced on three stacks, and **not a correctness defect** — every message was
accepted exactly once, with no duplicates and no loss. The cause is at the end of
this section; what comes before it is kept as written, because it records what was
ruled out and why.

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

### The cause: every 4 MiB message — about 2.35 s per MiB past the spill threshold (F16)

The harness gap turned out not to be the blocker. §3a's two instances were slow on
the **same message ids** — 232 of the 235 over six seconds, 148 of the 160 between
three and six — and two independent processes agree like that only when the cause
is the input. The loadgen picks each body size deterministically from the id
(`pick_size`, SplitMix64 over seed 1), so the sizes can be recomputed offline. On
`app` (`app2` agrees to within a few messages):

| body | share | p50 | min | over 3 s |
|---|---|---|---|---|
| 4 KiB | 79.9% | 10.7 ms | 6.2 ms | 17 |
| 100 KiB | 15.0% | 11.1 ms | 6.5 ms | 4 |
| 1 MiB | 4.0% | 17.2 ms | 10.8 ms | 0 |
| 4 MiB | 1.0% | 6,579 ms | 4,569 ms | **374 of 374** |

Every 4 MiB message is slow, and beyond about twenty small ones per instance nothing
else is. That is the bimodal shape and the one per cent in the table above. A direct
probe on the idle stack — three messages per size, one at a time, so no queue is
involved — draws the curve:

| body | 1 MiB | 2 MiB | 3 MiB | 4 MiB | 5 MiB |
|---|---|---|---|---|---|
| p50 | 45 ms | 2,430 ms | 4,747 ms | 7,131 ms | 8,772 ms |

Flat below §8.1's 1 MiB `SPILL_THRESHOLD`, then linear at about **2.35 s per MiB**
above it. The mechanism is in `src/smtp/buffer.rs`: once spilled, the buffer is a
bare `tokio::fs::File`, and `read_data_inner` in `session.rs` calls `append` twice
per line — the content, then `\r\n`. Each append is its own `write_all`, and tokio
performs every file write as a separate blocking-pool job. At 78-byte lines that is
about 27,000 round trips per MiB; 2.35 s over 27,000 is about 87 µs each.

Why the eight hypotheses missed it: the downstream really was fast — the time is
spent *receiving*, before the relay begins — and every partition tried (session
position, AUTH, timing) was orthogonal to body size.

**What it costs in production.** A 76-column base64 attachment has the same line
shape as the loadgen's filler, so the shipped 25 MiB `max_message_bytes` means
roughly 56 s of `DATA` for a maximal message, and 7 s for an ordinary 4 MiB one.
Each of those writes occupies a blocking-pool thread, which is the likely reason the
small messages beside a large one were slowed as well — likely, not measured.

**Fixed by D-080:** the spill file's writes are batched at 64 KiB and the last
batch is written at the terminating dot. The same probe afterwards gives 48, 55,
57, 66 and 74 ms for 1–5 MiB.

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

**`app` was scraped through a port the harness may not be able to reach.** The
scraper `curl`ed `127.0.0.1:8080`, which is the Docker host's loopback. From
anywhere else — the jail these runs are driven from, or a CI container — every
scrape failed, and a failed scrape was just a sample not written. V4's first
20-minute run is how it was found: no `metrics.csv` at all, so `app` went
unscraped, V2's asymmetry was absent, and F7 printed "too few scrapes" instead of
a verdict. Nothing failed. `app` is now scraped from inside its container, like
the final scrapes, and `soak_analyze` fails a run in which `app` was never
scraped. §3a's and §3b's runs were scraped — their `metrics.csv` rows survive in
the accumulated file described next — so their V2 comparison and F7 figures stand.

**`metrics.csv` used to outlive its run.** `soak_run` deleted the instances'
CSVs and not the scrape file, so every run appended to the last and its clock
restarted at zero. Found while building V4: the file held four runs. The F7 verdict
took its baseline from the first post-warm-up row in the file — the *oldest*
run's — and its end from the newest. The stream is deterministic, so that row was
identical to the right one (360 series, 300 unmatched-sender, at 600 s) and the
figures in §3a and §3b stand. Only §3b's count of 121 post-warm-up scrapes spans
more than one run. `soak_run` now deletes every file it writes.

**`soak_run` deletes the previous run's JSONL.** It begins with
`rm -f /results/soak-*.jsonl`, which is right — two runs' records interleaved in one
file would make the accounting meaningless — but it means a salvaged run's evidence
survives only until the next one starts. The host-side CSVs in `target/soak/` are
wiped the same way. **Copy both out of the volume before re-running** if the run is
one you will want to cite; §3's accounting figures are recorded here precisely
because their source no longer exists.

And one worth stating plainly: **the soak must not share a machine with heavy
builds.** The 1-hour run was killed while concurrent `cargo clippy` invocations ran
alongside two loadgens, two Simmer instances, a sink and Postgres. Other sessions
share the host as well, and nothing in the harness can stop them: §3a's
19-second database stall is the likely cost of one.

**Stopping `soak_run` does not stop its loadgens.** The loadgens are
`docker compose run -d` containers. Killing the test process leaves them sending
for the rest of their `--duration`, into the next run's sink and quota rows. §8's
F2 re-run found 48 phantom messages per instance that way, and a ledger 97 over.
After an interrupted run, check `docker ps` for loadgen containers and stop them
before starting another.

**The loadgen containers report `unhealthy`, and it means nothing.** The
`acceptance` stage is `FROM runtime`, so it inherits the server's `HEALTHCHECK`
(`/app/server healthcheck`, probing 8080), which a loadgen never serves. The
samples and the JSONL are the evidence that a sender is alive, not the health
column.

---

## 7. Outstanding

- ~~A clean 1-hour run~~ — done, §3a: no leak on either instance.
- ~~**V4**: relays cancelled by the session timeout, driving F2~~ — built, §8.
- Bursts every 15 minutes; idle gaps every 30 minutes past `idle_ttl` with the sink
  closing idle connections at 45 s (CLOSE_WAIT, F10).
- ~~Return-to-baseline assertions, currently verified by hand (§3, §3a, §3b)
  rather than by the analyser~~ — `soak/rest/baseline`, step 5c (§9).
- ~~**The threads gate fails a bounded step** (§8's hour)~~ — step 5c (§9): a
  trend failure is cleared when the count is back at its baseline at rest. Thread
  counts are integers, so one +1 held from mid-window to the end clears both the
  slope limit and the quartile step; the count at rest is what tells a ratchet
  from a climb, and no limit was raised.
- ~~**F16**: buffer the §8.1 spill file's writes~~ — fixed by D-080 (§5).
- ~~The `correlation_id` ↔ `X-Test-Id` link~~ — step 5c (§9): every soak route
  stamps `X-Simmer-Correlation` and the sink records it.
- The 24-hour variant, and whether the CI runner permits a job that long.
- ~~The tier against the `mssql` build, and against the capture~~ — both, §11.
- **The capture under eviction**: `retention` shorter than a run, so the sweeper
  actually deletes and the disk gauge's increments are tested across an eviction
  (§11).
- **T3 against the `mssql` build.** The soak is 10 msg/s; the stress tier has
  never been pointed at SQL Server at all.
- Gating the capture record count against messages accepted. The invariant held
  exactly on all three §11 runs (7,250, 36,230 and 7,250 records, each equal to
  the messages answered `250`); it is a check waiting to be written.

---

## 8. V4 — relays cancelled by the session timeout (F2)

*F2 was fixed by D-081 after the runs below. The session deadline now caps waits
on the client and never a relay, so V4 checks that it stays fixed: `driven` means
the timeout fired in the sessions, and a relay cut at the dot fails `accounting`.
What follows is the record of the defect as it was.*

S9 shows F2 exists: `timeouts.session` runs from connect, and when it fires during
the downstream conversation the relay future is simply dropped. V4 asks what that
costs over hours, and the code says one part of it is never given back. A
reservation leaves the §10.4 registry only by commit, release or the shutdown
`drain()`; the sweeper deletes the database row at `expires_at` and never touches
the registry. So `simmer_reservations_in_flight` should climb by one per
cancellation for the life of the process, while the rows come and go.

**How it is driven.** A third stream per instance, identical on both so V2's pair
stays a pair, on a sender (`cancel.soak.test`) and a warming route
(`warming-cancel`) of its own — its own pool and its own quota row, so V2's route
and V3's series count are untouched. One client, sessions of 25 small messages
back to back, each carrying `X-Sink-Script: slow@dot:15s`: a session is almost
entirely relays in flight, so the 300 s session timeout lands inside one about
once a session, roughly twelve an hour per instance. `slow@dot` rather than
`stall@dot` because the sink records a stall as `stalled_at_dot` even when it
answers in time, and reconcile would count every V4 message as ambiguous; `slow`
records `delivered` after the hold, so only the cancelled message breaks a rule.
The stream stops one session timeout before the run, so its last session is cut
inside it. `SOAK_V4=off` leaves V4 out.

The config test pins the timing it rests on, because each way of getting it wrong
stops the variant cancelling anything while it still appears to run: the 15 s hold
inside the route's 60 s data timeout (past it, the message takes §10.2's ambiguous
path and F2 looks fixed) and the loadgen's 30 s reply wait; 25 × 15 s beyond the
session timeout; and expiry plus one sweep (265 s) inside it, so a stranded row is
gone before the next session can strand another.

**What `soak_run` adds.** `app`'s 30 s scrape records `reservations_in_flight`; a
database sampler records the route's reservation rows, `reserved` and
`committed` every 30 s to `v4.csv` — the one view that includes `app2` without
scraping it. Once the load stops it waits for the sweeper to clear the rows, then
takes each instance's final `/metrics` (`final-<instance>.prom`, the only scrape
`app2` ever gets) and copies V4's loadgen records and the sink's V4 lines to
`target/soak/`, so the analyser needs nothing but the host. About five minutes on
top of the run.

**What `soak_analyze` judges:**

| Check | Today | Rule |
|---|---|---|
| `soak/V4/driven` | must pass | relays cut at the dot in at least half the sessions, and at least one. Zero fails outright, so "F2 did not reproduce" cannot be read as "fixed" |
| `soak/V4/delivery` | must pass | every other V4 message `250` and stored once; no phantom, duplicate or crossed envelope — kept apart so the XFAILs below cannot hide a regression |
| `soak/V4/accounting` | **XFAIL F2** | no message stored by the sink and answered `4xx`; the route's `committed` equals what the sink stored |
| `soak/V4/registry` | **XFAIL F2** | `reservations_in_flight` 0 on each instance after the drain |
| `soak/V4/sweeper` | must pass | at most 6 reservation rows at any sample (one in flight and one awaiting the sweeper per instance, and a spare each for a sweep delayed by a database stall); rows and `reserved` 0 after the drain |

With nothing cancelled, the two F2 checks would pass for want of a cancellation
and report a meaningless XPASS, so they are judged only when `driven` passes. The
existing fds and threads gates gain a use as well: at twelve cancellations an hour,
a socket leaked per cancellation would be +12 fds/h against the 1/h limit.

### The first 20-minute run — F2 driven, and a scraping hole found

2026-09-14, 10:25–10:46 UTC, `app`, `app2` and `sink` re-created by `soak_run`
from the D-080 images (ids checked against the build), `"senders": 2` on both.

**V2 and V3's streams were untouched:** 12,010 of 12,010 accepted per instance,
p50 11.1 / 10.3 ms, p99 55.8 / 57.7 ms, max 433 / 393 ms, no ERROR on either.

**V4 did what it is for.** Per instance, 75 messages in 3 sessions: 57 accepted,
and 3 cut by the session timeout — every one **at the dot, and every one stored by
the sink** — plus the 15 the closed connection never let it send. The first cut
landed at 10:30:58, 300 s into `app`'s first session, and `reservations_in_flight`
stepped from 1 to 2 at that instant: the stranded entry plus the next message's.

| | result |
|---|---|
| `driven` | pass — 3 of 3 sessions cut at the dot, per instance |
| `delivery` | pass — no other refusal, no phantom, no duplicate |
| `accounting` | **XFAIL F2** — 3 of 3 cuts per instance stored and told `421`; the ledger committed **114 of the 120** stored, short by exactly the 6 cuts |
| `registry` | **XFAIL F2** — `reservations_in_flight` **3 on each instance after the drain, one per cut** |
| `sweeper` | pass — at most 4 rows (two in flight, two stranded), 0 rows and 0 reserved after the drain; 6 expired |

So S9's reading holds up and sharpens: the database side of F2 is bounded — the
sweeper cleared every stranded row within one cycle — and the in-memory side is
not. The registry held one entry per cancellation, none of them ever reclaimed.

The sweeper's own warning is worth noting against F2: `app` logged three
`released an expired reservation; the process either died mid-send or the
reservation expiry is shorter than real downstream latency`. Neither is true
here. The warning names the two causes §7.4 anticipated, and F2 is a third.

**What it did not measure:** `app` was never scraped (§6), so there is no V2
asymmetry, no F7 verdict and no registry curve from this run. `soak_analyze` now
fails a run like it.

### The second 20-minute run — everything measured

10:48–11:09 UTC, the scraper fixed and nothing else changed; images and
`"senders": 2` checked as before, and `metrics.csv` confirmed filling within the
first minute.

**V2/V3:** 12,010 of 12,010 accepted per instance, p99 55.1 / 56.6 ms, max 284 /
248 ms, no ERROR. **F7 XFAILed** on 20 post-warm-up scrapes: 302 → 589
unmatched-sender series in ten minutes.

**V4 reproduced the first run exactly** — 3 of 3 sessions cut at the dot per
instance, every cut stored, the ledger 114 of 120, 3 left in each registry, at
most 4 rows, all swept — and this time `app`'s scrape shows the registry as it
happens:

| run-second | 91 | 242 | 393 | 544 | 695 | 846 | 997 | 1148 |
|---|---|---|---|---|---|---|---|---|
| `reservations_in_flight` | 1 | 1 | 2 | 2 | 3 | 3 | 4 | 4 |

One live reservation throughout, plus one more stranded every 300 s that never
comes back: a staircase, not a sawtooth. Over an hour that is about twelve per
instance; over 24 hours about 280, each a small `HashMap` entry — trivial as
memory, and a gauge that says reservations are in flight when none are. That is
the part of F2 only a soak shows: §10.4's registry was written for the shutdown
case and nothing prunes it in between.

Leak slopes are `inconclusive` at twenty minutes, as they must be (§4).

### The 1-hour run — F2 as predicted, and the threads gate trips

2026-09-14, relays from 11:27 to 12:23 UTC by the containers' clock; `app`, `app2`
and `sink` re-created by `soak_run` from the D-080 images (ids checked against the
build), `"senders": 2` on both. **`soak_analyze` failed it on one gate:** `app2:
threads is growing at +1.50 threads/h with a quartile step of +1.00`. Everything V4
exists to measure came out as predicted; the failure is taken apart below, and the
reading here is that the gate, not the server, is what needs work.

| | `app` | `app2` |
|---|---|---|
| messages | 36,010 | 36,010 |
| accepted | **36,010** | **36,010** |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| p50 / p90 | 10.7 ms / 40.2 ms | 10.0 ms / 39.2 ms |
| p99 / max | 63.5 ms / 537.9 ms | 64.9 ms / 500.0 ms |
| peak established | 10 | 10 |
| peak CLOSE_WAIT | 0 | 0 |

**Accounting.** The sink holds 72,460 records, every one `delivered` and
`mismatch:false`, no id twice: 36,010 per V2 instance and 220 per V4 instance.
`simmer_messages_total` shows 1,801 on `overflow-established` and 34,209 on
`warming-newbrand` per instance — identical to §3a and §3b — and 209 on
`warming-cancel`.

**V4 — eleven cuts per instance, the registry equal to the cuts.** Per instance,
275 messages in 11 sessions: 209 accepted, 11 cut by the session timeout, 55 never
sent on the closed connection.

| | result |
|---|---|
| `driven` | pass — 11 of 11 sessions cut at the dot, per instance |
| `delivery` | pass — no other refusal, no phantom, no duplicate |
| `accounting` | **XFAIL F2** — 11 of 11 cuts per instance stored and told `421`; the ledger committed **418 of the 440** stored, short by exactly the 22 cuts |
| `registry` | **XFAIL F2** — `reservations_in_flight` **11 on each instance after the drain**, against 11 cuts |
| `sweeper` | pass — at most 4 rows across 121 samples, 0 rows and 0 reserved after the drain; 22 expired |

`app`'s registry, as the lowest value its scrape saw in each five minutes:

| run-seconds | 0– | 300– | 600– | 900– | 1200– | 1500– | 1800– | 2100– | 2400– | 2700– | 3000– | 3300–3600 | drained |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `reservations_in_flight` | 0 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 11 | 11 |

Until V4's stream stops at 3,300 s the floor is the cuts so far plus V4's one live
reservation, exactly; after it, the eleven stranded entries alone, and that is what
the drain leaves. The second 20-minute run's staircase, carried to an hour without
a single step coming back.

All 22 stranded rows were released by `app`'s sweeper, two per sweep — one from
each instance — so `app` logged the 11 `released an expired reservation` WARNs and
`app2` none. The rows are shared; whichever sweeper runs first takes them.

**The leak verdict.** Ten floors after the 10-minute warm-up:

| | anon | fds | threads |
|---|---|---|---|
| `app` | −0.45 MiB/h, step −0.73 MiB | 0.00 /h, step −1.00 | 0.00 /h, step +1.00 |
| `app2` | +0.44 MiB/h, step +0.86 MiB | −2.40 /h, step −2.00 | **+1.50 /h, step +1.00 — LEAKING** |

Memory and descriptors: no leak. A socket leaked per cancellation would have been
+11 fds/h; descriptors fell.

Threads: one step, on both instances. Each sat at 5 through the load, as in §3a
and §3b, then gained one — `app` at run-second 1,465, `app2` at 1,968 — kept it
until the load stopped, and was back at 3 at rest (`/proc/1/status` read after the
drain: 3 on both). Neither ever reached 7. `app` passed only because its step
fell early enough to leave its Theil–Sen slope at 0; `app2`'s fell nearer the
middle of the window, which reads as +1.5/h against the 1/h limit, and a quartile
step of +1 against a step gate of about 0.3. Integer counts make that inevitable:
any single +1 held from mid-window to the end clears both halves of the rule.

The step is V4's — the pre-V4 hour (§3b) never went above 5 on either instance,
and `app2` took the same step in both V4 20-minute runs (at 952 s and 932 s),
where the verdict was inconclusive and so said nothing. Nothing is logged at
either moment but routine relays. The likeliest reading, **not established**, is
tokio's blocking pool: a blocking thread exits after 10 s idle, so the pool keeps
as many threads as its peak concurrent jobs for as long as the work reaches each
one within that; its jobs here are argon2 verifies (up to 4 at once, D-079) and
spill-file writes. Which job V4 adds to it is not known. Bounded by a peak and
released at idle is a ratchet, not a leak — but the gate cannot tell the two
apart, and that is the harness's to fix (§7), not something to argue away one run
at a time.

**Return to baseline**, from `final-*.prom` after the drain:

| | `app` | `app2` |
|---|---|---|
| `reservations_in_flight` | **11** (F2) | **11** (F2) |
| `quota_reserved`, all three routes | 0 | 0 |
| pool `active` (idle) | 0 (2) | 0 (2) |
| DB pool `in_use` (idle) | 0 (3) | 0 (4) |
| `sessions_active` / `tasks_alive` / threads | 0 / 11 / 3 | 0 / 11 / 3 |
| open fds / RSS | 20 / 20.9 MiB | 21 / 25.7 MiB |

`tasks_alive` is §3b's 11: a stranded registry entry holds no task.

**F7 XFAILed as intended:** 386 series (302 unmatched-sender) at 605 s to 1,883
(1,798) at 3,596 s, against §3b's 360 to 1,846 — V4's route adds its own.

**Log hygiene:** 79,688 and 79,677 lines, **no ERROR**, no slow statement. The
WARNs are the 3,602 `sender matched no rule` and the `strict_senders` notice on
each, plus `app`'s 11 sweeper lines (F2 — the text blames a crash or a short
expiry, and neither applies, as in the 20-minute runs).

**Latency.** Worse in the tail than §3b, and the cause is not found. p99 rose
from about 46 to 64 ms and the maximum from 263 to 538 ms; 21 messages on `app`
and 25 on `app2` took over 200 ms, none over one second. The outliers fall at the
same run-seconds on both instances — 1,937–1,951, 2,030–2,034, 2,086, 2,106,
2,494, 2,960, 3,014, 3,075 — so it is something shared, not per instance; none
is at a V4 cut (multiples of 300 s) and no statement was slow. Peak established
connections rose from 7 to 10.

One trap for whoever correlates these: the containers' clock ran about 7% slow
against the loadgens' monotonic one — 3,355 s from first relay to last over a
3,600 s load, and the sweeper's WARNs 279.6 s apart. Run-seconds cannot be turned
into log timestamps by addition on this host.

---

### F2 fixed — the first runs against D-081, and what they found

2026-09-15, on images built from `597b4a8` (D-081 and D-082).

**V4, 20 minutes, 12:35–12:55 UTC.** On everything F2 is about, it passed:

- No relay was cut. `reservations_in_flight` was 0 throughout, on both instances.
- The warming-cancel ledger committed 120, exactly the 60 + 60 messages accepted,
  and the sweeper expired nothing.
- On the main load, 12,010 of 12,010 were accepted on each instance, and both came
  back to baseline at rest.

It **failed `soak/V4/driven`**: "the session timeout fired in 0 of 3 sessions".
Each session was 20 accepted messages, then five that never got a reply. The 21st
was recorded as `transport: write: Broken pipe`, and the rest as `not_sent`. Two
things combined:

- **Simmer, at the boundary.** The first build refused unprompted. The moment the
  20th relay finished, past the 300 s deadline, it sent `250`, then `421`, and
  closed the socket. The client, already starting its next message, wrote into a
  socket that had just been closed, and got a broken pipe instead of the `421`.
  D-081 now waits up to 2 s for the next command and answers that command `421`.
- **The loadgen.** Between messages it writes `RSET`, and it read the reply
  without looking at the code. Even with the refusal answering `RSET`, it would
  have written `MAIL FROM` into the closed connection and recorded a transport
  failure. It now records a non-`250` reply to `RSET` with its code and stage
  `rset`, and ends the session there, like any other refusal.

**Neither shows up on loopback.** A V4-shaped test in `tests/loadgen_sink.rs`,
with the real loadgen and sink, a 2 s session and a 1 s hold at the dot, passes
with the grace disabled: there, only the loadgen fix is needed. So the grace is
proven by V4 on the stack, and nowhere else.

**Stress S8a** passed every check, with peak anon at 5.5 MiB against a 96 MiB
bound. That confirms D-082 on the stack.

**Stress S9 took three attempts, and none of the failures was F2:**

1. **Setup failed:** "the sink never answered its stats". The harness addressed
   the stack's published ports on `127.0.0.1`, and the jail these runs are driven
   from cannot reach them: the same trap as §6's `app` scraping. Two variables now
   override those addresses, `SIMMER_TEST_ADMIN` and `SIMMER_TEST_SINK_STATS`.
   They default to the loopback, and from the jail they name the containers on the
   Docker network.
2. **With a 30 s hold at the dot**, the relays were no longer cut at the 20 s
   deadline. They finished with `250` at 30,003 ms, milliseconds after the
   loadgen's own 30 s wait had expired. `bare-close` and `accounting` failed, and
   the sink had four messages stored that no client heard about. The scenario
   itself broke the README's "Timeout budget" rule. Its hold is now 25 s, and
   D-081 records the lesson.
3. **With a 25 s hold,** only `accounting` failed. The reconciler counted every
   stalled message as ambiguous, which was true in S9 only while the old code cut
   them all. Simmer answers a client `2xx` only after the downstream's own `2xx`,
   so a stall whose client was told `2xx` had its late reply seen in time, and is
   no longer counted. A stall answered `4xx` still is. A self-test pins both.

**V4 on the final build, 13:15–13:35 UTC, detached: clean for Simmer, spoiled by
its own harness.** For this run's own traffic, D-081 behaved exactly as intended
on both instances:

- There were 3 sessions, and each was ended by `421 4.4.2 session timeout` in
  answer to the client's `RSET` (stage `rset`), with the rest of the session
  `not_sent`.
- No relay was cut at the dot, and nothing was stored without a reply.
- `reservations_in_flight` was 0 after the drain, and `driven` passed.

Two checks failed, both on traffic that was not this run's:

- `delivery` found 48 phantom V4 messages per instance: ids 9017–9069 and
  5792–5844, where this run's were 0–74.
- `accounting` found the ledger at 217, against the 120 the sink stored for this
  run. That is the same 96 messages, plus one.

They came from the first attempt's V4 loadgens. That attempt was stopped after
four minutes so it could be relaunched detached. Stopping it killed `soak_run`,
but not the loadgen containers `soak_run` had started with `docker compose run -d`
(§6). Those went on sending their 15-minute stream into the same sink and the same
quota row until about 13:27. A third run, with no loadgen container left running,
gives the verdict.

**V4 run 3, 13:37–13:57 UTC: passed.** It used the same images, with no loadgen
left over, and every check passed on both instances:

| | `app` | `app2` |
|---|---|---|
| V4 messages | 75, in 3 sessions | 75, in 3 sessions |
| `250` at the dot | 60 | 60 |
| `421 session timeout` at `rset` | 3, one per session | 3 |
| relays cut at the dot | 0 | 0 |
| stored without a reply | 0 | 0 |
| `reservations_in_flight` after the drain | 0 | 0 |
| main load accepted | 12,010 of 12,010 | 12,010 of 12,010 |
| threads and tasks, before → at rest | 3 → 3, 11 → 11 | 3 → 3, 11 → 11 |

The warming-cancel ledger committed 120, exactly the messages stored, and the
sweeper expired nothing. F7 XFAILed as always. `app`'s last scrape during the load
read a `reservations_in_flight` of 1, which was a relay in flight at that moment;
after the drain it was 0. F2 is fixed, and V4 now checks that on every run.

---

## 9. Step 5c — the return to baseline asserted, and every message traceable

Three changes to the harness — `tests/soak.rs`, `tests/compose/leak.rs`, the sink
and the soak config — and none to the server.

**The threads gate reads the count at rest.** `leak::Verdict::released_at_rest`
clears a failing thread or descriptor trend when the count after the run is no
higher than before the first message. "After the run" is the load stopped, V4
drained, and `REST_SETTLE` (30 s, three times the blocking pool's idle keep-alive)
waited out. A ratchet gives its count back at idle; a leaked thread does not.
Memory is not judged this way, because an allocator keeps what it has grown. The
self-tests pin both halves: the shape of §8's hour (5 threads, then 6 from 1,968 s)
fails the trend alone and is released at rest, and a climb still held at rest is
not. Re-judged from its files, §8's hour now reads `app2 threads … a ratchet: the
trend rose, and it was back at its baseline at rest` (3 before, 3 at rest) and
passes, with F7 and both F2 checks XFAIL as before.

**The return to baseline is an assertion**, `soak/rest/baseline`, where §3, §3a
and §3b read it by hand:

- From the final scrape: `sessions_active`, every `quota_reserved`, every pool's
  `active` and the database pool's `in_use` are 0. A family with no series at all
  fails rather than passing for want of anything to read.
- Threads and `tasks_alive` are no higher than before the first message.
- No descriptor is open that was not open before and that no pool holds.
  Descriptors are compared **by kind**, from `ls -l /proc/1/fd` before and after:
  anything but a socket as a multiset, sockets by count against what the
  downstream and database pools report holding (`leak::unaccounted_fds`). The
  self-tests plant one leaked file, one socket no pool holds and a second eventfd,
  and each is named.
- `reservations_in_flight` is left to `soak/V4/registry`, so F2 is not counted
  twice.

`app2` is now scraped once, before its first message, for its baseline gauges. The
exporter holds nothing then for a scrape to drain, so V2's asymmetry (F8) is
untouched.

**Every message carries the id Simmer logged it under.** The soak routes stamp
`X-Simmer-Correlation: "{{correlation_id}}"` and the sink records it. It is not in
`unstable_headers`: §6.6's probe pins volatile variables, so the header is stable,
and declaring it would draw the stale-declaration WARN — the config test asserts
there is none. `soak_run` copies out every V2/V3 message over 200 ms with its sink
record, and `soak_analyze` prints the slowest with their correlation ids.

### The 20-minute run

2026-09-15, 09:37–09:57 UTC, on images freshly built from `83a8c68` plus these
changes. The config was verified in the volume (the header on all three routes)
and by `"senders": 2` in both instances' startup lines (§6's trap).

| | `app` | `app2` |
|---|---|---|
| accepted | 12,010 of 12,010 | 12,010 of 12,010 |
| p50 / p99 / max | 8.4 / 48.5 / 89.9 ms | 8.7 / 51.5 / 90.5 ms |
| threads, before → at rest | 3 → 3 | 3 → 3 |
| `tasks_alive`, before → at rest | 11 → 11 | 11 → 11 |
| descriptors, before → at rest | 15 → 19 | 15 → 19 |
| of which sockets, and what the pools hold | 9 and 2 → 13 and 6 | 9 and 2 → 13 and 6 |
| unaccounted descriptors | 0 | 0 |

`soak/rest/baseline` passed. The leak verdicts were inconclusive, as a 20-minute
run's must be. F7 and both F2 checks XFAILed — three cuts per instance and
`reservations_in_flight` 3 after the drain on each. The four new descriptors on
each instance are exactly the four sockets the pools gained; every other kind is
identical before and after.

**The link, end to end.** All 24,140 sink records carry a correlation id; none is
null. No message took over 200 ms, so the slow report printed nothing, and the link
was checked by hand instead: `soak-app-5000` in the sink gives `1f5e7802-…`, and
`app`'s log has its `relaying` line and its `downstream accepted the message` line
(`overflow-established`, `latency_ms: 1`).

### What this run did not establish

- **The link has not yet traced a slow message.** §8's 538 ms tail did not recur —
  this run's maximum was 90 ms — so the report is proven only on a run with nothing
  to report. The next hour will exercise it.
- **The at-rest checks have one clean run behind them.** A task started lazily, or
  a descriptor opened on first use and kept, would fail `soak/rest/baseline`
  without being a leak. Neither happened here; a second run is the confirmation.
- **The checks were checked on synthetic series and listings only.** The
  programme's planted-defect controls on a throwaway branch — a leaked task per
  message, one leaked descriptor — are still to be run against a live stack.

### The hour

2026-09-15, 10:09:46–11:07:26 UTC, on the same images as the 20-minute run (built
from `d74abc0`). The config was verified in the volume (the header on all three
routes) and by `"senders": 2` in both startup lines. Nothing else was built on the
host while it ran.

| | `app` | `app2` |
|---|---|---|
| accepted | 36,010 of 36,010 | 36,010 of 36,010 |
| p50 / p90 / p99 / max | 11.5 / 41.5 / 152.3 / 12,316 ms | 12.0 / 43.9 / 141.9 / 16,007 ms |
| anon slope, quartile step | −5.20 MiB/h, −3.63 MiB | −3.35 MiB/h, −2.51 MiB |
| descriptors, threads slope | −12.00 /h, 0 /h | −12.00 /h, 0 /h |
| threads, before → at rest | 3 → 3 | 3 → 3 |
| `tasks_alive`, before → at rest | 11 → 11 | 11 → 11 |
| descriptors, before → at rest | 15 → 19 | 15 → 19 |
| of which sockets | 9 → 13 | 9 → 13 |
| unaccounted descriptors | 0 | 0 |
| peak ESTABLISHED / CLOSE_WAIT | 30 / 0 | 30 / 0 |
| log lines, ERROR | 79,744, 0 | 79,755, 0 |

**No leak, conclusively, and the return to baseline asserted a second time.**
Memory, descriptors and threads are flat on both instances after a 10-minute
warm-up. `soak/rest/baseline` passed. As in the 20-minute run, the four new
descriptors per instance are sockets the pools hold, and every other kind is
identical before and after. F7 XFAILed: unmatched-sender series went from 304 to
1,800 over the 50 post-warm-up minutes. Both F2 checks XFAILed, exactly as §8's
hour did. Each instance had 11 relays cut by the session timeout, all 11 stored by
the sink and told `421`, and `reservations_in_flight` stood at 11 after the drain.
The warming-cancel ledger committed 418 of the 440 V4 messages the sink stored,
and the sweeper expired 22 reservations.

**The tail, traced: the link's first real use.** 282 messages on `app` and 285 on
`app2` took over 200 ms, the slowest 12.3 s and 16.0 s. This is not F16 returning.
Two greps settled where the time went:

- `soak-app-2390` (12,316 ms) is `c8392643-…`. `app` logs its `relaying` at
  10:14:04.516, and `downstream accepted the message` 144 ms later.
- `soak-app2-2410` (16,007 ms) is `7af87612-…`. The downstream accepted it in 1 ms.

So the time was spent before the relay began. The loadgen times each message from
its *scheduled* instant, so a stall anywhere upstream is charged to every message
queued behind it.

The slow messages come in bursts, and in the same minutes of the load on both
instances:

| minute of the load | 3 | 4 | 18 | 19 | 21 | 27 | 30 | 33 |
|---|---|---|---|---|---|---|---|---|
| `app` over 200 ms | 15 | 181 | 1 | 37 | 8 | 33 | 1 | 6 |
| `app2` over 200 ms | 21 | 170 | 1 | 40 | 12 | 34 | 1 | 6 |

Both instances log §7.4 slow statements against the shared database in three
clusters, counting slow pool acquires with them: 10:13–10:14 (53 on `app`, 51 on
`app2`), 10:28 (8 and 10) and 10:36 (6 and 6). They are `COMMIT`s of 2–5 s, and the
quota `INSERT` and commit `UPDATE`, the worst 9.1 s on `app` and 15.4 s on `app2`.
15 of them are pool acquires of over 2 s. The clusters are spaced like the three large bursts (15 minutes, then
8), and the traced message relayed at 10:14:04, inside the first. Don't convert
the loadgen's `sent_ms` to wall-clock by arithmetic to match them, because the
loadgen's clock is its own (§8). The instances share only the database, the host
and a sink that answered in milliseconds. So the tail is **database stalls,
waited out**. With no `statement_timeout` (F4) a stall is simply waited for, on
every message queued behind it. The bursts at minutes 21 and 33 have no slow
statement at all, so those stalls stayed under the 1 s threshold.

**What stalled the database is not established.** It is not the checkpointer.
Postgres checkpointed every five minutes all hour, each checkpoint's writes spread
over 45–115 s and every sync 0.061 s or less. The stalls do not follow the
checkpoints: 10:36 falls between two, and six other checkpoints had none. The host
is shared, with about twenty other database containers on it. One of them,
`vehicle-data-service-db`, went through crash recovery at 10:18:48 and shut down at
10:29:56, inside the hour. So §6's warning is the likely explanation, but
`docker events` kept no history for the window, and "likely" is as far as the
evidence goes. §8's 538 ms tail, "at the same moments on both instances", has the
same shape at a smaller size. It is probably the same mechanism, but its logs
predate the link, so it has not been re-checked.

### What the hour did not establish

- **The cause of the stalls.** Showing it needs evidence the harness does not
  collect: host I/O pressure (`/proc/pressure/io`) in the sampler, and the
  database's wait events while a statement is slow.
- **The planted-defect controls against a live stack.** They are still to run.
  The 20-minute run's other two open points are closed: the link has traced a slow
  message, and the at-rest checks now have a second clean run behind them.

---

## 10. Checking the checks — the planted-defect controls

A check that has never failed has not shown it can. The test programme's
Verification asks for each check to be seen failing on the defect it exists for. Two
already had been: the CA negative control (`tests/acceptance.rs`) and planted loss
(`--lose-every`, in `tests/loadgen_sink.rs` and stress). Three more follow. Runs B
and A were planted on a throwaway branch that was never pushed and built as the
soak's images.

**Duplicate delivery fails U3.** `sink --duplicate-every N` records every Nth
delivery twice. `a_sink_that_stores_mail_twice_is_caught_end_to_end` sends 200
messages with N = 20. It passes only on exactly ten `stored 2 times (duplicate
delivery)` violations, nothing else, and all 200 accepted, and it does pass. It is in
`cargo test` and needs no Docker.

**Run B — a leaked task per message and one leaked descriptor fail
`soak/rest/baseline`.** The plant: `tokio::spawn(std::future::pending::<()>())`
after every relay's commit or release, and `/proc/self/stat` opened and forgotten
once per process. It ran on 2026-09-15, 11:13–11:19 UTC, for five minutes with V4
off. Both containers ran the planted image, `cc3171f78421`, per `docker inspect`.
Exactly one check failed, and it failed on both plants, on both instances:

> soak/rest/baseline failed: app: 3021 tasks alive at rest, against 11 before the
> first message; app: 1 descriptors open at rest that were not open before the first
> message and that no pool holds: /proc/1/stat; app2: *the same*

3,021 − 11 = 3,010: one task per message relayed. Nothing else failed. Threads went
3 → 3, all 3,010 messages were accepted on each instance, and F7 XFAILed as always.
The leak verdicts were inconclusive, as a five-minute run's must be.

**Run A — a 64-byte leak per message was not caught. The memory gate lacks the
power it was calibrated for.** It ran separately from B, because leaked tasks cost
memory of their own and would confound it. The plant was only
`std::hint::black_box(Box::leak(vec![1u8; 64].into_boxed_slice()))` per relayed
message. At 36,010 messages per instance that is 2.2 MiB an hour, the calibration
target in `tests/compose/leak.rs`.

It ran on 2026-09-15, 11:26–12:23 UTC, with V4 off, because the planted branch
predates D-081. Both containers ran the planted image, `7df0fef77915`. Everything
else was clean: all 36,010 messages were accepted on each instance,
`soak/rest/baseline` passed, and F7 XFAILed. And the memory gate passed both
instances:

| | anon slope | quartile step | verdict |
|---|---|---|---|
| `app` | +2.08 MiB/h | −1.17 MiB | slope just over the 2.0 limit, step below the ~0.7 MiB gate: passed |
| `app2` | −3.19 MiB/h | −1.70 MiB | passed |

**Why: the real floors are noisy, and the self-test's are not.** The calibration
series in `tests/harness_selftest.rs` adds `i % 7` MiB of "bursts". Every
five-minute window of it contains a sample with `i % 7 == 0`, so its floors lie
exactly on the planted line: the self-test calibrated the gate against a
noise-free leak.

The real floors scatter about a line by 2.4–2.8 MiB, in both hours and on both
instances. That puts a one-hour slope's standard error at 3.2–3.8 MiB/h, more than
the whole 2.2 MiB/h signal. Run A's floors cannot be told from the clean hour's
(§9): 11–19 MiB, against 19–28 and 10–19. `process_resident_memory_bytes` does not
separate them either: 10.8 MiB rising to 30.5 and 25.6, against 10.9 rising to
23.4 and 28.1.

**What it means: a one-hour soak cannot tell a 2.2 MiB/h leak from no leak.** The
"no leak" verdicts of §3a, §3b and §9 mean "no leak much above about 6 MiB/h"
(twice the standard error), not "none". A gross leak is caught in the self-test,
but on a live stack that is not shown either. With the same noise, two hours of
floors would put 2.2 MiB/h at about 2.5 standard errors, and four hours at about
7. That is an estimate that assumes independent residuals, not a measurement.

The limit was not tuned to make this run pass. What would restore the gate's power
is a decision for the plan, not a change made here:

- a longer judged run, such as the 24-hour variant, which is already outstanding;
- a quieter series than cgroup anon. The allocator's own count of allocated bytes
  rises by exactly a leak's size, and churn does not move it. It would need a
  gauge on `/metrics`, which is a server change;
- a self-test whose noise the floors cannot remove, so that the calibration claim
  is tested against something like the real series.

---

## 11. The SQL Server build, against Express, with the capture on

The first soak of anything but the Postgres build, and the first of D-085's
capture. Both are stack-level changes rather than §1 variants: V2, V3 and V4 run
unchanged under each, and what moves is what `app` was compiled from, what it
connects to, and what it writes to disk. So they are stacks, not flags —
`tests/compose/stack.rs` carries all four combinations, and two environment
variables choose between them:

| | |
|---|---|
| `SOAK_BACKEND=mssql` | D-084's `--no-default-features --features mssql` build, against **SQL Server 2022 Express** (`test/compose/mssql.yml`) |
| `SIMMER_CAPTURE=on` | D-085's capture, on **both** instances (`test/compose/capture.yml` and the generated config twin). **Not the soak's** — the same variable captures any tier served from the config volume |

```sh
export SOAK_BACKEND=mssql SIMMER_CAPTURE=on
SIMMER_CONFIG=/config/simmer.soak.capture.yaml docker compose \
  -f docker-compose.yml -f test/compose/acceptance.yml -f test/compose/stress.yml \
  -f test/compose/mssql.yml -f test/compose/capture.yml \
  --profile acceptance --profile stress --profile mssql --profile capture \
  up -d --build --wait app app2 sink

SOAK_DURATION=1h cargo test --test soak -- --ignored --test-threads=1 --nocapture soak_run
cargo test --test soak -- --ignored --test-threads=1 --nocapture soak_analyze
```

**Both commands must carry both variables.** `soak_analyze` is a separate cargo
invocation, and although it only reads files today, the banner it prints is the
record of what was judged — and §6's rule about building every compose command
the same way is what stops a stray invocation re-creating `app` as something else.

### What each overlay changes, and what it deliberately does not

**Express, not Developer.** The base file's `simmer-mssql-db` is Developer
edition, which is right for the storage tests — they are about T-SQL correctness,
and an edition ceiling would only slow them down. A soak is the opposite: Express
is what a small deployment runs, and its ceilings (a 1410 MB buffer pool, four
cores, 10 GB a database) are the kind of limit that appears over an hour and not
over a test. `SERVERPROPERTY('Edition')` was read from the running container
rather than inferred from `MSSQL_PID`: `Express Edition (64-bit)`, 16.0.4265.3.

**A capture volume per instance.** A bucket file's name is a pure function of its
ten-minute window, so one shared directory would have both processes appending to
the same file under the same name, and the per-instance accounting — the A/B pair
the whole tier rests on — would be one interleaved stream. The capture is on
**both** instances for the same reason: V2's asymmetry is that `app` is scraped
and `app2` is not, and capture on one only would cost the tier its one controlled
comparison.

**One capture block, and it belongs to no tier.** `capture:` being absent is the
only way to turn the capture off and YAML has no conditional block, so a captured
run needs a second config file. Rather than commit one per tier,
`test/config/Dockerfile` generates a `<name>.capture.yaml` twin for every tier
config from a single `test/config/capture.block.yaml`. So the soak, the stress
tier and anything else served from the config volume are captured the same way,
by the same file, and no twin can drift from its original. The capture's values
are the documented defaults deliberately: what an operator who read
docs/CAPTURE.md §2 and changed nothing would get.

**The harness learned a second dialect.** `Stack::sql` replaces `Stack::psql`
where a tier does not care which backend it is on, and `Stack::sql_command`
hands V4's ledger sampler a ready-made command so a slow query costs one sample
rather than the run. Both dialects are asked for one `|` separated row —
`psql -At` is that by definition, `sqlcmd` needs `-h -1 -W -s '|'` and a
`SET NOCOUNT ON`. `reset_quota` gains the T-SQL form, since T-SQL has no
multi-table `TRUNCATE`.

### One change to the stack that is not plumbing: `tiberius=warn`

`docker-compose.yml` and the stress overlay set `RUST_LOG: "info,sqlx=warn"`.
That is the Postgres build's answer to a chatty driver, and the mssql build has no
equivalent: tiberius logs `Begin transaction` and `Commit transaction` at INFO, so
§7.4's reserve and commit put four lines per message on the session's own task —
about 290,000 lines an hour at the soak's rate, written synchronously, into the
same log step 5c's correlation ids are followed through. 28 of `app`'s 46 startup
lines were tiberius's.

`test/compose/mssql.yml` sets `RUST_LOG: "info,sqlx=warn,tiberius=warn"`, without
which this stack would not be measuring the same thing the Postgres runs measured
— it would be measuring its own logging. **It is also a gap in what is shipped:**
the published guidance gives `info,sqlx=warn` for both images.

### The hour — 2026-09-20, 14:27–15:28 UTC

Images built from this working tree; `Express Edition (64-bit)` 16.0.4265.3 read
back from the container; `"senders": 2` in both instances' startup lines (§6's
trap) and `migrations applied backend=mssql` in both.

| | `app` | `app2` |
|---|---|---|
| messages | 36,010 | 36,010 |
| accepted | 36,010 | 36,010 |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| p50 / p90 / p99 | 23.1 / 54.5 / 75.6 ms | 22.9 / 50.0 / 72.5 ms |
| max | 561.3 ms | 562.6 ms |
| over 200 ms | 2 | 2 |

**Not one message was deferred or refused in an hour on Express**, and the
latency distribution is indistinguishable between the instances. Both slow
messages are the same two on each instance — `soak-*-20313` at ~561 ms and
`soak-*-20298` at ~294 ms, both `250 at dot`, both traced by correlation id. Two
messages in 72,020 is not F16 returning.

V4 (F2) behaved exactly as on Postgres: 275 messages in 11 sessions per instance,
220 accepted, 11 cut by the session timeout, **none cut at the dot and none of
the cut ones stored by the sink**, `reservations_in_flight` 0 after the drain
against 11 cut, at most 2 reservation rows across 121 samples, and 440 committed
on `warming-cancel` — 220 per instance, which is what was accepted. §7.4's
reserve/commit protocol, the reservation registry and the sweeper all work on
SQL Server exactly as the ledger says they should.

At rest, both instances: threads 3 (3 before the first message), tasks 13 (13),
`sessions_active` 0, `reservations_in_flight` 0, **0 unaccounted descriptors**.

#### The capture, measured

| | |
|---|---|
| records | **36,230 per instance** |
| written | 913,960,956 bytes (0.85 GiB), **0.85 GiB/h** |
| bodies omitted over `max_body_bytes` | 1,827 (5.0% — the 1m and 4m ends of the size distribution) |
| late writes, clock regressions, files swept | 0, 0, 0 |
| dropped, deferred | **0, 0** |
| writer queue at rest | 0 deep |

**The accounting is exact, and it is the invariant worth keeping.** 36,230 =
36,010 V2/V3 messages + the 220 V4 messages that were accepted. The 11 cut by the
session timeout and the 44 that ended with the connection are *not* captured, and
should not be: D-081 answers `421` at the next command boundary, which is `RSET`,
so those sessions never reached DATA and there was never a message to record.
Capture records equal exactly the messages Simmer answered `250` to, on both
instances, over 72,460 records. `soak_analyze` does not yet gate on this — the
terms were first measured here — but it has now held on three runs: 7,250 records
on the 12-minute validation run, 36,230 on the hour, and 7,250 again on the
12-minute regression run against the fixed build.

`retention` was never reached, and neither was the sweeper's hourly interval
until the very end. See F17.

#### F17 — found, and fixed

`simmer_capture_disk_bytes` read **0 for 59.9 of the 60 minutes** while 904 MB
accumulated, then jumped to 906,530,238 in the final sample when the sweeper's
first post-startup tick fired. docs/CAPTURE.md §6 offered that gauge as "what
tells you a capture left on will fill the volume" and §1 said to alert on it; it
could do neither, because the only thing that wrote it ran hourly.

The first version of this tier's check read the **final scrape** and therefore
XPASSed this run — the tick lands about sixty minutes after startup, which on an
hour-long run is a few seconds before the last sample. The check now judges the
**series**, so its power does not depend on where the tick falls.

Fixed rather than left open: the writer adds what each flush pushes
(`metrics::capture_disk_grew`), and the sweeper's pass still *sets* the count
from the directory. That division is the point — an increment-only gauge cannot
see a bucket deleted by hand or bytes against blocks, and since the sweeper
evicts whole buckets it would climb and never come down; D-056's rule keeps the
directory authoritative and bounds the estimate's error to one interval. Verified
live: 18,123 bytes on the gauge two seconds after three messages, equal to
`du -b` on the file.

#### The one gate that failed, and why it is not a leak

`app` failed `anon` at **+4.28 MiB/h** with a quartile step of +2.64 MiB. Its
twin, carrying an identical stream, read **-7.29 MiB/h**. The series is
oscillation, not a climb — `app` runs 56, 49, 37, 36, 48, 24, 49, 43, 50, 43,
20 MiB across the hour, `app2` 26, 57, 44, 34, 43, 33, 42, 62, 38, 50, 24 — and
both finish near their lowest reading.

§10 already established what this gate can resolve: the residual noise puts its
standard error near 3 MiB/h, so a "no leak" verdict from a one-hour run means
"no leak much above about 6 MiB/h". +4.28 against a twin at -7.29 is inside that
band, and the run therefore establishes **no memory verdict either way** — not a
leak, and not a clean bill. That is the same limitation §10 recorded, met again
on a different backend with a new subsystem running; **the limit was not
adjusted to make this run pass**, and the remedies remain §10's: a longer judged
run, or a quieter series than cgroup anon.

Everything else passed. F7 XFAILed as always: 1,495 new metric series over
50 minutes, all `simmer_unmatched_sender_total{domain}`, client-controlled.

#### What this hour did not establish

- **Nothing about Express's ceilings.** The database stayed at ~1.43 GiB, its
  edition buffer-pool limit, and never approached the 10 GB database cap: an hour
  of quota rows and recipient-frequency events is megabytes. A run that pressed
  either limit would be a different test.
- **Nothing about the capture under eviction.** `retention` is 24h and the
  sweeper's interval is an hour, so no bucket was ever deleted. What the capture
  costs while the sweeper is actually evicting — and whether the gauge's
  increments and its recount agree across an eviction — needs a run longer than
  the interval, or a shorter retention and a run past it.
- **Nothing about the mssql build under stress.** T3 has never been run against
  it; this is 10 msg/s, not peak load.

---

## 12. The partial ramp (D-091), on — `share: [0.5]` for an hour

The first soak with `warmup.schedule.share` set. `v0.6.0` shipped the partial
ramp after the ordinary release hour (2026-09-21, 21:43–22:40 UTC: every gate
passed, F7 XFAILed). But the standard soak config sets no `share`, so that hour
only showed the walk *carrying* step 3c′. This one runs it.

### What was changed, and how to repeat it

One line in `test/config/simmer.soak.yaml`, **reverted after the run** so the
standard soak stays comparable with every hour above:

```yaml
  - name: warming-newbrand
    warmup:
      schedule:
        default: [100000000]
        share: [0.5]
```

And `SIMMER_WARMUP_STARTED` set to an hour before the run. The stack's default,
2026-08-01, puts the route on day ~52, **past the end of a one-entry list, where
every message is offered**. The line would then have done nothing, and the hour
would have looked exactly like a pass. The run script checks for this before it
starts. It reads the line back from the config volume (§6's stale-reseed trap
applies, so the `stress-config` image was rebuilt first), and it reads `/routes`
on both instances. Both reported `day_index 0` and
`partial_ramp: {share: [0.5], today: 0.5}` before the first message. The script
is `target/soak-runs/2026-09-22-share-hour/run.sh`.

`soak_analyze` does not look at which route carried a message. So the script
also scrapes both instances' `simmer_messages_total` and
`simmer_route_skipped_total` after the load, which is where the split below
comes from.

### An attempt voided by a suspended host

The first attempt (06:35 UTC) was frozen partway through when the host was
suspended. On resume, its script, `soak_run` and all four loadgens were still
alive, with the hour's clock broken. They were killed, and the loadgens were
removed by name (§6: stopping `soak_run` does not stop them). The stack went down
**with `-v`**, so the retry started from empty quota rows and an empty sink.
Nothing from that attempt is used here; its directory is kept as
`2026-09-22-share-hour-aborted/`. **A soak on a laptop has to keep the laptop
awake.**

### The hour — 2026-09-22, 08:12–09:12 UTC

On the `v0.6.0` images (built from `1fd176b`: `app` `52cf395a…`, `app2`
`c0c2628f…`, the same images as the release hour). `"senders": 2` in the startup
lines.

| | `app` | `app2` |
|---|---|---|
| walk decisions at `warming-newbrand` | 34,209 | 34,209 |
| offered, and delivered on it | 16,917 | 17,222 |
| turned away (`reason="partial_ramp"`) | 17,292 | 16,987 |
| **share offered** | **49.45%** | **50.34%** |
| delivered on `overflow-established` | 19,093 | 18,788 |
| of which unmatched senders (V3) | 1,801 | 1,801 |
| delivered on `warming-cancel` (V4) | 220 | 220 |
| **delivered, all routes** | **36,230** | **36,230** |
| messages over 200 ms | 0 | 0 |

**Every message was delivered.** `simmer_messages_total` has no `deferred` or
`rejected` series on either instance. 36,230 is the usual 36,010 plus V4's 220
accepted, the same total as every Postgres hour above. Overflow is exactly the
partial-ramp skips plus V3's unmatched senders (17,292 + 1,801 = 19,093 on
`app`, 16,987 + 1,801 = 18,788 on `app2`). **Every message the share turned away
reached the next link.** None was dropped, and none reached §10.3's `451`.

**The share is what was configured.** Over 34,209 decisions, a fair 50% has a
standard deviation of 0.27 points. `app` is 2.0 of those low and `app2` 1.3
high, which is ordinary for an HMAC over distinct recipients.

**One thing this run cannot show: that two instances agree.** Each loadgen tags
its recipients with its own instance (`soak-app-N@…` against `soak-app2-N@…`,
`tests/soak.rs`), so no recipient is ever seen by both. The two splits are
independent samples, and they differ, as they should. At the 10-minute mark the
two instances happened to show identical counts, and that was briefly misread as
agreement. It was a coincidence, gone by 18 minutes. Cross-instance agreement
rests on `routing::partial`'s `the_same_message_gets_the_same_answer` and
`tests/partial_ramp.rs`'s dry-run check, not on the soak.

**The gate costs nothing a soak can see.** 0 messages over 200 ms on either
instance, the same as the release hour without it. The salt is read once per
process and held (`Frequency`'s `OnceCell`), so a turned-away message costs an
HMAC and no database round trip.

V4 (F2), unchanged: 275 messages in 11 sessions per instance, 220 accepted, 11
cut by the session timeout, **none at the dot and none of those stored**.
`reservations_in_flight` was 0 after the drain, there were at most 2 reservation
rows across 121 samples, and 440 were committed on `warming-cancel`.

At rest, both instances: threads 3 (3 before the first message), tasks 11 (11),
**0 unaccounted descriptors**. Descriptors and threads were flat on both.

F7 XFAILed as always: 1,645 new metric series over 55 minutes, all
`simmer_unmatched_sender_total{domain}`.

#### The one gate that failed: `app2`'s `anon`, again not a leak and not a clean bill

`app2` failed `anon` at **+2.84 MiB/h** with a quartile step of +1.42 MiB,
against a limit of 2 MiB/h. Its twin read **−3.40 MiB/h**. The 5-minute floors
the gate works from, after the warm-up:

| minute | 5 | 10 | 15 | 20 | 25 | 30 | 35 | 40 | 45 | 50 | 55 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `app` | 14 | 15 | 17 | 12 | 16 | 13 | 13 | 13 | 14 | 13 | 11 |
| `app2` | 11 | 11 | 10 | 13 | 14 | 16 | 13 | 14 | 15 | 12 | 13 |

(MiB, the minimum `anon` in each 5-minute window, from the saved `app*.csv`.)
`app2` steps up by about 3 MiB between minutes 15 and 30 and then holds. That is a plateau, not a climb, and a
least-squares slope over an hour cannot tell the two apart. §10 measured what
this gate can resolve: a standard error near 3 MiB/h, so a one-hour "no leak"
means "no leak much above about 6 MiB/h". +2.84 is well inside that band. **The
run therefore gives no memory verdict either way**, the same result §11 recorded
at +4.28. The limit was **not** adjusted to make it pass.

Nothing D-091 added keeps anything per message. The share is read from
configuration, the keyer is built once, and the hash's input and output are
dropped at the end of the check. So there is no mechanism here for a leak to
come from. That is an argument, not a measurement, and the measurement is
§10's: a longer judged run, or a quieter series than cgroup anon.

#### What this hour did not establish

- **Anything about a share that changes during a run.** One entry, one day. A
  day boundary moving the route from `share[0]` to `share[1]`, or off the end of
  the list, happened in the unit and walk tests, never on the stack under load.
- **Cross-instance agreement,** for the reason above.
- **The partial ramp against a real cap.** The soak's allowance is 10⁸, so no
  message was ever refused for quota, and "the cap fills later in the day",
  which is what the feature is for, was not observed. It needs a cap small
  enough to be met within the run.
- **The mssql build.** Postgres only.

---

## 13. What `anon` was measuring — the share hour with jemalloc's counters (D-092)

§12's hour failed the `anon` gate on a series that stepped up once and then held,
and nothing in the samples could say whether that was simmer holding memory or
jemalloc keeping pages simmer had freed. D-092 added jemalloc's own counters to
the samples. This hour repeats §12 with them on.

### What was run

- Images built from the working tree with `SIMMER_CARGO_FEATURES="--features
  alloc-stats"` (`app` `d806f8b6…`, `app2` `604e203d…`), and
  `SIMMER_ALLOC_STATS_FILE=/tmp/simmer-alloc-stats`. **Not a published build**:
  jemalloc's `stats` option is on.
- `share: [0.5]` on `warming-newbrand` and `SIMMER_WARMUP_STARTED` an hour back,
  as §12. The line was reverted afterwards, as there.
- The stack brought down with `-v` first, so quota rows and the sink started
  empty.
- The run script refused to start unless both instances were writing all six
  counters, and logged a first reading from each before the load.
  `target/soak-runs/2026-09-22-share-je-hour/run.sh`.

2026-09-22, 10:02–11:02 UTC. `"senders": 2`.

### The answer: simmer holds about 1.6 MiB, and `anon` is the allocator

| MiB, whole hour | `app` | `app2` |
|---|---|---|
| `anon` | 5 – 69 | 5 – 71 |
| `je_resident` (what jemalloc holds) | 7.7 – 68.7 | 7.5 – 70.9 |
| **`je_allocated` (what simmer holds)** | **1.5 – 2.0** | **1.5 – 2.0** |
| `je_metadata` | 5.2 – 7.4 | 5.1 – 5.4 |

`anon` follows `je_resident`, and `je_resident` is 4 to 35 times what simmer
actually holds. At 10 msg/s and about 20 ms a message, 0.2 messages are in flight
on average, so live message bodies are tens of KiB. A 1 MiB message makes jemalloc
map pages; simmer frees them within milliseconds; jemalloc keeps them resident
for its dirty-page decay before returning them. The floors the anon gate judges
are that decay's low points. A 3 MiB step in them, which failed §12, is how many
freed pages jemalloc happened to be holding at the quietest sample, not a change
in simmer.

The same code under the same load, this hour: **`anon` passed on both
instances**, +0.23 and −0.38 MiB/h. §12's failure was noise, as §10 predicted
it could be.

### The one real growth: F7, about 204 bytes per series

`je_allocated` rose steadily on both instances: **+0.39 MiB/h** on `app` and
**+0.38 MiB/h** on `app2` (Theil–Sen over the 5-minute floors), a fifth of the
anon gate's limit. `allocated` counts only bytes simmer holds, so this is simmer's.
Lined up against the unmatched-sender series count the soak scrapes from `app`
every 30 s:

| minute | unmatched series | `je_allocated` median |
|---:|---:|---:|
| 5 | 219 | 1.612 MiB |
| 15 | 521 | 1.683 MiB |
| 25 | 823 | 1.754 MiB |
| 35 | 1,125 | 1.835 MiB |
| 45 | 1,426 | 1.871 MiB |
| 55 | 1,676 | 1.871 MiB |

**r = 0.979, and the slope is about 204 bytes per series**: 1,457 new series,
265 KiB. V3 mints one series per fresh `u<n>.soak.test` sender, and until now the
exporter held every one for the life of the process. The rate is the soak's; the
behaviour is simmer's. A long-running daemon fed new sender domains grows without
bound, about 195 MiB per million, and only `/metrics` ever reads them. That is
D-093: `/metrics` off unless `admin.metrics` enables it, and idle counters
expired.

`je_retained`, address space jemalloc keeps mapped for reuse, rose over the first
ten minutes and then held around 150–160 MiB. Its hourly slopes (−9.54 and +2.82
MiB/h) are that early rise and later wobble, not a trend. `je_metadata` was flat.

### The split, again

| | `app` | `app2` |
|---|---|---|
| offered to `warming-newbrand` / turned away | 17,050 / 17,159 | 17,229 / 16,980 |
| share offered | 49.84% | 50.36% |
| delivered, all routes | 36,230 | 36,230 |

Every message delivered; overflow is again exactly the turned-away messages plus
V3's 1,801. V4 unchanged: 220 accepted and 11 cut per instance, none at the dot,
0 reservations in flight after the drain, 440 committed on `warming-cancel`.
F7 XFAILed: 1,645 new series over 55 minutes.

### What failed, and why: the instrument

**Threads at rest, 5 against 4 before the first message, on both instances.**
Every earlier hour read 3 → 3. The baseline itself was one higher, because the
first version of D-092's writer used `tokio::fs`. Each call runs on tokio's
blocking pool, whose threads linger about 10 s after use, so a write every 5 s
kept one pool thread alive and sometimes started a second. **The check failed on
the instrument, not on simmer.** The writer now uses `std::fs` directly on its
task, which starts no thread (D-092). The next run with the counters on is the
check that it did.

**The analysis did not run at the end of the hour.** `metrics-util` had been
added to `Cargo.toml` while the hour ran, for D-093, and the script's
`cargo test --locked` refused to update the lockfile. Nothing was lost: every
sample was on disk, and `soak_analyze` was run over them once the load was over.
The figures above are from that run. **Do not edit `Cargo.toml` while a soak
is running**; it is §6's no-builds rule, met from a new direction.

**Latency: 4 messages over 200 ms on `app`, 5 on `app2`,** against none in §12.
The slowest two are the same messages on both instances (`soak-*-24500` at
~1.3 s, `soak-*-24480` at ~0.97 s), about 41 minutes in, so the cause is shared:
the database, the sink or the host. It was not traced. Nothing was being built at
the time.

### What this hour did not establish

- **That F7 is all of the growth.** A correlation of 0.979 over eleven windows
  leaves little room, but a second source growing at the same rate would hide in
  it. The metrics-off run is the test: with no recorder there is no F7, so
  `je_allocated` should be flat.
- **Anything about the published binary's memory.** jemalloc's `stats` option
  changes the allocator slightly. The anon comparison with §12, same code
  otherwise, suggests nothing material, but it was not measured.
- **Whether the anon gate should be replaced.** `je_allocated` is the better
  series for a leak gate: flat and quiet where `anon` swings by 60 MiB. Moving the
  gate needs the feature in every soak image, and a limit. That is a decision for
  after the metrics-off run.

---

## 14. Metrics off — does simmer still grow? (D-093)

§13 attributed simmer's only live-heap growth to F7's unmatched-sender series,
at about 204 bytes each. D-093 made `/metrics` opt-in: off, no recorder is
installed and nothing is held. This run checks the attribution: with no recorder
there is no F7, so `je_allocated` should not grow.

### What was run

§13's setup exactly (`share: [0.5]`, jemalloc's counters on, the stack reset with
`-v`), but with `admin.metrics: false` and images built from `d44b68f` (the
v0.7.0 code, plus `alloc-stats`; `app` `c2e25e25…`, `app2` `465b6933…`). **30
minutes**, not an hour. The run script refused to start unless the config volume
said `metrics: false` and both instances answered `/metrics` with `404`; both did,
and both logged `admin.metrics is off: no recorder, and /metrics is not served`.
2026-09-22, 11:12–11:42 UTC.
`target/soak-runs/2026-09-22-metrics-off-30m/run.sh`.

### The answer: flat

| `je_allocated` median, MiB | 0–5 | 5–10 | 10–15 | 15–20 | 20–25 | 25–30 | Theil–Sen |
|---|---|---|---|---|---|---|---:|
| `app`, metrics **off** | 1.478 | 1.464 | 1.472 | 1.499 | 1.510 | 1.501 | **−0.05 MiB/h** |
| `app2`, metrics **off** | 1.396 | 1.402 | 1.419 | 1.417 | 1.419 | 1.384 | **−0.01 MiB/h** |
| `app`, metrics on (§13) | 1.582 | 1.611 | 1.653 | 1.683 | 1.714 | 1.754 | +0.39 MiB/h |

**With no recorder, simmer's live heap does not grow.** Over the same 30 minutes
with metrics on, the median rose 0.17 MiB; with metrics off it moved within 0.05
MiB and ended where it began. The baseline is also lower, by about 0.1 MiB on
`app` and 0.2 MiB on `app2`: with no recorder, the registry, the histograms and
every route's series are not held at all. §13's attribution holds: **F7 was the
whole of the growth.**

The soak's F7 check **XPASSed** for the first time: 0 series from 302 s to 1,781 s,
against 1,645 new series an hour in every run before.

**The writer fix worked.** Threads at rest were 3 against 3 before the first
message on both instances, where §13's hour read 4 → 5 with the `tokio::fs`
writer.

Everything else behaved as before: V4 cut 5 sessions per instance in 30 minutes,
none at the dot, 0 reservations in flight after the drain; every message
delivered.

### What failed, and why: the harness reads `/metrics`

- **`soak/rest/baseline`** reported the final scrape missing
  `simmer_sessions_active`, `simmer_db_pool_connections`, the quota series and
  the pool series. There was no final scrape: `/metrics` answered `404`, as
  configured.
- **"3 descriptors" (`app`) and "2" (`app2`) at rest "that no pool holds",** where
  earlier runs had 0. The harness counts pooled sockets from the pool gauges in
  the final scrape, so with no scrape it counts none and reports the pools' idle
  database and downstream connections as unaccounted. The same harness
  dependency, not a leak.
- **The trend gates were inconclusive** by design: 30 minutes past a 5-minute
  warm-up is five 5-minute floors, and the gates need eight. The `je_resident`
  and `je_retained` slopes over five points (+22, +34, +127 MiB/h) are
  meaningless for the same reason.

**The harness cannot fully judge a metrics-off run.** A soak of the default
configuration would need the at-rest checks to get pool counts some other way.
Until then, a metrics-off run is judged on `je_allocated`, as this one was.

**Latency:** 9 messages over 200 ms on `app` and 7 on `app2`. The slowest two are
the same messages on both instances (`soak-*-15600` at ~2.0 s and `soak-*-15610`
at ~1.0–1.1 s), about 26 minutes in, so the cause is shared, as in §13. It was
not traced.

### What this run did not establish

- **An hour's verdict on the trend gates.** Thirty minutes was the question
  asked, and `je_allocated` answers it. The anon, descriptor and thread gates
  need an hour.
- **The published binary.** As in §13, the counters need `alloc-stats`, which no
  published image has.

---

## 15. The computed share (D-097), on — `share: {mode: auto}` for an hour

The first soak with D-097's controller. §12 ran the partial ramp at a **listed**
`share: [0.5]`; this hour runs the same gate at the same fraction through the
**computed** path, so the two are directly comparable and the difference between
them is the new code and nothing else.

### What was changed, and how to repeat it

Four lines in `test/config/simmer.soak.yaml`, **reverted after the run** for
§12's reason — the standard soak has to stay comparable with every hour above:

```yaml
  - name: warming-newbrand
    warmup:
      schedule:
        default: [100000000]
        share:
          mode: auto
          ceiling: 0.5
```

**Why the ceiling pins it, and why that is the point.** The allowance above
cannot be met in a run — the soak's own soundness check requires that, since a
route that exhausted its allowance would leave the rest of the hour measuring
overflow. So `c` sits at 1 throughout, the ratio is above the ceiling at every
sample, and the controller is clamped at 0.5 from the first message to the last.
That is a *fixed* share of 0.5 reached through D-097's arithmetic, which is
exactly what makes it a controlled comparison with §12.

So this hour measures the auto path's **cost and agreement** — whether the extra
non-locking read of the quota row on every message shows up in memory, in
latency, or in the two instances disagreeing. It does **not** measure the
controller's dynamics: the share moving as the cap fills, the window closing,
the tail releasing. Those need a cap that can be met, which this tier cannot
have. `tests/auto_share.rs` is where they are measured, against a cap of 40 on
the acceptance stack's two mail traps.

`SIMMER_WARMUP_STARTED` was set to an hour before the run, and `/routes` on both
instances was read before the first message: both reported `day_index 0` and
`partial_ramp: {mode: "auto", today: null, auto: {ceiling: 0.5, …}}`, with
`partial_ramp_share: 0.5` on the `catchall` window. §12's trap — a stale config
in the volume, or a day index past the end of a list — is checked the same way,
and the `stress-config` image was rebuilt before the stack came up.

### The hour — 2026-09-24, 09:12–10:13 UTC

| | `app` | `app2` |
|---|---:|---:|
| messages | 36,010 | 36,010 |
| accepted | 36,010 | 36,010 |
| deferred / refused / transport | 0 / 0 / 0 | 0 / 0 / 0 |
| sustained rate | 10.00 msg/s | 10.00 msg/s |
| offered, and delivered on it | 17,282 | 17,084 |
| turned away (`reason="partial_ramp"`) | 16,927 | 17,125 |
| decisions | 34,209 | 34,209 |
| **share offered** | **50.52%** | **49.94%** |
| p50 / p90 / p99 | 7.8 / 37.2 / 47.9 ms | 8.3 / 38.9 / 51.6 ms |
| messages over 200 ms | 3 | 2 |
| `fds` | +0.00/h | +0.00/h |
| `threads` | +0.00/h | +0.00/h |
| at rest | threads 3 (3 before), tasks 11 (11 before), 0 unaccounted fds | same |

**The share is what the controller computed, on both instances.** 50.52% and
49.94% over 34,209 decisions each, against §12's 49.45% and 50.34% over the same
34,209 at a listed 0.5. Both instances reported `simmer_partial_ramp_share` of
exactly 0.5 throughout, and — the part that matters for a value read from shared
state — **they never disagreed**: `simmer_quota_committed` for the route was
34,366 on both, which is `17,282 + 17,084` exactly. The row the two instances
share equals the sum of what they each delivered, to the message.

**The extra read costs nothing a soak can see.** 3 and 2 messages over 200 ms,
against §12's 0 and 0, §13's 4 and 5, and §14's 9 and 7 — the *lowest* tail
since §12, on a path that now reads one more row per message per warming route.
p99 is 47.9 and 51.6 ms, within the band every hour above sits in. Descriptors
and threads are flat to two decimal places, and both instances returned to the
thread and task counts they started with, with no unaccounted descriptors.

#### The one gate that failed: `app`'s `anon`, and it is §12's failure again

> **Since retired as a failure (D-098).** This was the third hour the 2 MiB/h
> limit failed on noise, and it prompted raising the limit to **6 MiB/h** — what
> §10 and §11 had already established a one-hour run can resolve. Re-judged at 6,
> this hour passes every gate. What follows is what the run reported at the time,
> and the reasoning that made the limit change the right answer rather than a
> tuning.

`app` failed `anon` at **+2.02 MiB/h** with a quartile step of +2.29 MiB, while
`app2` — running the identical path against the identical stream — came in at
**−1.88 MiB/h**. The series both slopes are fitted through:

| | `app` | `app2` |
|---|---:|---:|
| minimum | 4.53 MiB | 4.53 MiB |
| maximum | 58.95 MiB | 63.43 MiB |
| at 5 min | 27.10 MiB | 55.61 MiB |
| at 30 min | 17.48 MiB | 17.68 MiB |
| at 60 min | 24.05 MiB | 20.76 MiB |

A series that swings by 40 MiB is not one a 2 MiB/h trend can be read out of.
This is §12's failure in every particular except which instance drew the short
straw, and §12 already established what it is: *"a standard error near 3 MiB/h,
so a one-hour run gives no memory verdict either way"*. D-092 then settled what
the swing is made of — simmer's live heap is 1.5–2.0 MiB and nearly flat, while
`anon` and `je_resident` swing between about 8 and 70 MiB as jemalloc holds and
returns pages.

**Two things make it very unlikely to be D-097's read.** `app2` is never scraped
(F8), so it is the instance that carries the message path and nothing else — and
it *shrank*. And the growth D-092 attributes to F7 lands on the scraped instance
by construction: this hour's metric series went from 236 to 1,882, 1,797 of them
unmatched-sender, which is the XFAIL below and D-093's subject.

**It is not a clean bill, and should not be read as one.** No sample carried
jemalloc's counters — this hour was built without `alloc-stats` — so the live
heap was not measured, and this run cannot separate held bytes from retained
pages by itself. It rests on `app2`'s sign and on §13 having answered the same
question for the same series. D-092's instrument is what would settle it: an
hour with `SIMMER_CARGO_FEATURES=alloc-stats` and `SIMMER_ALLOC_STATS_FILE` set,
reading `je_allocated` rather than `anon`. §13 measured F7 there at +0.39 MiB/h,
and a repeat that lands near it is the confirmation this hour cannot give.

#### What else the hour reported

- **XFAIL F7** — metric series 236 → 1,882 (unmatched-sender 151 → 1,797) over
  55 minutes. Expected, known, D-093's subject, and unrelated to this change.
- **V4 (F2)** — 220 accepted and 11 cut by the session timeout on each instance,
  `reservations_in_flight` 0 after the drain on both, at most 2 reservation rows
  across 121 samples, and the sweeper clearing the stranded ones. The cancelled
  relay's ledger is unaffected by the share, as it should be: the gate runs
  before the reservation, so a message it turns away never reaches the row.

#### What this hour did not establish

The controller's dynamics, for the reason given above — a share clamped at its
ceiling exercises the arithmetic and the read, not the feedback. The `-mssql`
build's run of it. And any memory verdict at all, at this gate's resolution —
which is now what the gate itself says, rather than something a reader had to
find in §10.

---

### The case for gating on `je_allocated` (open)

D-098 raised `anon` to 6 MiB/h because that is what it resolves, and said
plainly what that gives up: a 64 B/message leak at 10 msg/s is 2.2 MiB/h and a
one-hour run no longer catches it. The way to get that sensitivity back is not a
lower limit on a noisy series but a gate on a quiet one, and D-092 built the
instrument for it and deliberately left the decision open — *"which to gate on,
and at what limit, is a decision the data exists to inform."*

Three hours of that data now exist:

| | `je_allocated` slope | `anon` slope, same hour |
|---|---:|---:|
| §13, `app` | +0.39 MiB/h | — |
| §13, `app2` | +0.38 MiB/h | — |
| §14 | +0.39 MiB/h | — |

Scatter in the hundredths, against an `anon` series that swings between 4.5 and
63 MiB in a single hour. §13 measured `je_allocated` climbing 1.582 → 1.754 MiB
over its hour and tracking F7's unmatched-sender series at r = 0.979 — which is
the point: on that series a real, small, explicable growth is *visible*, and
gating near 1 MiB/h would restore the 64 B/message sensitivity and better.

**What stops it being done today,** and what a decision would have to settle:

- Every soak run would need `SIMMER_CARGO_FEATURES=alloc-stats`, and D-092 is
  explicit that jemalloc's `stats` changes the allocator — so a gated run
  measures a slightly different binary from the one that ships.
- With the feature off there are no counters and so no gate. Either `anon`
  stays as the fallback (two gates, two limits, one of them usually skipped) or
  the soak refuses to run without the feature.
- The limit itself. +0.39 MiB/h is F7, which is known, bounded by D-093's idle
  expiry, and not a defect. A gate at 1 MiB/h passes it; a gate at 0.25 would
  not. Picking the number means deciding what counts as F7's ceiling.

## 16. v0.9.0 (named ramps, MX grouping) — 40 minutes with jemalloc's counters

### What was run

Images built from the `v0.9.0` release commit `35fffb1`, with
`SIMMER_CARGO_FEATURES="--features alloc-stats"` and
`SIMMER_ALLOC_STATS_FILE=/tmp/simmer-alloc-stats` (`app` `9264dcbd…`, `app2`
`9ea02bf4…`). **Not a published build**: jemalloc's `stats` option is on. The
shipped soak config has one ramp, `main`, and the stack was reset with `-v`, so
the ramp migration ran on a fresh schema. 40 minutes at 10 msg/s per instance,
with a 5-minute warm-up. 2026-09-25, 15:48–16:28 UTC.
`target/soak-runs/2026-09-25-v090-je/run.sh`.

**The first attempt stopped at its own gate**, and the reason is a trap. The
script rebuilt only `app` and `app2`. `down -v` removed the config volume, but
`up` refilled it from a `simmer-stress-config` image 29 hours old, which held the
pre-ramps config. v0.9.0 refused it, as designed ("moved under ramps"), and
`app` exited, so the gate saw no allocator stats. The script now builds every
service. **After any config-schema change, rebuild the stack whole**; the config
image is a service like any other.

### The answer: correct, and the live heap flat

- **Correctness.** 24,010 of 24,010 accepted per instance (0 deferred, 0 refused,
  0 transport). V4 behaved as designed: 140 accepted and 7 cut by the session
  timeout per instance, none at the dot, `reservations_in_flight` 0 after the
  drain, and 0 reservations expired by the sweeper. Every route series carried
  `ramp="main"`, and `simmer_ramp_selected_total{ramp="main",source="default"}`
  was 24,150 per instance.
- **Back at rest:** threads 3 and tasks 12, as before the first message; 0
  unaccounted descriptors; peak `CLOSE_WAIT` 1.
- **`je_allocated`,** simmer's live heap: 1.4–2.1 MiB (`app`) and 1.5–2.0 MiB
  (`app2`), slopes +0.73 and +0.46 MiB/h. The same band §13 measured on v0.7.
  Named ramps and MX grouping added no measurable steady-state heap; the soak
  config has no `mx` lists, so no MX cache was filled.
- **`je_resident` and `je_retained`** swing to 75–78 MiB and about 164 MiB during
  the large-message bursts, and come back. This is the allocator, as §13 found,
  and it is not judged.
- **F7, as expected:** unmatched-sender series 151 → 1,193 over 35 minutes. That's
  the known client-controlled `domain` label, bounded by D-093's idle expiry, and
  still an XFAIL.

### What this run did not establish

- **A leak verdict.** 35 minutes after the warm-up is seven five-minute floors,
  and `leak::verdict` needs eight, so `anon`, `fds` and `threads` are
  *inconclusive*, not green. An hour settles them, as in §3a.
- **More than one ramp under load.** The soak config has one. Two ramps were
  exercised end to end in the upgrade rehearsal (`docs/STATE.md` §0), not for
  duration.
- **The SQL Server build.**

## 17. v0.9.0, the hour — the leak verdict §16 could not give

### What was run

§16's script with `SOAK_DURATION=1h`, on the tagged release commit `35fffb1`,
checked out detached (`app` `57248106…`, `app2` `7fdb2341…`, `alloc-stats` on),
and the stack reset with `-v`. 2026-09-29, 11:42–12:40 UTC.
`target/soak-runs/2026-09-29-v090-je-hour/run.sh`.

### The answer: green, with one unexplained one-off

- **The leak gates are green on both instances** (eleven floors after the
  warm-up; §16 had seven). `anon` slope +1.99 and +2.37 MiB/h; fds and threads
  flat.
- **Correctness.** 36,010 of 36,010 accepted per instance, none over 200 ms
  (max 82.5 ms), 0 deferred, refused or transport errors. V4: 220 accepted and
  11 cut per instance, none at the dot, `reservations_in_flight` 0 throughout,
  and 0 expired by the sweeper. Back at rest: threads 3, tasks 12, 0
  unaccounted descriptors.
- **F7:** unmatched-sender series 151 → 1,796 over 55 minutes, still the
  XFAIL. `app2`'s `je_allocated` creeps +0.50 MiB/h (1.5–2.2 MiB), which is
  about F7's size.
- **`app` took a one-off 18 MiB step at the start of the load and held it.**
  `je_allocated` went 2.0 → 20.4 MiB between t = 0.4 s and t = 10.6 s. It then
  rose only +0.64 MiB/h to 21.2 MiB over the hour, the same slope as `app2`.
  So it is a single retention, not growth. `app2` did not do it; nor did either
  instance in §16, on the same code. The two instances' logs are identical in
  shape, and the first relayed message came 24 s after start on both.
  **Cause undetermined.**

### Following up the step (2026-09-29)

- **It was released at rest.** In `rest-app.csv` (t = 3656 s, after the load
  and V4's drain), `app`'s `je_allocated` is 2.3 MiB. So something that lived
  exactly as long as the load held it, and dropped it when the load stopped.
- **Not argon2.** A lone instance built from this image (`57248106…`) was
  measured after 1 authentication, then 50 concurrent ones, then 20 s idle:
  2.6, 3.0 and 2.8 MiB. The 19 MiB AUTH block is freed and not counted.
- **Not a pooled downstream buffer.** After the large messages, the heap was
  back to baseline within the minute, with two pooled connections still idle.
- **An in-flight 4 MiB message costs about 14 MiB, and gives it back.** Under
  the soak's own mix at 10 msg/s, sampled every 5 s, the heap alternated between
  2.3 and 16.3 MiB and returned to 2.3 as each large message finished.
- **So the soak did not behave like the reproduction.** There, `app2` never
  showed such a spike in 354 samples (maximum 2.2 MiB), and `app` held a flat
  20.4–21.2 MiB. The allocator settings and the 5 s stats epoch are the same in
  both. The difference is unexplained.

### What this run did not establish

- **Where the 18 MiB is.** `stats.allocated` counts bytes the allocator has
  handed out. That includes freed regions still in a thread cache, so this may
  be allocator caching rather than something simmer holds. Telling the two
  apart needs jemalloc's heap profiler (`prof`), which no build here enables.
  It is worth doing before trusting a gate on `je_allocated` if §13's
  open question makes it one.
- **Two ramps under load, and the SQL Server build.** As in §16.

## 18. OTLP export on (D-101) — SQL Server, an hour, then a control and a fix

### What was run

D-101's export on both instances, to the acceptance stack's dummy collector, on
the SQL Server build against Express, with jemalloc's counters. Three runs, each
from a stack reset with `-v`, images built from the working tree
(`feat/opentelemetry`, uncommitted; `alloc-stats` on, **not a published
build**):

| | run | image | scripts in `target/soak-runs/` |
|---|---|---|---|
| A | an hour, export on, 2026-10-03 11:19–12:42 UTC | `3f40b770…` | `2026-10-03-mssql-otel-je-hour` |
| B | 20 minutes, export **off**, the same image as A | `3f40b770…` | `2026-10-03-mssql-je-20m-otel-off` |
| C | 20 minutes, export on, after the fixes below | `146829e1…` | `2026-10-03-mssql-otel-je-20m-fixed` |

```sh
export SOAK_BACKEND=mssql SIMMER_OTEL=on
export SIMMER_CARGO_FEATURES="--features alloc-stats" SIMMER_ALLOC_STATS_FILE=/tmp/simmer-alloc-stats
```

`SIMMER_OTEL=on` is new, and the capture's twin (§11): `tests/compose/stack.rs`
layers `test/compose/otel.yml`, which gives both instances
`SIMMER_OTEL_ENDPOINT` and an instance name, and points them at the `.otel`
config twin. `test/config/Dockerfile` generates that twin by appending
`test/config/telemetry.block.yaml`. The block sets only the endpoint and
resource. Everything else is the documented default, including the 60 s
`metrics_interval`: what an operator would run. It combines with the capture
(`.capture.otel.yaml`). Three other changes were needed:

- `test/compose/mssql.yml` now appends `SIMMER_CARGO_FEATURES` to its build
  flags. Before, D-092's counters could not reach the SQL Server build.
- The collector's file exporters rotate at 256 MiB with two backups, because
  an hour is about 900 MiB of JSON.
- The collector's own counters are on `otel-collector:8888`. That is what
  accounts for what arrived.
- `otel-init` no longer empties the files: re-run under a collector that was
  already running, deleting them left it writing to unlinked inodes. D-101 has
  the detail. `tests/telemetry_compose.rs` now reads only what was exported
  after it started.

### Hour A: correct, export complete, and three problems

- **Correctness.** 36,010 of 36,010 accepted per instance, none deferred,
  refused or failed in transport. V4 as always: 220 accepted and 11 cut per
  instance, none at the dot, 0 reservations in flight after the drain.
- **The export was complete.** The collector accepted 441,986 spans, 449,748
  log records and 120,546 metric points, and refused or failed none. Neither
  instance logged an SDK warning or error. The span count is about what
  72,000 messages, the sessions and V4 should produce.
- **Memory, descriptors and threads passed** on both instances.

**Problem 1: the latency tail.** p99 was 208 and 222 ms, against §11's 76 and
73 ms on the same backend. 424 and 510 messages took over 200 ms, against 2.
The tail was not spread over the hour. Both instances had 25–35 slow messages a
minute for sixteen minutes, then it stopped abruptly: 421 and 509 of them came
in the first 20 minutes. 93% were the first message of a session.

The export is what showed where the time went. The slowest message (574 ms,
`soak-app-9621`, kept as `slow-trace-soak-app-9621.jsonl`) spent 113 ms in
`simmer.quota.resolve`, the SQL Server commit, and then **430 ms after it with
no span at all**. That gap is the post-commit read of the quota row for §9.1's
gauges (`relay.rs`), which has existed since phase 3 and had never been traced.
It now has a span, `simmer.quota.usage`.

**Problem 2: F7, a second time.** The SDK's cumulative temporality held and
re-exported every unmatched-sender series it had ever seen: 1,801 per instance
at the end, each in every 60 s export. D-093's idle expiry covers `/metrics`
only. `je_allocated` crept about 2 MiB/h, against §17's 0.5–0.64, which is the
size of that. It is bounded only by the SDK's 2,000-series-per-instrument limit.

**Problem 3: tiberius exported.** `test/compose/mssql.yml`'s `tiberius=warn`
(§11) reaches stdout only. The export filter, `telemetry.level`, defaulted to
`info`, and `RUST_LOG` deliberately does not reach it. So the export carried
tiberius's `Begin transaction` and `Commit transaction`: four lines a message,
the majority of the 449,748 records.

**A gate failure that was the harness.** `soak/rest/baseline` reported 4 tasks
and 2 unpooled sockets per instance at rest that were not there before the first
message. Both sockets were live, `ESTABLISHED` connections to the collector:
three per instance at rest, one per exporter, flat all hour. The baseline missed
two of them because `soak_run` re-creates the instances just before sampling it.
The exporters' channels connect lazily, so the fresh processes had connected
for logs, which come at startup, but not yet for spans or metrics. The run
script's own warm-up had connected the processes that were then replaced.

### Control B: the same image, export off

| 20 min | p50 | p99 | max | over 200 ms | CPU, one core | `je_allocated` |
|---|---|---|---|---|---|---|
| B, export off | 22.1 / 22.7 ms | 65.0 / 61.1 ms | 152 / 153 ms | 0 / 0 | 7.4% | 1.5–2.2 MiB |
| A, its first 20 min | — | — | 574 / 562 ms | 421 / 509 | 11.3% (the hour) | 5.9–28.2 MiB |

So the tail came with the export. The baseline checks passed: threads 3 → 3,
tasks 12 → 12, 0 unaccounted descriptors.

### The fixes, and run C

- **`telemetry.metrics_temporality`, default `delta`** (the SDK's `LowMemory`):
  an idle counter series is dropped after an export, and gauges stay cumulative.
  `cumulative` is available for backends that reject delta sums.
- **The export filter defaults to `info,sqlx=warn,tiberius=warn`**, the
  published stdout guidance.
- **`simmer.quota.usage`**, the span over the post-commit read.
- **`soak_run` waits for the exporters before its baseline** when
  `SIMMER_OTEL=on`. It makes an admin request, which produces a span, then
  waits for three `ESTABLISHED` connections to :4317 per instance, failing
  after two minutes rather than measuring a wrong baseline.

| 20 min | p50 | p99 | max | over 200 ms | CPU, one core | `je_allocated`, median |
|---|---|---|---|---|---|---|
| C, export on, fixed | 26.6 / 28.8 ms | 79.0 / 70.3 ms | 177 / 160 ms | 0 / 0 | 7.7% | 6.9 MiB |
| B, export off | 22.1 / 22.7 ms | 65.0 / 61.1 ms | 152 / 153 ms | 0 / 0 | 7.4% | 1.6 MiB |

**The tail is gone.** Against B, the export's CPU cost fell from 3.9 percentage
points of a core to 0.3. Exported log records fell to about a third (53,190 in
20 minutes) and metric points to about a fifteenth. The baseline checks passed:
tasks 22 → 22, 0 unaccounted descriptors, threads 7 → 6. With the export on, the
process has three or four more threads (the SDK's batch and reader threads) and
ten more tasks, from before its first message.

**The export's steady cost:** about 4–5 MiB of live heap (median 6.9 against
1.6 MiB), 0.3 points of CPU, and 9–14 ms on p99.

### What these runs did not establish

- **Which fix removed the tail.** The two fixes that change behaviour, the filter
  and the temporality, went in together. The trace places the time in SQL Server
  (the commit, and the read waiting behind it), not in the export path. Exporting
  four tiberius records from inside each transaction is the likeliest link, but
  it was not isolated. **Why the tail stopped at minute sixteen** is not
  explained either. A collector writing about 1 GB of JSON an hour, on a host
  disk at 98% used, competing with SQL Server's log flushes, is a candidate that
  was not tested.
- **A leak verdict with the export on.** B and C are 20 minutes, with too few
  floors. Hour A's memory, descriptor and thread gates passed, but on the code
  before the fixes.
- **Whether delta bounds F7 over hours.** The unit test shows an idle series is
  dropped. V3 mints a new domain for every message, so each series sees one
  increment and is idle from the next interval. An hour with the fixes would
  show the creep gone.
- **The ~18 MiB step of §17.** It appeared on both instances in A, on `app`
  only in C (6.2–21.0 MiB), and not in B. That is not enough runs to tie it to
  the export, and it stays unexplained.
- **The stress tier, a TLS endpoint, a real vendor.**
