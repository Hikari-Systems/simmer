# Two instances against one database — what is safe, and what is not

**Status: a correction, not a proposal.** Nothing here asks for new capability.
It reports that a claim the repository made about its own code was wrong, says
what is true instead, and shows the evidence. Recorded as `DECISIONS.md` D-061;
the tests are `tests/quota_multi_instance.rs`.

`docs/INGRESS.md` proposes capability `SPEC.md` rules out. `docs/RECIPIENTS.md`
removes capability `SPEC.md` requires. This is the third kind: `SPEC.md` §2.2
rules something out, and the implementation turns out to have ruled it out more
thoroughly than §2.2 needs — for one of the two reasons §2.2 would have to give,
and not the other.

---

## 1. What this touches

| § | Says |
|---|---|
| **2.2** | "**No multi-instance clustering.** One Simmer instance owns its quota state. Horizontal scaling is not supported in v1; the storage layer should not preclude it later." |
| **11** | "The storage layer sits behind a trait so the concrete backend can be substituted" — and, by §2.2's second clause, should not preclude horizontal scaling |

**The recommendation is that §2.2 stands.** Simmer should remain
single-instance in v1 and should stay off the spot fleet. What needs the author's
attention is the *reason*, because the repository has been giving a false one, and
because "one instance **owns its quota state**" is a stronger statement than the
code requires — the quota state is not owned by an instance at all, it is owned by
Postgres, and that is what makes it safe.

There is a live decision behind this. D-007 kept simmer off the standard house
deploy method on the strength of the false claim. That call is still right, but a
future reader weighing "can we just put this on the fleet" deserves the real
constraints, which are smaller, cheaper to fix, and nothing to do with the ramp's
counters.

---

## 2. What the repository said, and why it is wrong

D-007 and `README.md` both said:

> the fleet's roll method requires target capacity ≥2 and replaces instances one
> at a time, which would run two Simmers against one database during every deploy
> — precisely the window in which quota overshoot occurs.

Quota overshoot through the §7.4 path is **not reachable**, across instances or
otherwise.

`PgQuotaStore::reserve` opens one transaction and calls
`models::quota::lock_usage`, which is:

```sql
INSERT INTO quota_usage (route, domain_group, day_index, allowance)
VALUES ($1, $2, $3, $4)
ON CONFLICT (route, domain_group, day_index)
DO UPDATE SET updated_at = now()
RETURNING allowance, allowance_override, committed, reserved
```

`DO UPDATE` rather than `DO NOTHING` is the load-bearing detail, and the function's
own doc comment has said so since phase 3: `DO UPDATE` takes a row lock **even when
the row already existed**. The headroom check and the `reserved` increment then run
inside that same transaction, behind that lock. Contenders for one
`(route, domain_group, day_index)` row queue behind it and re-read after it — and
Postgres does that for two processes on two hosts exactly as it does for two tasks
in one process. Nothing in the path is per-instance: the counters, the
reservations and the route states are all rows.

Two neighbouring pieces are also concurrency-safe, and were written that way under
D-007's own "costs nothing now, expensive to retrofit" clause:

- **`sweep_expired`** is a single CTE. Only one `DELETE` can win a row, and the
  decrement is derived from the rows that `DELETE` actually removed, so two
  sweepers cannot double-release.
- **`release_by_ids`** (§10.4, at shutdown) is scoped to the reservation ids the
  process holds rather than truncating the table, so one instance shutting down
  does not free another's in-flight headroom.

Those two were kept as insurance. They turn out to be the difference between the
claim and the fact.

---

## 3. The evidence

`tests/quota.rs` has raced N tasks against N−1 slots since phase 3, and it is not
sufficient evidence for this claim: it races them through **one** `PgQuotaStore` on
**one** pool, and a sceptic is entitled to say the pool could be serialising them
itself.

`tests/quota_multi_instance.rs` therefore builds **two independent pools** against
one database — via `#[sqlx::test]`'s pool-options form — and wraps each in its own
store with its own lazily-resolved §7.3 salt. Two pools is as close to two
processes as an in-process test reaches: distinct connections, distinct pool state,
one server. What it does not reproduce is two *hosts*, and nothing in the mechanism
is sensitive to that, because the serialisation is done by the server, on a row.

| Test | Shows |
|---|---|
| `n_reservations_split_across_two_pools_never_overshoot` | 16 concurrent reservations, split across the two instances, against 15 slots: exactly 15 granted, `committed` 15, `reserved` 0 |
| `one_instances_open_reservation_blocks_the_others` | Instance A holds the §7.4 transaction open; instance B's `reserve` does not complete, and when it does it decides against **A's** row rather than the one it would have read |
| `two_instances_can_exceed_one_frequency_window_by_one` | §7.3's race, at C = 2 |
| `the_frequency_overshoot_is_bounded_by_concurrency_not_by_a_constant` | §7.3's race, at C = 6, reaching `threshold + (C - 1)` |
| `sequential_sends_across_two_instances_respect_the_window` | With no overlap the §7.3 window holds exactly, and instance B sees instance A's events — the salt is shared (D-050), only the read-then-write gap is not serialised |

**A green race test proves nothing on its own**, so all five were re-run against a
`lock_usage` temporarily replaced by an unlocked read. Two failed, as they must:
the N-way test granted 16 reservations against 15 slots, and the contender in the
mechanism test came back `Taken` where it must be `NoHeadroom`.

That exercise corrected the mechanism test twice, and both corrections are worth
recording because both were tests that passed for the wrong reason:

1. It first raced for a row that **did not yet exist**. Two `INSERT`s collide on
   the unique index there, and Postgres serialises them whatever the conflict
   clause says — so it passed against a deliberately broken `lock_usage`. It now
   pre-creates the row, which is the only case where `DO UPDATE` versus `DO
   NOTHING` is the difference, and the case the doc comment is about.
2. Its "the contender is still blocked" assertion is **necessary but not
   sufficient**. Against an unlocked read the contender still blocks — later, on
   the `UPDATE` inside `insert_reservation`, having already decided it had headroom
   from a stale read. The assertion with the teeth is the one about what it
   decided, not the one about whether it waited. The test says so now.

---

## 4. What genuinely constrains two instances

Two things, neither fixed by a lock, and neither about the ramp's counters.

### 4.1 Config skew during a roll

`quota_usage.allowance` is authoritative once written (D-026): a config change
applies from the next day boundary, not retroactively. So during a roll, two
instances running different schedules for the minutes a replacement takes will have
**whichever writes the day's first row set that day's ceiling**, and the other will
silently honour it.

There is no lock that fixes this, because nothing is racing — the two instances
simply disagree about what the schedule says, and D-026 makes the first writer
win. It is inherent in D-026 and belongs in the deployment constraints. It is also
the more operationally surprising of the two: the ramp is the product, and a deploy
that silently pins today to yesterday's allowance is a bad way to find out.

### 4.2 The recipient-frequency race (D-049)

§7.3's recipient-event count is read **outside** the reservation transaction, in
`routing::chain::walk_and_reserve`. Sends whose reads all land before the first of
them commits all see room and all take it.

**The bound, stated exactly** — because it is easy to state wrongly, and an earlier
draft of this correction did. It is *not* "one extra message". With `C` sends whose
§7.3 reads land before the first commit, the window reaches:

```
threshold + (C - 1)
```

One extra per concurrent send, bounded by peak concurrency against a single
recipient key and by nothing else. `C = 2` is the two-instance case people picture;
it is not the general one. The tests pin `C = 2` and `C = 6` so this document and
the code cannot drift apart quietly.

**Settled 2026-08-10: this stays where it is.** Moving the check inside the
reservation transaction would mean holding the ramp's hot row lock across a
high-cardinality index read on *every message*, which costs more than the messages
it would save. §7.3 is a reputation-shaping heuristic, not an accounting invariant
like quota. §7.3's own framing supports that reading: being over threshold makes a
route ineligible so the message *steers*, and nothing is ever dropped by it — so
the failure mode of the race is one extra message on a warming route per concurrent
send, not a lost message and not a §14.1 violation.

Note that this is **already true within a single instance**. A second instance
widens the window; it does not introduce the bug. Anyone reading this as a reason
to stay single-instance should notice that it argues equally for capping
concurrency, which nobody proposes.

---

## 5. What would have to be true to run two

Not a proposal — a checklist, so the answer is on record if the question is asked.

1. **Deploy without config skew.** Either roll with an unchanged `routes:` block,
   or accept that a schedule change lands on the next day boundary after the roll
   rather than the first. Nothing in the code needs to change; the constraint needs
   to be stated where a deployer will see it.
2. **Accept the §7.3 bound**, at `threshold + (C - 1)` for peak concurrency `C`
   against one recipient key. For the thresholds simmer configures — small integers
   per day — this is a rounding error against the reputation goal it serves.
3. **Nothing else.** No `instance_id` column, no advisory locks, no leader
   election, and specifically **no lock table and no coarser lock**: a table-level
   lock would serialise every route and domain group against each other, which the
   row lock deliberately does not, and would add nothing to correctness that the
   row lock does not already provide.

The §7.4 protocol, the sweeper and the shutdown release need no changes at all.
That is the finding, and it is a stronger position than §2.2 assumes.

---

## 6. What this does not change

- **§2.2 stands.** Simmer is single-instance in v1 and stays off the spot fleet.
- **D-007's decision stands.** Only its stated reason was wrong.
- **No code changed.** This piece added tests and corrected three documents. The
  storage layer was already what §2.2's second clause asks for — "the storage layer
  should not preclude [horizontal scaling] later" — and rather more so than the
  repository realised.
- **§7.5 fail-closed, §10.4 shutdown release and the sweepers** are untouched.

---

## 7. Open questions for the spec's author

1. **Does §2.2's wording want amending?** "One Simmer instance owns its quota
   state" describes an ownership model the code does not have and does not need.
   Postgres owns it. A form of words like *"v1 runs a single instance; the storage
   layer is safe for more"* would say what is true without granting the capability.
2. **Is the §7.3 bound acceptable as a documented property**, rather than a defect?
   D-049 assumed so and this piece has now measured it. If it is not, the fix is not
   a lock but a lower-cost check — and the cost analysis would need redoing.
3. **Should the two constraints in §4 be written into §2.3's deployment
   assumptions?** They are the real operational constraints on running simmer, and
   they are currently recorded only in `DECISIONS.md`.
