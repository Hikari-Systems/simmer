# The warm-up schedule

Every warming route has a `warmup` block: when its day 0 begins, and how many
messages it may send each day to each domain group.

```yaml
warmup:
  started: "2026-11-02T09:00:00Z"
  schedule:
    default: [50, 100, 200, 400, 800, 1500, 3000, 5000]
    overrides:
      google:    [20, 50, 100, 250, 500, 1000, 2000, 4000]
      microsoft: [20, 50, 100, 250, 500, 1000, 2000, 4000]
    share: [0.1, 0.25, 0.5]          # optional; see below
```

| Key | Meaning |
|---|---|
| `started` | When day 0 begins, as an RFC 3339 instant with a time zone: `2026-11-02T09:00:00Z` |
| `schedule.default` | Messages allowed per day, per domain group, starting at day 0 |
| `schedule.overrides` | Optional. A different list for named domain groups |
| `schedule.share` | Optional. Offer the route only part of its traffic, to spread it across the day ([below](#spreading-a-route-across-the-day)) |

Overflow routes have no `warmup`: they are never capped.

## How days are counted

Day 0 starts at `started`, and each day is exactly 24 hours long. A route started
at 09:00 UTC moves to its next day at 09:00 UTC every day. It is not a calendar
day, so daylight saving time never makes a day 23 or 25 hours long.

- **Before `started`**, the route is not eligible at all, and its mail goes to the
  next route in the chain. You can deploy Simmer days early with a future
  `started` and nothing changes until then. Startup warns that the route has not
  started.
- **Day `n` uses entry `n`** of the list. Past the end of the list, **the last
  value repeats** for ever. A route never becomes uncapped on its own: that is a
  deliberate act, [graduating](12-admin-api.md#changing-things-without-a-restart)
  it, or raising the numbers.
- **The cap is per domain group.** With the schedule above, on day 2 the route
  may send 100 messages to `google`, 100 to `microsoft`, and 200 to each other
  group.
- **Unused allowance does not carry over.** A day the route sent nothing still
  counts as a day.
- **Only delivered messages count.** A message is reserved against the cap while
  it is being sent, and counted only if the provider accepts it. Reservation is
  exact even with many messages in flight at once, and across several Simmer
  instances sharing a database, so the cap is never overshot.

Every value must be 0 or more. A 0 means "send nothing to this group today",
which can be useful for holding back one provider.

## Choosing a schedule

There is no single right ramp; it depends on your volume, how engaged your
recipients are and how the providers react. Some guidance:

- **Start small.** A few dozen to a few hundred messages per provider per day.
- **Roughly double** every day or two while reputation holds, so a ramp to tens of
  thousands takes three to six weeks.
- **Be more careful with Google and Microsoft**, which are the strictest, by
  giving them their own, slower `overrides`.
- **Send your most engaged recipients first**, if you can choose which mail goes
  through Simmer early on: opens and replies build reputation, and complaints
  destroy it.
- **Watch, and adjust.** Google Postmaster Tools and Microsoft SNDS show the
  domain's reputation as the providers see it. If it drops, hold the ramp:
  [pause](12-admin-api.md#changing-things-without-a-restart) the route, or lower
  the numbers.

## Changing the schedule during a warm-up

Edit the lists and restart. What changes, and when:

- **Future days** use the new numbers.
- **Today** keeps the cap that was in force when today's first message was
  counted. The admin API shows both, as `allowance` (what is being enforced) and
  `scheduled` (what the configuration now says), with `drift: true` when they
  differ. That is expected for the rest of the day after a change.
- To change today's cap now, use an [allowance override](12-admin-api.md#changing-things-without-a-restart),
  which lasts until the route's next day begins.
- **Changing `started`** moves every day. Moving it later puts the route back to
  an earlier day; that is the way to restart a ramp.

## Spreading a route across the day

Without help, a warming route takes **every** message that reaches it from the
start of its day until the cap is used up, and then none. Early in a ramp, that
means the whole day's allowance goes out in a burst in the first minutes or
hours after each day starts, which is not what a provider wants to see from a
new domain. `share` offers the route only part of the traffic that reaches it,
so the cap fills more slowly and its mail is spread across the day.

A message the route is not offered goes to the next route in the chain, exactly
as if the route were full. Nothing is dropped and the cap is unchanged.

### A share per day

```yaml
schedule:
  default: [50, 100, 200, 400, 800]
  share:   [0.1, 0.25, 0.5]   # day 0: 10% of messages are offered; day 1: 25%; day 2: 50%; from day 3: all
```

- One fraction per day, from day 0, applying to every domain group. Each must be
  above 0 and at most 1.
- **Past the end of the list, every message is offered.** Unlike the caps, the
  last value does not repeat.
- Which messages are offered is decided by a keyed hash of the recipient, the
  route and the day, not at random, so every instance agrees, the dry run gives
  the real answer for a recipient, and a recipient who is offered on a given day
  is offered every time that day.
- A route with a `share` cannot be last in a chain, because the messages it is
  not offered would have nowhere to go.

The right fraction is roughly the cap divided by the traffic reaching the route,
which changes as both do. The automatic mode works that out for you.

### Automatic share

```yaml
schedule:
  default: [50, 100, 200, 400, 800]
  share:
    mode: auto
```

That is the whole feature: every other setting has a default. Simmer then
calculates the share continuously, for each domain group, from how much of
today's cap is used and how far through the day it is. When the route is ahead
of pace, the share drops; when it falls behind, the share rises.

```yaml
share:
  mode: auto
  floor:   0.05     # never offer less than this, until the tail releases it
  ceiling: 1.0      # never offer more than this
  gain:    4.0      # how sharply the share reacts to running ahead of pace
  fill_by: 0.6      # aim to fill the cap 60% of the way through the day
  tail: { below: 0.1, ceiling: 1.0 }
```

| Key | Default | Meaning |
|---|---|---|
| `floor` | `0.05` | The lowest share offered, until the tail releases it. Above 0 and at most 1, and not above `ceiling` |
| `ceiling` | `1.0` | The highest share offered. The one firm promise about the peak a provider sees: the route never takes more than this fraction of the traffic reaching it. Above 0 and at most 1 |
| `gain` | `4.0` | 1 is proportional; higher follows the pace more tightly and throttles harder when ahead. Above 0 |
| `fill_by` | `0.6` | How far through the day to aim to have filled the cap. Above 0 and at most 1. After that point the share is `ceiling` |
| `tail.below` | `0.1` | When less than this fraction of the cap remains, the throttle is released. 0 turns the release off. At least 0 and below 1 |
| `tail.ceiling` | `1.0` | The share once the tail has been released. Above 0 and at most 1 |

Two of those defaults look odd and are deliberate:

- **`fill_by` is below 1** because Simmer never delays a message: one the route is
  not offered goes down the chain at once. So Simmer cannot smooth out a sender
  that sends in bursts. It can only take a fraction of each burst. Aiming to fill
  the cap exactly at the end of the day would lose part of it whenever the
  traffic is heavier in the morning, by holding back for an afternoon that never
  comes. Aiming earlier means filling it early instead of not at all.
- **The last of the cap is released rather than floored.** A floor does not finish
  a ramp: at a share of 0.05, the last 40 messages of an 800 cap need about 800
  messages to arrive. Below `tail.below` of the cap remaining, the share opens to
  `tail.ceiling`.

Unlike a list, `auto` never ends: it applies as long as the route is warming and
not graduated. It cannot hold a route back for ever, though, because once the
cap is well above the traffic, the share sits at `ceiling`.

### Seeing it

- `/ramps/{ramp}/routes` shows each domain group's `partial_ramp_share`, or
  `null` when every message is offered.
- `simmer_partial_ramp_share{ramp,route,domain_group}` is the same as a metric.
- `simmer_route_skipped_total{reason="partial_ramp"}`, against
  `reason="quota"`, shows how much traffic is being held back on purpose against
  how much is being turned away because the cap is full.
- Replies pinned by [thread affinity](09-thread-affinity.md) are always offered,
  and a graduated route is offered everything.
