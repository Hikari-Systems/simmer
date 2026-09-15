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
D-081, and V4 and stress S9 are now its regression checks, not yet re-run on the
stack. The planted-defect controls (§10) have begun: duplicate delivery and run B
fail exactly as planted, and run A is running. The burst/idle variants are
outstanding.** A one-page summary is at the end of `DECISIONS.md`, "Test programme
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

**Run A — a 64-byte leak per message must fail the memory slope.** It runs
separately from B, because leaked tasks cost memory of their own and would confound
it. The plant is only
`std::hint::black_box(Box::leak(vec![1u8; 64].into_boxed_slice()))` per relayed
message, about 2.2 MiB an hour at 10 msg/s, the calibration target in
`tests/compose/leak.rs`. It needs a full hour. If the gate misses it, that is a
finding about the gate's power, recorded rather than tuned away.
