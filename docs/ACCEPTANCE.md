# Acceptance harness — design

**Status: planned, not built.** This is the design for `SPEC.md` §12.3's
acceptance tier and §13.10's "acceptance suite". Nothing in this document exists
in the repository yet.

`SPEC.md` says *what* the acceptance suite must prove:

> **Acceptance** — compose stack with real Postgres; a warm-up walked across
> simulated day boundaries by manipulating `warmup.started`; verifying
> fall-through to overflow at exhaustion; verifying the cutover invariant by
> sending the same logical message under both arrangements of §1.1 and asserting
> byte-equivalent downstream output.

It does not say how. This document is the how, and the calls it makes are
recorded as `DECISIONS.md` D-032.

---

## 1. What it proves that nothing else does

Every tier below this one substitutes something. The unit tests substitute the
network, the integration tests substitute the downstream with an in-process fake,
and the quota tests substitute the relay. This tier substitutes nothing: real
Postgres, real TCP, real TLS, a real SMTP server on the other end, and Simmer in
the container it actually ships in.

Four claims can only be made here:

| Claim | Spec |
|---|---|
| A message that arrives as identity A leaves as identity B, and the recipient's mail server sees B | §1.1, §6.2 |
| A warming route carries exactly its daily allowance and not one message more, across a multi-day ramp | §7.2, §7.4 |
| Traffic beyond the allowance reaches a **different provider**, under a different identity | §3.1, §3.2 |
| Both arrangements of the cutover invariant produce byte-equivalent output | §1.1, §12.3 |

The third is the one the existing suite cannot even approximate: `tests/quota_relay.rs`
proves the *routing decision* by pointing two fake downstreams at different ports,
but it cannot prove that a real mail server on the other end receives a
well-formed message under the right identity.

---

## 2. Topology

A compose profile, so `docker compose up -d` keeps meaning what it means today
and the acceptance stack is opt-in.

```
                    ┌───────────┐
                    │  loadgen  │   sends bulk mail, records reply codes
                    └─────┬─────┘
                          │ SMTP 25 (container network only)
                    ┌─────▼─────┐
                    │  simmer   │◄──── SIMMER_WARMUP_STARTED (env)
                    └──┬─────┬──┘
              route:   │     │   :route
             warming   │     │    overflow
                 ┌─────▼──┐ ┌▼───────┐
                 │ trap-  │ │ trap-  │   Mailpit
                 │ warming│ │overflow│
                 └────┬───┘ └───┬────┘
                      │ HTTP    │ HTTP        ┌──────────┐
                      ▼         ▼             │ postgres │
                 127.0.0.1:8025 / :8026       └────┬─────┘
                                                   │ 127.0.0.1:5433
                              ┌────────────────────┴───────┐
                              │  tests/acceptance.rs (host) │
                              └─────────────────────────────┘
```

**Mailpit, settled.** MIT, actively maintained, and — the deciding factor — it can
be given a certificate and made to require STARTTLS and SMTP AUTH, which is what
lets this suite close the largest gap in `STATE.md` §6 (§5 below). MailHog is the
obvious alternative and would work, but it has had no meaningful release since
around 2020. The trap sits behind a small client in the harness either way, so the
choice is one file wide.

**Two traps, not one.** They are what makes "delivered to the correct downstream"
observable at all. With one sink you can only assert on headers Simmer wrote,
which is circular — it proves Simmer *said* it used the overflow route, not that
it did. Two separate servers make the routing decision a physical fact.

**Why `loadgen` is a container and not the host test.** §2.3 says port 25 must not
be published to a host interface, and `docker-compose.yml` deliberately does not
publish it. A host-side sender would require punching that hole in the one file
that documents why it should stay shut. Sending from inside the compose network is
also the realistic topology: an application container talking to a relay container.

The host test orchestrates and asserts. It reaches the traps' HTTP APIs and
Postgres, both of which are safe to publish to loopback.

### Services added under the `acceptance` profile

| Service | Image | Purpose |
|---|---|---|
| `trap-warming` | `axllent/mailpit`, pinned by digest | Stands in for Postal. API on `127.0.0.1:8025` |
| `trap-overflow` | `axllent/mailpit`, pinned by digest | Stands in for SendGrid. API on `127.0.0.1:8026` |
| `loadgen` | built from this repo | Bulk sender |

`simmer-db` and `app` are the existing services, with `app` given
`SIMMER_CONFIG=/app/simmer.acceptance.yaml`.

### `loadgen`

A second `[[bin]]` in this crate, **not** copied into the production runtime
image — a separate `FROM runtime AS acceptance` stage adds it, and only the
acceptance profile builds that stage. The shipped image stays exactly what it is
today.

It is a Rust binary rather than a shell script for one reason: it reads the
acceptance `simmer.yaml` through `simmer::config`, so the schedule it sends
against is *the same file Simmer reads*. A Python or `swaks` sender would need the
expected numbers written down twice, and the day they drift is the day the
acceptance suite starts lying.

Deliberately dumb: it sends, it records `(recipient, reply code, reply text)`, and
it writes that to stdout as JSON. It asserts nothing. Every assertion lives in the
host test where a failure is legible.

---

## 3. Walking the ramp

The hard part. A warm-up is weeks long and the suite has to run in a couple of
minutes.

§12.3 names the mechanism: "simulated day boundaries by manipulating
`warmup.started`". §7.2's day index is elapsed duration from a configured instant,
so moving that instant backwards moves the route forwards through its schedule.

```
for day in 0 .. schedule.len():
    SIMMER_WARMUP_STARTED = now - (day × 24h) - 1h      # 1h in, not on the seam
    docker compose up -d --force-recreate app           # §2.2: no hot reload
    wait for healthy
    reset both traps
    loadgen: send allowance[day] + margin messages
    assert
```

The config uses `started: "${SIMMER_WARMUP_STARTED}"`, which is the §4
interpolation machinery already in place — no new mechanism, and it is
substituted into the parsed tree before deserialisation, so a `DateTime` field
takes it fine. *(Worth confirming on first run; it is the one assumption here
that has never been exercised.)*

**The `- 1h` matters.** Landing exactly on a day boundary makes the test a race
against its own clock. An hour in is unambiguous.

**Restarting rather than mutating the clock** is deliberate. Changing container
time needs privileges, breaks TLS certificate validity, and would make the whole
stack lie to itself. Moving one config value is what the spec asks for and touches
nothing else.

**Day rows do not collide.** Each simulated day is a different `day_index`, so
each gets its own `quota_usage` row. Walking 0→7 creates seven rows, and the ones
for days never simulated simply never exist. This also exercises D-026 for real:
each row takes the allowance from the schedule at the moment it is created.

---

## 4. Assertions

### 4.1 The ramp

Per simulated day, having sent `allowance[day] + margin`:

- `trap-warming` holds exactly `allowance[day]` messages. **Not one more** — that
  is the entire purpose of the component.
- `trap-overflow` holds exactly `margin`.
- `quota_usage` for `(warming, catchall, day_index)` shows
  `committed == allowance[day]`, `reserved == 0`.
- Every reply code the loadgen recorded is `250`.

And with the overflow route removed from the chain, the excess is `451` and
`trap-overflow` is empty — §10.3, and never `550`.

### 4.2 Routing evidence

Which container holds the message is the primary evidence. On top of that, per
message in `trap-warming`:

- `X-Simmer-Route: warming-newbrand`
- envelope sender (`ReturnPath`) matches the route's rendered `envelope_from`
- a `Received:` header naming Simmer

and the mirror in `trap-overflow`. The two must never be crossed.

### 4.3 The rewrite (§6)

Per message in `trap-warming`, against the §4.1 example config:

| Assertion | Spec |
|---|---|
| `From:` is `<display name> <sales@newbrand.com>`, display name carried through | §6.2 |
| `Message-ID:` domain is `newbrand.com` | §6.2 |
| `Reply-To:` is the *original* `From:` address | §6.2, §6.6 |
| `List-Unsubscribe` and `List-Unsubscribe-Post` present and well-formed | §6.2 |
| `Return-Path` and `X-Mailer` **absent** | `remove_headers` |
| `DKIM-Signature`, `Authentication-Results`, `ARC-*` **absent** | §6.5 |
| body links rewritten `oldbrand.com` → `newbrand.com` | §6.4 |

The §6.5 row is the one worth being loud about. It is unconditional, it has no
config switch, and a failing signature is treated more harshly by filters than an
absent one — so a regression here is invisible in every other tier and expensive
in production.

### 4.4 The cutover invariant (§1.1, §12.3)

The suite's centrepiece. Send the same logical message twice:

- **Arrangement A** — app not yet updated: `From: Jane <jane@oldbrand.com>`
- **Arrangement B** — app already cut over: `From: Jane <sales@newbrand.com>`

Both take the warming route. Fetch both raw sources from `trap-warming` and assert
byte equality after excluding, per D-002:

- headers named in the route's `unstable_headers` (`Reply-To` — it is *defined* as
  differing between these two arrangements)
- the `Received:` header Simmer prepends
- `X-Simmer-*`
- volatile template output: `Message-ID`, anything from `uuid`, `now.*`,
  `correlation_id`

If those two byte strings match, Simmer's output is exactly expressible as
application-side configuration, and it can be removed in either order. That is the
whole thesis of the component, and this is the only test that states it.

---

## 5. Gaps this closes

From `docs/STATE.md` §6:

- **No real-certificate TLS test.** Mailpit can be given a certificate and made to
  require STARTTLS. Generating a small CA in the harness, mounting it into the
  `app` container's trust store and pointing the warming route at
  `tls: required_verify` exercises the default mode against a real chain for the
  first time. The overflow route can use `tls: required` against a self-signed
  cert to cover the unvalidated mode.
- **No clock movement.** §3 is exactly this.
- **Downstream AUTH against a real server** — Mailpit supports SMTP AUTH.

It does **not** close `fail_closed` (§7.5) or §10.4 under a real `SIGTERM`. Both
are cheap to add here later — stop `simmer-db` mid-run for the first, `docker
compose stop -t 30 app` with sessions in flight for the second — but they are
failure-injection rather than acceptance and are better added once the happy path
is trustworthy.

---

## 6. Things that will bite

- **`single_recipient_only: true`** is the shipped default, so bulk means one
  transaction per recipient. The loadgen must not batch recipients unless the
  acceptance config turns the switch off.
- **Polling, not sleeping.** Assert by polling the trap API until the count is
  stable with a timeout, never by sleeping a fixed interval. This is the single
  most likely source of flakes.
- **Message order is not guaranteed.** Assert on counts and per-message content,
  never on arrival order.
- **Reset traps between simulated days**, or day 3's assertions see day 2's mail.
- **Restart cost.** Roughly eight restarts for an eight-day schedule, a few
  seconds each. The suite belongs behind `--ignored` and a compose profile, not in
  `cargo test`.
- **Mailpit's API surface is pinned from memory in this document.** Verify the
  exact paths and JSON field names against the pinned tag before writing the
  client, and pin the image by digest rather than `:latest` so the suite cannot
  break under it.
- **The trap is a test dependency, not a shipped one.** It is a container the
  harness runs, never a crate linked into the binary, so the `LICENSES.md`
  discipline does not strictly apply — but record it there anyway (MIT), because
  the next person to read that file will want to know what the image is.

---

## 7. Phasing

The suite is §13.10 work, but it does not have to arrive all at once, and waiting
until phase 10 to discover the rewrite has been wrong since phase 4 would be a bad
trade.

| | Lands | Needs |
|---|---|---|
| Compose profile, traps, loadgen, ramp walk, routing evidence (§4.1, §4.2) | **could land now** | phase 3, which is done |
| Header rewrite assertions (§4.3 less the body row) | phase 4 | the rewriting engine |
| Body rewrite assertion | phase 5 | §6.4 |
| **Cutover invariant** (§4.4) | phase 4 | the rewriting engine |
| Real-certificate TLS (§5) | phase 10 | nothing; deferred for scope |
| Failure injection (`fail_closed`, `SIGTERM`) | phase 10 | nothing; deferred for scope |

The first row is genuinely available today: the ramp works, and *which container
received the message* is evidence that needs no rewriting to be meaningful.

---

## 8. Open questions

1. **Land the ramp-walk half now, or keep the whole suite in phase 10?** Landing
   it now means phase 4 gets an acceptance harness to plug rewrite assertions
   into, rather than building both at once. It also means the ramp is proven
   end to end before more is stacked on it.
2. **Does the acceptance config mirror `simmer.yaml`, or is it its own file?** A
   separate `simmer.acceptance.yaml` is proposed — it needs `${SIMMER_WARMUP_STARTED}`,
   trap hostnames and possibly `single_recipient_only: false`, none of which belong
   in the shipped example. The cost is a second file that can drift from the first;
   a test asserting both parse and expose the same route names would contain that.
3. **CI.** The suite needs Docker and takes minutes. Run it on every push, or only
   on a tag / manual dispatch? The house `build.yml` currently has no test gate at
   all, and Simmer already added one.
