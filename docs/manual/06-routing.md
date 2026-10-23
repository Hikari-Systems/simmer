# Routing: ramps, senders and domain groups

This chapter covers how Simmer decides which route a message takes. The routes
themselves are in [Routes](07-routes.md).

For each message, Simmer:

1. picks the **ramp** (most installations have just one);
2. finds the first **sender rule** in that ramp matching the message's sender,
   which gives a **chain** of routes, or uses the `default_chain` if none matches;
3. works out the recipient's **domain group**;
4. walks the chain in order and takes the first route that is eligible for this
   message right now.

## Ramps

A ramp is one complete routing setup. Everything about routing lives inside
one:

```yaml
default_ramp: main
ramps:
  main:
    domain_groups: [ ... ]         # required
    senders: [ ... ]               # required (may be empty)
    default_chain: [overflow]      # required unless strict_senders is true
    strict_senders: false          # default
    exhausted_chain_reply: "451"   # default
    thread_affinity: false         # default; see 09-thread-affinity.md
    allow_no_overflow_route: false # default
    black_hole_allowed: false      # default
    routes: [ ... ]                # required
```

`default_ramp` names the ramp a message uses unless something chooses another.
It is required, and must name a declared ramp.

Most installations need exactly one ramp. Add another when one Simmer is warming
two unrelated things at once: two brands, two provider accounts, or two
programmes with different start dates. Ramps share nothing. Each has its own
groups, senders, routes, caps and counters, and route and group names only need
to be unique within a ramp, so two ramps can each have a route called
`overflow`. Ramp names are at most 64 characters.

```yaml
default_ramp: main
ramps:
  main:
    domain_groups: [ ... ]
    senders: [ ... ]
    routes: [ ... ]
  partner:
    domain_groups: [ ... ]
    senders: [ ... ]
    routes: [ ... ]
```

### Choosing a ramp per port or per message

With more than one ramp, there are two ways to send mail into one other than
the default.

**By port.** A listener with `ramp` routes everything it receives in that ramp:

```yaml
server:
  listeners:
    - address: "0.0.0.0:587"            # uses default_ramp
    - address: "0.0.0.0:2587"
      ramp: partner                     # everything arriving here uses `partner`
```

**By header.** A logged-in client may add an `X-Simmer-Ramp: partner` header to a
message, if its user's `grants.ramps` lists that ramp:

```yaml
server:
  auth:
    users:
      - username: "app"
        password_hash: "${SIMMER_APP_HASH}"
        grants:
          send_as: ["oldbrand.com"]
          ramps: [partner]              # may choose `partner` by header
```

The order of precedence is:

1. the listener's `ramp`, unless the listener sets
   `header_overrides_affinity: true`;
2. a permitted `X-Simmer-Ramp` header;
3. the listener's `ramp` (when `header_overrides_affinity` let the header go
   first);
4. `default_ramp`.

Things to know about the header:

- **Only logged-in clients can use it**, and only for ramps in their user's
  `grants.ramps`.
- **A header that cannot be used is ignored, never refused.** It is logged at
  `WARN` and counted in `simmer_ramp_header_rejected_total{reason}`, with
  `reason` one of `unknown`, `not_permitted`, `malformed`, `conflicting` (more
  than one header, naming different ramps) or `affinity_locked` (the listener's
  `ramp` wins), and the message is routed as if it were not there.
- **It is always removed** before the message is relayed, used or not. It is an
  instruction to Simmer and never reaches a provider.
- `simmer_ramp_selected_total{ramp,source}` counts which rule chose each
  message's ramp.

## Domain groups

Providers throttle each sender separately, so a warming route's cap is counted
separately per **domain group**: a cap of 200 means 200 to Google, 200 to
Microsoft and so on. You define the groups:

```yaml
domain_groups:
  - name: google
    domains: ["gmail.com", "googlemail.com"]
    mx: ["google.com", "googlemail.com"]
  - name: microsoft
    domains: ["outlook.com", "hotmail.com", "hotmail.co.uk", "live.com", "msn.com"]
    mx: ["mail.protection.outlook.com", "olc.protection.outlook.com"]
  - name: yahoo
    domains: ["yahoo.com", "yahoo.co.uk", "ymail.com", "aol.com"]
    mx: ["yahoodns.net"]
  - name: other
    domains: ["*"]
```

| Key | Meaning |
|---|---|
| `name` | The group's name, used in schedules, metrics and the admin API. Unique within the ramp |
| `domains` | Recipient domains, matched exactly and case-insensitively. `*` makes this the catch-all group |
| `mx` | Optional. Mail server name suffixes, used for domains no group lists (below) |

Exactly one group must contain `*`: it is where every recipient that no other
group claims goes. A domain may appear in only one group.

### Grouping by mail server

`domains` alone misses company domains whose mail is hosted by Google Workspace
or Microsoft 365, such as `acme.com` hosted at Google. Those would land in the
catch-all, so a low cap for `google` would not stop them reaching Google.

With `mx` suffixes, a recipient domain that no group lists is looked up in DNS,
and joins the first group (in the order written) with a suffix matching its
mail server. A suffix matches on whole labels: `google.com` matches
`aspmx.l.google.com` but not `notgoogle.com`.

- **No `mx` anywhere means no DNS lookups at all.** A listed domain never needs a
  lookup either.
- **Only the preferred mail servers count**: those with the lowest MX preference.
  A domain behind a filtering service with Google as its backup is judged by the
  filtering service, so it is not counted as `google`.
- **DNS never delays mail.** A failed lookup, or one that takes more than 2
  seconds, puts the recipient in the catch-all.
- **Answers are cached** for their DNS time to live, kept between 5 minutes and
  24 hours. A domain with no MX records is remembered for an hour, and a failed
  lookup for a minute.
- The catch-all group may not have `mx` suffixes.
- `simmer_mx_lookups_total{result}` counts lookups by `ok`, `cached`, `error` and
  `timeout`, and the [dry run](12-admin-api.md#the-dry-run) reports how each
  recipient was grouped (`literal`, `mx:<host>`, `fallback` or
  `fallback:mx-unavailable`).

## Sender rules

`senders` decides which chain a message uses, by its sender:

```yaml
senders:
  - match: "marketing@newbrand.com"   # one address; put it above the domain rules
    match_on: from_header
    chain: [warming-marketing, overflow]
  - match: "oldbrand.com"             # the domain
    match_on: from_header
    chain: [warming, overflow]
  - match: "*.oldbrand.com"           # its subdomains, NOT oldbrand.com itself
    match_on: from_header
    chain: [warming, overflow]
default_chain: [overflow]
```

| Key | Meaning |
|---|---|
| `match` | A domain (`oldbrand.com`), a subdomain wildcard (`*.oldbrand.com`, which does not match `oldbrand.com` itself), or an address (`jane@oldbrand.com`). Case-insensitive |
| `match_on` | Which sender to test: `envelope` (`MAIL FROM`), `from_header` (the first address in `From:`), or `either` |
| `chain` | The routes to try, in order |

**The first matching rule wins**, in the order written. There is no ranking by
how specific a rule is, so put address rules above the domain rules they would
otherwise lose to.

**Choosing `match_on`.** `from_header` is usually what you want, since `From:` is
the address recipients see and the one domain reputation follows. Its cost is
that Simmer can only decide after the whole message has arrived, so a refusal
comes after the body rather than at `RCPT TO`. With every rule on `envelope`,
Simmer decides earlier and refuses at `RCPT TO` where it can, which saves sending
a body that will be refused.

When the envelope sender and `From:` disagree, Simmer logs a `WARN` with both and
counts `simmer_sender_mismatch_total`. A message whose `From:` is missing or
unreadable is refused `550 5.6.0 malformed From header` by any rule that needs
it.

**Cover both arrangements.** During a migration, an application may already have
been switched to send as the new domain, or not yet. List a rule for each
domain, both going to the same chain, so mail is routed the same way whichever
it uses. The starter configuration does this.

### Unmatched senders: `default_chain` and `strict_senders`

A message whose sender matches no rule uses `default_chain`, which must end in an
overflow route. Simmer logs a `WARN` and counts
`simmer_unmatched_sender_total{ramp,domain}`. Alert on that counter: a typo in a
rule sends that sender's mail, unwarmed and at full volume, through the default
chain.

With `strict_senders: true`, an unmatched sender is refused instead, with
`550 5.7.1 sender domain not configured`, and `default_chain` is not needed.
That is a permanent failure, but it is about the sender rather than the
recipient, so no recipient is suppressed for it, and it should be loud, because
it means the configuration is incomplete.

## Walking the chain

For each route in the chain, in order, Simmer skips the route if:

| | Skipped when | `reason` |
|---|---|---|
| 1 | it is **paused** through the admin API | `paused` |
| 2 | its [DNS preflight](07-routes.md#dns-preflight) is failing and set to `strict` | `preflight` |
| 3 | this recipient has had too many messages from it recently ([recipient frequency](07-routes.md#recipient-frequency)) | `frequency` |
| 4 | its `warmup.started` is still in the future | `not_started` |
| 5 | its [partial ramp](08-warmup.md#spreading-a-route-across-the-day) is not offering it this message | `partial_ramp` |
| 6 | it is a warming route with **no allowance left today** for this recipient's domain group | `quota` |

The first route not skipped is reserved and used. An overflow route is never
skipped for allowance, so a chain ending in one only runs out if that route is
paused, failing a strict preflight or over a frequency limit. Each skip is
counted in `simmer_route_skipped_total{reason}`, and the dry run shows the walk
step by step.

Once a route is chosen, the message is relayed through it and only it. If its
provider fails, the application gets `451` and Simmer does not try the next
route: see [How Simmer works](02-concepts.md#never-a-permanent-failure-for-a-temporary-reason).

## When no route can take a message

If every route in the chain is skipped, the reply is `exhausted_chain_reply`:

| Value | Reply |
|---|---|
| `"451"` (default) | `451 4.7.1 no eligible route, try later`. The application retries later |
| `"550"` | `550`, permanent. The application gives up |

Leave it at `451`. A `550` here tells the application the recipient is
undeliverable, which can get a deliverable recipient suppressed in systems that
outlive Simmer, for the sake of a cap that resets tomorrow. Quote it: YAML reads
an unquoted `451` as a number, and the key wants the string.

### Ramps with no overflow route

Normally every chain ends in an overflow route, so a message always has
somewhere to go. A ramp that would rather **refuse what it cannot warm** than
send it under another identity can turn that off:

```yaml
ramps:
  transactional:
    allow_no_overflow_route: true
    black_hole_allowed: false
```

With `allow_no_overflow_route: true`:

- `default_chain` no longer has to end in an overflow route, though it is still
  required unless `strict_senders` is true.
- A message that finds no eligible route is refused
  `550 5.7.1`, saying that no overflow route is configured and the message was
  not relayed. This replaces `exhausted_chain_reply`, and it also applies on a
  day when every warming route is at its cap. It is a **permanent** failure,
  which is the point of the setting, but it can suppress recipients; choose it
  knowingly.
- The decision is made after the message body has been received, so `RCPT TO`
  is accepted.

With `black_hole_allowed: true` as well, such a message is answered
`250` and **thrown away**: not relayed, not stored. The application believes it
was sent. It is refused at startup unless `allow_no_overflow_route` is on and
`strict_senders` is off (so a mistyped sender rule cannot be accepted and
dropped), and Simmer warns about it on every start. Alert on
`simmer_no_overflow_total{outcome="discarded_black_hole"}`.

Both outcomes are counted in `simmer_no_overflow_total{ramp,outcome}`, and each
emits two OpenTelemetry events, `simmer.no_overflow.reply` and
`simmer.no_overflow.action`.
