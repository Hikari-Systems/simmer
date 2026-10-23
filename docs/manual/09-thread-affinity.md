# Thread affinity

Without thread affinity, every message is routed afresh. If a recipient replies
to a message that went out as `newbrand.com`, and your application answers,
that answer may go out as `oldbrand.com` because the warming route is full
today. The recipient sees the conversation change sender mid-thread.

With thread affinity, a reply into a conversation that Simmer started goes out
through **the route that started it**, so the recipient keeps seeing the same
sender.

```yaml
ramps:
  main:
    thread_affinity: true      # default false
```

## How a reply is recognised

Nothing is stored. Every route sets its own `Message-ID:` at its own domain
(`<{{uuid}}@newbrand.com>`), so an ID that Simmer sent names the route that sent
it. When the recipient replies, their mail program puts that ID in the reply's
`In-Reply-To:` and `References:` headers. When your application answers, its
`References:` carries it forward, and Simmer reads it.

Simmer looks at the IDs in `In-Reply-To:` and then `References:`, most recent
first (up to 256). The first whose domain matches a route in the message's chain
**pins** that route: it is tried first, and the rest of the chain follows in its
usual order.

With `thread_affinity: true`, Simmer refuses to start unless every route in every
chain of the ramp sets a `Message-ID:` with a written-out domain, and no two
routes in one chain use the same domain.

## What your application must send

A correctly threaded reply (RFC 5322 §3.6.4), built from the recipient's message:

| Header on your reply | Built from their message | Rule |
|---|---|---|
| `In-Reply-To:` | its `Message-ID:` | Exactly that one ID, in angle brackets |
| `References:` | its `References:` followed by its `Message-ID:` | If it has no `References:`, use its `In-Reply-To:` (when that holds one ID) followed by its `Message-ID:`; if neither, just its `Message-ID:` |
| `Subject:` | its `Subject:` | Add `Re: ` unless it is already there |
| `To:` | its `Reply-To:` if it has one, else its `From:` | |

`From:` is your application's usual sending address, so the same sender rule,
and so the same chain, matches.

- **Carry `References:` forward on every turn.** Drop it once and the link back
  to Simmer's first ID is gone for the rest of the thread. If you must shorten a
  long one, keep the first ID and the most recent ones.
- **Copy the IDs exactly**, byte for byte.
- **Check that your inbound mail handling gives you the headers.** Some inbound
  parsers and webhooks drop `References:` and `Message-ID:` unless you ask for the
  raw headers.
- A new message that is not a reply has no `References:`, and is routed as usual.

## What a pinned reply skips, and what it does not

- **The day's cap, but only once it is used up.** A pinned reply first takes an
  ordinary slot, like any message, so replies within the cap use it up and new
  conversations move to the overflow sooner. When the cap is already used, the
  reply still goes out on its route, **past the cap, and counted**: `committed`
  then reads above `allowance` in the admin API. The allowance itself never
  changes, and every message that is not a pinned reply is still turned away at
  it. `simmer_thread_affinity_total{outcome="over_cap"}` counts these replies;
  watch it, because each is a message the schedule did not plan for.
- **The recipient frequency limit** on the pinned route: a conversation the
  recipient started is not over-mailing. The message still counts towards the
  limit for later messages.
- **The partial ramp** on the pinned route.
- **Not skipped:** a pause, a failing strict preflight, or a `started` in the
  future. Those mean the route cannot send at all, so the reply is routed as
  usual (`outcome="ineligible"`) and the thread changes sender. A provider
  failure is still a failure: there is no failover.

Only the routes in the message's own chain can be pinned.

With thread affinity on, an all-`envelope` set of sender rules no longer refuses
at `RCPT TO` (see [Routing](06-routing.md#sender-rules)): whether a message is a
reply is only known from its headers.

## Checking it

Give the [dry run](12-admin-api.md#the-dry-run) `in_reply_to` and `references`,
or a whole `message`, and its `thread` field names the route that would be
pinned. A pinned route that would go past its cap shows as selected with reason
`over_cap`.
