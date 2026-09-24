# Simmer's database load — volumes and growth

What Simmer writes to its database, per message and per day, and how much of
that is net growth. Written for a DBA sizing the database Simmer points at.
Estimated from the schema (`migrations/`) and the statements Simmer runs
(`src/models/quota.rs`, `src/models/recipient_event.rs`, `src/quota/postgres.rs`),
2026-09-22, at v0.7.0. Figures are Postgres; SQL Server is covered at the end.

## Summary

For **100 messages a minute, around the clock: 144,000 messages a day.**

| | Per day |
|---|---:|
| Transactions | 288,000 (about 3.3 a second) |
| Row versions written | 720,000 – 860,000 |
| Logical bytes written (rows + index entries) | **~90 MB** (~110 MB with recipient-frequency rules) |
| **Net growth of live data** | **~1.4 KB a day** |
| One-off plateau, only with recipient-frequency rules | ~26 MB, reached within ~1.1 days, then flat |

Nearly everything Simmer writes, it removes or supersedes within seconds. What
the database accumulates is one small counter row per route and recipient
domain group per day.

**Message bodies never reach the database.** Simmer is a relay, not a queue: a
body is held in memory or on tmpfs while it is relayed and then discarded. The
4 MB body size in the example does not change any figure in this document.

## Assumptions

- **144,000 messages a day,** every one delivered. A failed delivery runs the
  same number of statements (a release in place of a commit).
- **One recipient per message.** Simmer refuses a second `RCPT TO`, so this is
  always true (`DECISIONS.md` D-047).
- **Route and domain-group names of about 16 and 8 characters** (the example
  configuration's `warming-newbrand` and `catchall`). Each extra character adds
  one byte to the rows that carry the name.
- **2 routes × 4 domain groups** for the net-growth figure, as in the example
  configuration.
- **Byte counts are logical:** heap tuples (24-byte header, alignment padding,
  4-byte line pointer) plus index entries. They are not WAL volume and not disk
  space; see "What the figures leave out".

## What one message writes

Two short transactions, whichever route carries the message. Overflow routes are
counted too, though never capped.

| Transaction | Statement | Effect |
|---|---|---|
| **reserve** | `INSERT … ON CONFLICT DO UPDATE` on `quota_usage` | locks the day's counter row; writes one new row version |
| | `UPDATE quota_usage SET reserved = reserved + 1` | one new row version |
| | `INSERT INTO quota_reservation` | one new row, plus 3 index entries |
| *(the message is relayed; nothing is written meanwhile)* | | |
| **commit** | `DELETE FROM quota_reservation` | marks the row deleted; writes no new row |
| | `UPDATE quota_usage SET committed = committed + 1` | one new row version |
| | `INSERT INTO recipient_event` | one new row, plus 2 index entries: **only if the route has a `recipient_frequency` rule** |

The `quota_usage` updates change only non-indexed columns, so Postgres can
apply them as heap-only (HOT) updates, with no index writes.

**One extra read, only under `share: {mode: auto}`.** D-097's partial ramp paces
against how full the day's cap is, so §3.2 step 3c′ reads the `quota_usage` row
before deciding — one indexed `SELECT` per message per `auto` route the walk
evaluates, outside any transaction, taking no lock and writing no row version.
A listed share, and every route with no partial ramp, read nothing. Against the
two write transactions above it is noise: at 144,000 messages a day with one
`auto` route in the chain, 144,000 extra primary-key lookups of a table holding
one row per route and group per day — which is in cache.

A reservation lasts only as long as the relay: a few seconds for a 4 MB body.
At 100 messages a minute that is a handful of `quota_reservation` rows live at
any moment.

## Row widths

| Table | Row | Index entries | Written per message |
|---|---:|---:|---:|
| `quota_usage` | ~116 B | PK ~52 B (only when a row is created) | 3 row versions = **~350 B** |
| `quota_reservation` | ~148 B | ~100 B (primary key, `expires_at`, route) | **~250 B**, deleted in the same message |
| `recipient_event` | ~76 B | ~80 B (lookup, `sent_at`) | **~155 B** |

Columns:

- `quota_usage`: two short `TEXT` keys, the day index, allowance, override,
  `committed`, `reserved` and two timestamps (`BIGINT` and `TIMESTAMPTZ`, 8 bytes
  each).
- `quota_reservation`: a `UUID`, two short `TEXT`s, the day index and count, a
  36-character correlation id, and two timestamps.
- `recipient_event`: a 16-byte keyed hash (never the address), the route name,
  and a timestamp.

## Gross writes per day

| | Rows touched per day | Logical bytes per day |
|---|---:|---:|
| `quota_usage` updates (3 per message) | 432,000 | ~50 MB |
| `quota_reservation` inserts | 144,000 | ~36 MB |
| `quota_reservation` deletes | 144,000 | none (a flag on the existing row) |
| **Total without recipient-frequency rules** | **720,000** | **~86–90 MB** |
| `recipient_event` inserts | 144,000 | ~22 MB |
| `recipient_event` deletes (hourly sweep) | ~144,000 | none |
| **Total with recipient-frequency rules** | **~1,000,000** | **~108–110 MB** |

## Net increase per day

Inserts add to the live data, deletes take it back, and an update adds only the
difference in row size, which for Simmer's fixed-width counters is none.

| Table | Inserts (+) | Deletes (−) | Updates (delta) | **Net live growth per day** |
|---|---:|---:|---:|---:|
| `quota_usage` | ≤ 8 new rows (one per route × group, the first time each is used that day) | none: rows are never deleted | 432,000, each 0 bytes (same width, new values) | **+8 rows, ~1.4 KB** |
| `quota_reservation` | +144,000 rows, ~36 MB | −144,000 rows, ~36 MB | — | **0** |
| `recipient_event`, first ~1.1 days | +144,000 rows, ~22 MB | the sweep has nothing old enough yet | — | grows to a plateau of **~165,000 rows, ~26 MB** |
| `recipient_event`, after that | +144,000 rows, ~22 MB | −144,000 rows, ~22 MB | — | **0** |
| **All tables, steady state** | | | | **~1.4 KB a day, ~0.5 MB a year** |

Where the plateau comes from: the sweeper keeps events for the longest
configured window plus 10% and runs hourly. With a daily window that is about
27 hours of events: 144,000 × ~1.14 ≈ 165,000 rows. A weekly window holds about
7.7 days' worth, roughly 1.1 million rows, or ~175 MB. No rules configured
means no rows and no sweeper at all.

`quota_usage` is the only table that grows without limit, one small row per
route and group per day. At 8 rows a day that is under 3,000 rows a year.
Removing old days is safe once they are past, if anyone wants to; Simmer only
ever reads the current day's row.

## What the figures leave out

- **WAL.** Each change also carries WAL record overhead, and the first change to
  a page after a checkpoint writes a full-page image. Expect WAL volume of
  roughly 2–4× the logical figure, so ~200–450 MB a day, more with frequent
  checkpoints. Nearly all of it is short-lived.
- **Dead tuples and vacuum.** Each superseded `quota_usage` version and each
  deleted row is dead until pruned or vacuumed. The day's counter row is
  updated about 430,000 times, so autovacuum and HOT pruning will be active on
  that small table. The space is reused rather than accumulated: the table stays
  a few pages. Default autovacuum settings suffice. A `fillfactor` below 100 on
  `quota_usage` is an optional refinement that gives HOT updates room within the
  page.
- **Reads.** Each message also reads `route_state` (a few rows) and, on a
  frequency-limited route, runs one indexed count on `recipient_event`. The
  control plane's reads are occasional.
- **Connections.** Simmer holds at most `database.max_connections` (10 by
  default) per instance.

## SQL Server

The SQL Server build runs the same statements against the same tables
(`migrations-mssql/`). Text columns are `NVARCHAR`, two bytes a character, and
row overhead differs, so expect rows roughly 1.3–1.6× wider and the byte figures
to scale to match. The row and transaction counts, and the net-growth picture,
are identical.

## Checking it on a live system

On Postgres, the cumulative counters show the real mix:

```sql
SELECT relname, n_tup_ins, n_tup_upd, n_tup_hot_upd, n_tup_del, n_live_tup, n_dead_tup
FROM pg_stat_user_tables
WHERE relname IN ('quota_usage', 'quota_reservation', 'recipient_event');

SELECT wal_records, wal_bytes, wal_fpi FROM pg_stat_wal;   -- Postgres 14+
```

Sampled a day apart, `n_tup_ins` and `n_tup_del` for `quota_reservation` should
each rise by about the day's message count; `n_tup_upd` for `quota_usage` by
three times it, nearly all HOT; and `n_live_tup` should stay flat except for
`quota_usage`'s handful of new rows.
