# One recipient per transaction — design

**Status: built.** This is implemented in the repository as of phase 6. Recorded as
`DECISIONS.md` D-047.

This is the opposite of `docs/INGRESS.md`, which proposes capability `SPEC.md`
rules out. This *removes* capability `SPEC.md` requires — a whole numbered phase of
§13 — so the case for it has to be made in the same detail.

---

## 1. What this reverses

`SPEC.md` is authoritative and is not amended silently. These are the passages this
design contradicts, quoted so the conflict is visible rather than implied:

| § | Says |
|---|---|
| **5.6** | "`single_recipient_only` defaults to **true**. … When false, the message is **split by recipient** … one downstream transaction is performed per distinct selected route" — followed by the four-row collapse table |
| **4.1** | Lists `single_recipient_only: true` in the server block, with the comment "DEFAULT true — see §5.6" |
| **13** | Phase 9: "Multi-recipient splitting and result collapse (behind the default-off switch)" |
| **9.1** | `simmer_partial_delivery_total`, which counts §5.6's mixed outcome |
| **5.5** | `max_recipients`, the ceiling on a transaction's recipient count |

Under this design, a second `RCPT TO` is refused with `452 4.5.3 multiple
recipients not permitted` — unconditionally, with no configuration that permits it.
`single_recipient_only` is deleted from the schema. §13 phase 9 is void: there is
no splitting to build and no collapse table to implement.
`simmer_partial_delivery_total` can never be nonzero and is not emitted.
`max_recipients` stays in the schema because §4.1 mandates the key, but no value
above 1 is reachable and §4.2 warns when one is set.

**This needs the spec's author to decide.** Either §5.6, §4.1, §13 and §9.1 are
amended by whoever owns them, or D-047 stands as a divergence that deletes a phase.
It is a smaller claim than D-033's — it removes rather than adds, and it makes
Simmer stricter rather than more capable — but it is the first divergence to strike
out a §13 phase entirely.

---

## 2. Why

### The collapse is lossy, and §5.6 says so itself

SMTP allows exactly one reply per transaction. A message split across three
recipients has three outcomes and one reply in which to report them, so §5.6's
table maps the outcome set onto a single code:

| Outcome across all splits | Client reply |
|---|---|
| All succeeded | `250 2.0.0 accepted` |
| All failed, any permanently | `550` with a summary |
| All failed, all temporarily | `451` with a summary |
| Mixed success and failure | `250 2.0.0 partially accepted` |

§5.6's own closing paragraph is the argument against building it:

> The client is not told which recipients failed, because SMTP provides no way to
> say so in a single reply. **This lossiness is the reason the switch defaults to
> rejecting multi-recipient messages.**

Row 4 is the sharp one. `250 partially accepted` tells a client that its message
was accepted, and the client's queue is then empty. Whichever recipients failed are
simply gone — no bounce, no retry, no record on the client side, because Simmer is
not an MTA (§2.2) and has nothing to generate a DSN from. A component whose entire
purpose is to avoid damage during a migration should not have a documented path
that silently drops mail.

### Row 2 conflicts with §14.1

> All failed, any permanently → `550`

§14.1 is the constraint that outranks convenience:

> Never emit a reply that makes a client record permanent state.

A `550` in row 2 is emitted about *the transaction*, but the client records it
against *every recipient in it*. Three recipients where one is genuinely
undeliverable produce three suppression entries, two of them wrong, in systems that
outlive Simmer by years. That was O-8, and its working assumption — `550` only when
every failure is permanent — narrows the damage without removing it: an all-failed
transaction still attributes each recipient's failure to all of them.

Refusing the transaction at `RCPT TO` avoids the question. `452` is temporary, it
is about the *limit* rather than the recipient, and no client records anything
permanent from it.

### It is a switch nobody should choose

The default is already `true`, and `simmer.yaml`, `simmer.acceptance.yaml` and
every test fixture that mattered set it or inherited it. The `false` path is a
configuration that trades an ambiguous reply and a possible silent drop for fewer
SMTP round trips on the ingress leg — inside our own network, against a relay whose
whole reason to exist is care. Keeping it as a switch means building, testing and
maintaining a path whose correct answer is "do not use this".

---

## 3. What it dissolves

Two open questions close without being answered. Both existed only inside §5.6's
world, so the right verb is *dissolved*, not *settled*:

- **O-9** — "§5.6 splits by route; §6.3 implies per-recipient splitting when a
  template references `recipient.*`." One transaction, one recipient: there is
  nothing to group and no granularity to choose between.
- **O-8** — "§5.6's collapse table returns `550` when *any* split failed
  permanently, which records permanent state about recipients that did not fail."
  There is no collapse.

And one caveat is lifted. §6.3 scopes `recipient.address`, `.local` and `.domain`
to "the single-recipient case only", which is now the only case. They always render
a real value, `config::validate`'s §6.3 warning is deleted, and — the concrete gain
— D-036's VERP-shaped spelling becomes available at no cost:

```yaml
envelope_from: "bounce+{{recipient.local}}@newbrand.com"
```

D-036 rejected `bounce+{{original.envelope_from.local}}@…` because a route that
rewrites the envelope sender cannot derive its new value from the old one and stay
stable under §6.6. `recipient.local` is derived from a field Simmer never rewrites,
so it is stable — and D-036 could only offer it "at the cost of forcing
per-recipient splitting". That cost is gone.

---

## 4. What it costs

**The ingress contract changes.** An application that batches recipients into one
transaction must be modified before it can sit behind Simmer. This is a real cost
and it is stated plainly in the README rather than discovered from a `452`.

It is not, however, a §1.1 cost. The cutover invariant constrains Simmer's
*output* — what the recipient's mail server sees, and whether it is expressible as
application-side configuration. One message per recipient is exactly what the
application keeps doing after Simmer is unplugged, so the arrangement survives the
cutover unchanged. What Simmer requires on ingress is a deployment prerequisite,
like `allowed_cidrs` or AUTH.

**`max_recipients` becomes vestigial.** §5.5's ceiling can no longer be reached: the
one-recipient rule is stricter than any value of it. The key stays because §4.1
mandates it; `reply::too_many_recipients` was deleted rather than left unreachable,
because `src/smtp/reply.rs` holds every reply Simmer can emit and a dead one in
there is exactly what its enumeration test exists to catch.

**One test lost its subject.** `several_recipients_go_out_in_one_downstream_transaction`
asserted the behaviour this removes. Its replacement asserts the new invariant from
the downstream's side: every transaction the downstream sees carries exactly one
`RCPT TO`.

---

## 5. What it does not change

- **Quota is still "per message, by the recipient count"** (§3.2). The count is now
  always 1, but `chain::walk_and_reserve` still reserves by slice length: the rule
  is about the magnitude of a reservation, not about how many recipients a
  transaction may hold, and the walk is callable directly.
- **The refusal is per `RCPT TO`, not per transaction.** RFC 5321 lets a client
  carry on with the recipients it has, so recipient one still delivers. Resetting
  the transaction would turn our limit into a delivery failure for a recipient who
  was accepted.
- **Nothing about §6.** The rewrite engine never depended on the recipient count
  except through `recipient.*`, which this widens rather than narrows.

---

## 6. Open questions

1. **Does `SPEC.md` get amended?** §5.6, §4.1's `single_recipient_only`, §13 phase 9
   and §9.1's `simmer_partial_delivery_total` all describe behaviour that no longer
   exists. This is the spec author's call, as §1 says.
2. **Should the refusal be permanent rather than temporary?** `452` is chosen for
   §14.1's sake, but it means a client that never adapts retries forever rather
   than failing loudly. The alternative — `552 5.5.3` — is exactly what §14.1
   forbids, because the client records it against the recipient it named. A third
   option, refusing at `MAIL FROM` where there is no recipient to record anything
   against, cannot work: the recipient count is not known until `RCPT TO`. `452`
   stands, but the retry loop is worth an operator's awareness.
3. **Does `max_recipients` stay in the schema at all?** It is kept for §4.1
   fidelity and warned about. If §5.6 is amended, deleting the key would be
   tidier — at the cost of one more removed-key diagnostic.
