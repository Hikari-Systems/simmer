# How Simmer works

## The problem it solves

Mailbox providers (Gmail, Outlook, Yahoo and the rest) judge mail partly by the
reputation of the domain it comes from. A domain with no history that suddenly
sends tens of thousands of messages a day looks like a spammer, and its mail is
throttled, sent to spam or refused. The fix is to **warm it up**: send a small
volume at first and increase it day by day, so the providers see a steady,
well-behaved history build.

The usual way to do that is in the application: change its sending code to
split traffic between the old and new domain by a daily quota. Simmer does it
outside the application, so the application needs no code change and nothing
to undo afterwards.

## What happens to a message

```
application ──SMTP──▶ simmer ──SMTP──▶ provider A  (as newbrand.com, while today's allowance lasts)
                         │
                         └─────SMTP──▶ provider B  (as oldbrand.com, for everything else)
```

For each message, Simmer:

1. **accepts it** over SMTP, checking the application's login and that it may
   send as that sender;
2. **picks a route**: the first route in the sender's chain that is eligible
   right now, which for a warming route means it still has allowance today for
   this recipient's provider;
3. **rewrites the identity** (the envelope sender, `From:`, `Message-ID:` and any
   other headers or links the route names) to the route's domain;
4. **relays it** to that route's provider, holding the application's connection
   open while it does;
5. **answers the application** with the provider's verdict, and counts the
   message against the route's allowance if the provider accepted it.

## The pieces

**Route.** One way out: a provider to connect to (its *downstream*), the identity
to rewrite messages into, and, for a warming route, a daily schedule. See
[Routes](07-routes.md).

**Warming route.** A route with a `warmup` schedule: a list of daily caps,
counted from a start instant. It is skipped once today's cap is used up. See
[The warm-up schedule](08-warmup.md).

**Overflow route.** A route with `overflow: true`. It has no cap and always
takes the message, though it still counts what it sends. It is usually your
established domain on its existing provider, and it is what makes a cap safe:
mail that the warming route cannot take is still delivered.

**Chain.** An ordered list of routes, such as `[warming, overflow]`. Simmer tries
them in order and uses the first that is eligible. Any number of warming routes
come first, and at most one overflow route comes last.

**Sender rule.** Which chain a message uses, chosen by its sender. `senders` is a
list of rules such as "mail from `oldbrand.com` uses `[warming, overflow]`". The
first rule that matches wins. Mail that matches no rule uses the
`default_chain`. See [Routing](06-routing.md#sender-rules).

**Domain group.** Providers throttle independently, so a warming route's cap is
kept separately for each group of recipient domains: `google`, `microsoft`,
`yahoo`, and a catch-all for everything else. A cap of 200 means 200 to Gmail,
200 to Outlook and so on, each day. Groups can also match a company domain by
where its mail is hosted, so mail to a Google Workspace customer counts as
`google`. See [Routing](06-routing.md#domain-groups).

**Ramp.** One complete routing setup: its domain groups, sender rules, chains and
routes. Most installations have one, called `main` in the starter
configuration. Several can run side by side in one process, for two brands
warming on different schedules. See [Routing](06-routing.md#ramps).

**Warm-up day.** Day 0 begins at the route's `warmup.started` instant, and each
day is exactly 24 hours after that, so a route started at 09:00 UTC rolls over
at 09:00 UTC every day. It is not a calendar day, and daylight saving time does
not affect it. Unused allowance does not carry over. After the end of the
schedule, the last value repeats.

## Rules that shape everything

### Simmer is temporary

Simmer exists for the length of a warm-up and is then removed. Its whole design
follows from one requirement: whatever Simmer does to a message must be
something the application could do itself with a configuration change. Then you
can remove Simmer before or after reconfiguring the application and the mail
looks the same either way. In practice:

- **Rewrites are fixed values.** "Set `From:` to `jane@newbrand.com`" is allowed.
  "Append `.new` to the domain" is not, because applied to mail the application
  already sends as the new domain, it would produce `newbrand.com.new`.
- **Applying a rewrite twice changes nothing.** Simmer checks this at startup by
  running every route's rewrite on a sample message twice, and refuses to start
  if the second pass changes anything. A few headers, such as a `Reply-To:`
  that points back at the original sender, are deliberately migration-only;
  you name those in `unstable_headers`. See
  [Routes](07-routes.md#stability-why-a-rewrite-may-be-refused).
- **A route's sending domain is fixed.** The domain in `envelope_from` must be
  written out, not computed from the message, because a route exists to build
  reputation for one domain.

### Simmer is not a mail server

There is no queue, no spool and no retry. The application's SMTP connection
stays open while Simmer talks to the provider, and the application gets the
provider's answer, translated. If the provider is down, the application is told
`451` (try later) and retries the way it would with any mail server. Simmer
stores only its counters and recipient-frequency records, never messages.

It also holds no DKIM keys and signs nothing. The provider signs, exactly as if
the application were talking to it directly. That is why the provider's domain
authentication for your new domain matters so much: if the provider signs as
itself, your domain builds no reputation, however well the ramp runs.
[DNS preflight](07-routes.md#dns-preflight) checks for it.

### Never a permanent failure for a temporary reason

An SMTP `5xx` reply tells the sender something permanent, and applications act
on it: bounce handlers add the recipient to a suppression list, CRMs mark the
contact invalid. A recipient suppressed because a warming route was full that
day would stay suppressed for years after Simmer is gone. So:

- A chain with no eligible route is answered `451 4.7.1`, temporary, by default.
- A provider that is unreachable, times out or answers `4xx` is answered `451`.
- A provider that refuses the **recipient** (`5xx` to `RCPT TO`) is passed on as
  `550` with the provider's text: that is a verdict about the recipient, and it
  would have been given without Simmer too.
- A provider's `5xx` at any other stage (login, `MAIL FROM`, the message body)
  is answered `451`. Those usually mean the route is misconfigured, for example
  domain authentication that was never finished, and that is not the
  recipient's fault. They are counted in `simmer_downstream_config_error_total`,
  which is worth an alert.

There is no failover. If the chosen route's provider fails, Simmer does not try
the next route, because that would send the message under a different identity
than the one it counted. The application gets `451` and retries, and the retry
is routed afresh.

### One recipient per message

A message may have exactly one recipient. A second `RCPT TO` in the same
transaction is refused `452 4.5.3`, whatever the configuration says.

The reason is that SMTP allows one reply per message. With two recipients, one
might be accepted by the warming route's provider while the other is refused;
Simmer could then only either report a single success and lose the second
message, or report a single failure and get the first recipient suppressed.
Applications that batch recipients into one transaction must send one message
per recipient, which is what they will do anyway once Simmer is gone.
[`docs/RECIPIENTS.md`](../RECIPIENTS.md) has the long version.

## The life of a warm-up

1. **Before day 0.** Publish the new domain's DNS records and finish the
   provider's domain authentication. Deploy Simmer with `warmup.started` set to
   when you want day 0 to begin, and move the application over to Simmer.
   Until day 0, the warming route has no allowance and everything goes out as
   before through the overflow route, so the application has already been
   switched over with no change to its mail.
2. **During the ramp.** Each day the warming route takes more. Watch its
   `committed` counts, the overflow's volume, and the new domain's reputation in
   the providers' tools (Google Postmaster Tools, Microsoft SNDS). If reputation
   suffers, pause the route or lower an allowance without restarting
   ([The admin API](12-admin-api.md)).
3. **At the end.** When the warming route's cap is comfortably above your real
   volume, it is effectively taking everything. Reconfigure the application to
   send as the new domain directly through the warming route's provider, with
   the same settings Simmer was applying, and remove Simmer.
