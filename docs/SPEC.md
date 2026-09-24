# Simmer — Feature Specification

**Status:** draft for implementation
**Artefact:** `simmer` — a Rust SMTP relay facade, shipped as a `bookworm-slim` Docker container
**Audience:** implementers (human or agentic)

---

## 1. Purpose

When a domain begins sending bulk mail for the first time, its volume must be ramped
gradually. Sudden appearance at scale is one of the strongest negative signals available to
mailbox providers and reputation services such as Spamhaus. The remedy is a warm-up: a
scheduled daily ceiling that rises over weeks until the domain is established.

Implementing that ramp inside every sending application is invasive and temporary. Simmer
externalises it. It is an SMTP relay that sits between an application and one or more real
SMTP providers ("downstreams" — SendGrid, Postal, and similar). It accepts a message,
selects an outbound route according to quota state, rewrites the message's identity to match
that route, forwards it, and returns the downstream's verdict to the client on the same
connection.

Simmer is **temporary infrastructure**. It exists for the duration of a warm-up and is then
removed. Every design decision below is subordinate to that.

### 1.1 The cutover invariant

For any route, Simmer's output identity must be exactly expressible as application-side
configuration. Removal can therefore happen in either order:

- **App updated first.** The application is reconfigured to send as the new identity; Simmer
  now matches that identity and passes it through unchanged; Simmer is unplugged.
- **Simmer removed first.** Simmer has been producing the target identity all along; the
  application is reconfigured to produce it directly.

Neither ordering may change what the recipient sees. Two arrangements are therefore equally
first-class and the engine must not privilege either:

| | Incoming identity | Warming route | Overflow route |
|---|---|---|---|
| **App unchanged** | `oldbrand.com` | rewrite → `newbrand.com` | rewrite → `oldbrand.com` (or pass through) |
| **App updated** | `newbrand.com` | pass through | rewrite → `oldbrand.com` |

Two constraints follow, both testable:

1. **Rewrites are absolute assignments, never relative transformations.** "Set the `From:`
   domain to `newbrand.com`" is permitted. "Append `.new` to the sending domain" is not,
   because applying it to already-migrated traffic corrupts it.
2. **Rewrites are idempotent.** For every route, `rewrite(rewrite(m)) == rewrite(m)`. Pass-through
   is simply the degenerate case where the target identity already equals the incoming one.

A route that happens to emit the identity it received still counts against quota. Warming
applies to the domain, not to whether Simmer altered the message.

The invariant holds per ramp (§3.4). Selecting a ramp by listener leaves nothing in the
message. Selecting one by `X-Simmer-Ramp` (§5.8) leaves a header in the *application's*
output, which Simmer always strips (§6.5), so it never reaches the downstream under either
arrangement. After cutover the application drops it, or keeps sending it to nothing.
*(Added. See `DECISIONS.md` D-099.)*

---

## 2. Scope

### 2.1 In scope

- SMTP ingress for trusted internal clients: port 25, and optionally 587 and 465, each with
  its own TLS and AUTH policy (§5.1). *(Amended — was "on port 25, plaintext". See
  `DECISIONS.md` D-070.)*
- Route selection driven by incoming identity, warm-up quota state, and per-recipient
  frequency state.
- Header, envelope, and `text/*` body rewriting per route.
- Synchronous relay to a downstream with the downstream's reply mapped back to the client.
- Postgres-backed quota accounting with reservation semantics.
- Admin HTTP API, Prometheus metrics, structured logging.
- DNS preflight validation of SPF/DKIM/DMARC for warming routes.
- An optional HTTP/1.x link proxy that forwards the tracking and unsubscribe links in rewritten
  messages to one upstream (§5.7). *(Added. See `DECISIONS.md` D-083.)*
- Optional thread affinity: an outbound reply into a conversation Simmer started leaves via the
  route that started it (§3.2 step 2a). *(Added. See `DECISIONS.md` D-090.)*
- Several named ramps in one process, each an isolated routing profile, selected per message by
  the listener it arrived on or by an `X-Simmer-Ramp` header (§3.4, §5.8). *(Added. See
  `DECISIONS.md` D-099.)*

### 2.2 Explicitly out of scope

Simmer is **not an MTA**. It does not own messages.

- **No spool, no queue, no retry scheduler.** The client connection is held for the duration
  of the downstream conversation. A message is either delivered during that conversation or
  the client is told it was not.
- **No DSN or bounce generation.** Simmer never composes mail.
- **No inbound mail handling, no bounce processing, no reply routing.** "Reply routing" means
  the recipient's replies, which go wherever the route's `Reply-To:` or `From:` sends them and
  never pass through Simmer. The application's *outbound* replies are ordinary submissions, and
  §3.2 step 2a may route them by the message IDs they refer to. *(Clarified. See `DECISIONS.md`
  D-090.)*
- **No DKIM signing and no key material.** Downstreams sign. See §6.5.
- **No ACME and no certificate reload.** Inbound TLS uses one PEM certificate and key read at
  startup (§5.1); rotation is a restart, like any other configuration change. *(Amended — was
  "No inbound TLS". See `DECISIONS.md` D-070 and `docs/INGRESS.md`.)* The §5.7 link proxy has
  no TLS at all: a load balancer in front of it terminates HTTPS.
- **No `CHUNKING`/`BDAT`, no `DSN` extension.**
- **No multi-instance clustering.** v1 runs a single instance; the storage layer is safe for
  more. Quota state is owned by Postgres, not by a process: §7.4's reservation performs its
  headroom check and its write inside one transaction holding a row lock, which serialises
  contenders whether they are two tasks in one process or two processes on different hosts.
  What actually constrains running two is narrower and is stated in §2.3. *(Amended — the
  original wording described an ownership model the implementation does not have. See
  `DECISIONS.md` D-061 and `docs/MULTI_INSTANCE.md`.)*
- **No hot config reload.** Configuration changes require a restart.

### 2.3 Deployment assumption

Simmer runs on a trusted internal network segment. It is a submission relay for our own
applications, and TLS and AUTH do not change that: there is no inbound mail handling, no
bounce processing, no defence against hostile peers, and no rate limiting beyond §5.5. What
inbound TLS buys is the ability to sit on a segment where *cleartext credentials* are
unacceptable, which is a much smaller claim than "internet-facing". The container must not be
exposed to an untrusted network. Bind defaults to a private interface; publishing an SMTP port
to a host interface must be a deliberate act.

A listener with `auth: optional` (port 25's default) accepts unauthenticated mail from any
address in `allowed_cidrs`, and the §5.3 sender ACL does not apply to such a session. On a
segment where that is too much trust, set `auth: required` on every listener. *(Amended — was
"The listener is plaintext and accepts plaintext AUTH". See `DECISIONS.md` D-070, D-071.)*

**Two operational constraints on running more than one instance**, which §2.2 defers to here.
Neither is fixed by a lock, and neither is a quota-overshoot window:

1. **Config skew during a rolling deploy.** `quota_usage.allowance` is authoritative once
   written (`DECISIONS.md` D-026), so if two instances run different `warmup.schedule`
   values, whichever writes the day's first row sets that day's ceiling and the other
   silently honours it. Either roll with an unchanged `routes:` block, or accept that a
   schedule change takes effect on the next day boundary after the roll rather than the first.
   The same holds for everything that selects a ramp (§5.8): the `ramps:` blocks themselves,
   listener `ramp` and `header_overrides_affinity`, and `grants.ramps`. Two instances that
   disagree about any of them can send the same message under two identities depending on
   which one took it. *(Widened. See `DECISIONS.md` D-099.)*
2. **The §7.3 recipient-frequency bound.** The count is read outside the reservation
   transaction (D-049), so `C` concurrent sends that all read before any commits can take a
   window to `threshold + (C - 1)`. This is a documented property, not a defect: §7.3 is a
   reputation-shaping heuristic rather than an accounting invariant, and the bound already
   holds *within* one instance — a second instance widens the window rather than introducing
   it.

*(Added — see `docs/MULTI_INSTANCE.md`, which measures both.)*

**The upgrade that introduced ramps is stop-the-world.** It rekeys every quota table by ramp
(§11), and a pre-ramp instance against the migrated schema fails every reservation (§7.5's
`451`) rather than miscounting. Stop every old instance, start one new instance, which migrates
and adopts the old state into `default_ramp`, then start the rest. There is no downgrade path.
*(Added. See `DECISIONS.md` D-099.)*

**The link proxy is the one listener reached from an untrusted network**, and only indirectly.
Recipients click tracking and unsubscribe links from anywhere, so §5.7's listener sits behind
an internet-facing, TLS-terminating layer 7 load balancer, and that load balancer is the trust
boundary:

- `link_proxy.allowed_cidrs` is required and lists the load balancer's subnets. A connection
  from anywhere else is closed before a byte is read.
- Only the link proxy's port is placed in the load balancer's target group. An SMTP port or
  the admin port never is, so everything above about the SMTP listeners still holds.
- The target group's health check uses the admin listener's `/health`, on the internal
  segment, so the proxy reserves no path of its own.

*(Added. See `DECISIONS.md` D-083.)*

---

## 3. Domain model

### 3.1 Concepts

**Incoming identity** — the sender as the client application presents it. Purely a matching
key. Never rewritten in place; it selects a chain.

**Route** — a downstream endpoint plus an *outbound identity* to rewrite into, plus (if
warming) a ramp schedule and optional recipient-frequency constraint. Purely an output.

**Chain** — an ordered list of routes attached to a sender rule. Zero or more warming routes,
followed by **at most one** overflow route. Evaluated in order; the first eligible route wins.

**Overflow route** — a route marked `overflow: true`. It carries no warm-up schedule and is
never quota-limited. It must be last in any chain containing it. A chain may contain at most
one.

**Domain group** — a named list of literal recipient domains, and optionally of MX host
suffixes, used as the second axis of the quota key. Entirely configuration-defined; no
heuristic grouping. A group whose domain list contains `*` is the catch-all and must exist.
*(Amended — was "a named list of literal recipient domains … no MX-based or heuristic
grouping". See `DECISIONS.md` D-100.)*

**Ramp** — a named, complete routing profile: its own domain groups, sender rules, default
chain, routes and their schedules (§3.4). Every message is routed within exactly one ramp,
chosen before §3.2 runs (§5.8). *(Added. See `DECISIONS.md` D-099.)* Not to be confused with the
**partial ramp** (§7.2) or the **ramp day** (the `day_index`), both of which describe the
warm-up schedule of one route, and keep that meaning.

### 3.2 Selection algorithm

Given an accepted message with a resolved incoming identity, within the ramp §5.8 selected
for it — every rule, group, chain and route below is that ramp's:

1. **Match the sender rule.** First matching rule in configuration order wins (see §5.4). If
   no rule matches, the message is routed **directly to the overflow route** of a designated
   default chain; emit a `WARN` log and increment
   `simmer_unmatched_sender_total{ramp,domain}`. If `strict_senders: true`, reject instead with
   `550 5.7.1 sender domain not configured`.
2. **Resolve the recipient's domain group.** Exact, case-insensitive match of the recipient
   domain against each group's domain list. Failing that, and only if some group lists `mx`
   suffixes, look up the domain's MX records: the first group, in configuration order, one of
   whose suffixes matches a **lowest-preference** exchange host on a label boundary
   (`google.com` matches `aspmx.l.google.com`, not `notgoogle.com`). Fall back to the
   catch-all group — including when the lookup fails or does not answer within 2 seconds,
   which never defers the message. Answers are cached for their TTL, clamped to 5 minutes–24
   hours; no MX for an hour; a failure for a minute. *(Amended — the MX step is added. See
   `DECISIONS.md` D-100.)*

   2a. **Thread affinity**, when `thread_affinity: true`. Read the message IDs in
   `In-Reply-To:` and then `References:`, most recent first (at most 256). The first whose
   domain equals the literal domain of a chain route's `Message-ID:` template **pins** that
   route: it is walked first, and the rest of the chain follows in configured order. Only the
   matched chain's routes can be pinned. For the pinned route alone, step 3b is not applied
   (its §7.3 event is still recorded on commit), step 3c′ is not applied, and step 3c does
   not refuse: an ordinary
   reservation is attempted first, and only if it finds no headroom is the route reserved
   **past its cap** (§7.4). Steps 3a, §6.7's strict preflight and §7.2's future start still
   eliminate it, and the walk then continues with the next route.
3. **Walk the chain in order.** For each route:
   a. If the route is paused (admin API, §9.3), skip.
   b. If the route has a `recipient_frequency` constraint and this recipient is at or over
      threshold within the window, skip. Evaluated **first** — it can eliminate routes outright.
   c′. If the route's `warmup.schedule.share` is below 1 right now (§7.2) and this message
      is not in it, skip. Membership is a keyed hash of the normalised recipient, the ramp,
      the route and the day index under the §7.3 salt — deterministic, so every instance and §9.4's dry
      run give the same answer. *(Added. See `DECISIONS.md` D-091.)* Under `mode: auto` the
      share itself is computed from the day's usage row and the clock, so the row is read
      — without a lock, and before the reservation — to decide it. *(Added. See
      `DECISIONS.md` D-097.)*
   c. If the route is warming and has no remaining headroom for this domain group today,
      skip.
   d. Otherwise, attempt reservation (§7.4). If reservation fails due to a concurrent
      claim, re-evaluate this route once, then skip.
   e. First route to reserve successfully is selected.
4. **If no route is eligible**, reply per `exhausted_chain_reply` (default `451 4.7.1 no
   eligible route, try later`). See §10.3.

Quota is decremented **per message**, by the recipient count, not per recipient.

### 3.3 No failover

A downstream failure is a failure. Simmer does **not** fall through to the next route in the
chain on a connection error, timeout, or downstream rejection. Doing so would silently emit a
message under the wrong identity and corrupt both the ramp accounting and the reputation
being built. The reservation is released and the client is told (§10.1).

### 3.4 Ramps

*(Added. See `DECISIONS.md` D-099.)*

A ramp is a complete, independent routing profile. Configuration declares one or more under
`ramps`, keyed by name, and names one of them `default_ramp`. Each carries its own
`domain_groups`, `senders`, `default_chain`, `strict_senders`, `thread_affinity`,
`exhausted_chain_reply` and `routes` — everything §3.2 consults — so two ramps can warm two
unrelated identities on unrelated schedules from different start instants.

**Ramps share nothing.** Route and domain-group names are scoped to their ramp, so two ramps may
each have a route called `overflow`. Every piece of state is keyed by ramp as well as route:
quota rows and reservations (§7.1, §7.4), pause, graduation and allowance overrides (§9.3),
recipient-frequency history (§7.3) and the partial-ramp hash (§7.2). A thread-affinity pin
(§3.2 step 2a) can only name a route in the selected ramp's chain. Each route has its own
connection pool (§8.3), so two ramps pointing at one downstream present it with the sum of
their `max_connections`.

**What is global:** listeners, TLS and authentication (§5), which is what selects a ramp;
`dot_insensitive_domains` (§7.3), a fact about mailbox providers rather than a routing policy;
and the §7.3 salt, which is a key, not a history.

A message never changes ramp once selected, and there is no failover between ramps, for §3.3's
reason.

---

## 4. Configuration

Format is YAML. Secrets are supplied by `${ENV_VAR}` interpolation, resolved at startup;
an unresolvable reference is a fatal startup error. Configuration is validated in full at
startup and the process refuses to start on any violation (§4.2).

### 4.1 Schema by example

```yaml
server:
  listeners:                           # one per port; see §5.1 for the per-port defaults
    - address: "127.0.0.1:25"
      auth: required                   # port 25 defaults to optional
    - address: "127.0.0.1:587"         # starttls_required, auth required
      ramp: main                       # port affinity (§5.8); absent => header or default_ramp
    - address: "127.0.0.1:465"         # implicit TLS, auth required
    - address: "127.0.0.1:1025"
      auth: required
      ramp: partner
      header_overrides_affinity: true  # default false: the affinity wins over X-Simmer-Ramp
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"   # leaf first; should cover hostname
    private_key: "/etc/simmer/tls/privkey.pem"     # readable by the container's user
  hostname: "simmer.internal"          # EHLO banner, Received headers, certificate name
  max_message_bytes: 26214400          # 25 MiB
  max_recipients: 100                  # a transaction carries ONE recipient; see §5.6
  max_concurrent_sessions: 64
  allowed_cidrs: ["10.0.0.0/8", "172.16.0.0/12"]
  timeouts:
    command: 30s
    data: 300s
    session: 600s
  auth:
    allow_insecure_auth: true          # AUTH on an unencrypted session; default false
    mechanisms: [PLAIN, LOGIN]
    users:
      - username: "cfapp"
        password_hash: "${SIMMER_CFAPP_HASH}"   # argon2id; `server hash-password` mints one
        grants:
          send_as: ["oldbrand.com", "*.oldbrand.com", "newbrand.com"]   # §5.3
          ramps: [main, partner]       # may name these in X-Simmer-Ramp (§5.8); default none

database:
  url: "${DATABASE_URL}"
  max_connections: 10
  connect_timeout: 5s
  fail_closed: true                    # DB unavailable => 451 everything

admin:
  listen: "127.0.0.1:8080"
  auth_token: "${SIMMER_ADMIN_TOKEN}"
  metrics: true                        # default false; or { enabled, idle_timeout } (§9.1)

logging:
  level: info
  format: json

link_proxy:                            # optional; absent => no listener (§5.7)
  listen: "0.0.0.0:80"
  upstream: "https://link.esp.example/tracking"   # a path is a prefix; no query
  public_scheme: https                 # what the load balancer terminates
  allowed_cidrs: ["10.0.0.0/8"]        # the load balancer's subnets; required
  max_request_bytes: 1048576
  max_connections: 512
  timeouts:
    header_read: 10s
    upstream_connect: 5s
    upstream_response: 30s             # to response headers; 504 after
    idle: 60s

default_ramp: main                     # required; the ramp for a session with no affinity
                                       # and no usable header (§5.8)
ramps:                                 # each is a complete, isolated routing profile (§3.4)
  main:
    domain_groups:
      - name: google
        domains: ["gmail.com", "googlemail.com"]
        mx: ["google.com", "googlemail.com"]   # optional MX host suffixes (§3.2 step 2)
      - name: microsoft
        domains: ["outlook.com", "hotmail.com", "hotmail.co.uk", "live.com", "msn.com"]
        mx: ["mail.protection.outlook.com", "olc.protection.outlook.com"]
      - name: yahoo
        domains: ["yahoo.com", "yahoo.co.uk", "ymail.com", "aol.com"]
        mx: ["yahoodns.net"]
      - name: catchall
        domains: ["*"]                     # exactly one group must contain "*"

    senders:
      - match: "oldbrand.com"              # exact domain
        match_on: from_header              # from_header | envelope | either
        chain: [warming-newbrand, overflow-established]
      - match: "*.oldbrand.com"            # subdomain wildcard
        match_on: from_header
        chain: [warming-newbrand, overflow-established]
      - match: "marketing@newbrand.com"    # full-address match
        match_on: from_header
        chain: [warming-newbrand, overflow-established]
      - match: "newbrand.com"
        match_on: from_header
        chain: [warming-newbrand, overflow-established]

    default_chain: [overflow-established]  # used for unmatched senders unless strict_senders
    strict_senders: false
    thread_affinity: false                 # true: replies stay on the route that started the
                                           # thread (§3.2 step 2a); every route then needs a
                                           # Message-ID with its own literal domain (§4.2)

    routes:
      - name: warming-newbrand
        downstream:
          host: "smtp.postal.internal"
          port: 587
          tls: required_verify             # off | opportunistic | required | required_verify
          auth:
            username: "${POSTAL_USER}"
            password: "${POSTAL_PASS}"
          pool:
            max_connections: 4
            idle_ttl: 60s
            max_messages_per_connection: 100
          timeouts:
            connect: 10s
            command: 30s
            data: 120s
        identity:
          envelope_from: "bounce@newbrand.com"   # constant. See the note below §4.2
          set_headers:
            From: "{{original.from.display_name}} <sales@newbrand.com>"
            Reply-To: "{{original.from.address}}"
            Message-ID: "<{{uuid}}@newbrand.com>"
            List-Unsubscribe: "<mailto:unsub@newbrand.com>, <https://newbrand.com/u/{{uuid}}>"
            List-Unsubscribe-Post: "List-Unsubscribe=One-Click"
            X-Simmer-Route: "{{route.name}}"
            X-Simmer-Correlation-Id: "{{correlation_id}}"
          unstable_headers: ["Reply-To"]   # migration-only; see §6.6
          remove_headers: ["Return-Path", "X-Mailer"]
          body_rewrites:
            - pattern: 'https://oldbrand\.com/'
              replacement: "https://newbrand.com/"
          header_rewrites:                 # §6.2; between remove_headers and set_headers, so a
            - header: List-Help            # header set_headers also names (List-Unsubscribe
              pattern: '<https://oldbrand\.com/'   # above) is set, not rewritten
              replacement: "<https://click.newbrand.com/"
        preflight:
          enabled: true
          spf_include: "spf.postal.internal"
          dkim_selector: "s1"
          require_dmarc: true
        warmup:
          started: "2026-08-01T09:00:00Z"   # RFC 3339 instant, must be explicit
          schedule:
            default: [50, 100, 200, 400, 800, 1500, 3000, 5000]
            share:   [0.1, 0.25, 0.5]      # optional (§7.2, D-091); every message from day 3
            # or, computed instead of listed (§7.2, D-097) — every parameter optional:
            # share:
            #   mode: auto
            #   floor:   0.05                # never offer less, until `tail` releases it
            #   ceiling: 1.0                 # never offer more; the only promise about the peak
            #   gain:    4.0                 # how sharply it reacts to running ahead of pace
            #   fill_by: 0.6                 # meet the cap this far through the ramp day
            #   tail: { below: 0.1, ceiling: 1.0 }   # release the last tenth of the cap
            overrides:
              google:    [20, 50, 100, 250, 500, 1000, 2000, 4000]
              microsoft: [20, 50, 100, 250, 500, 1000, 2000, 4000]
        recipient_frequency:
          mode: to_address                 # to_address | to_domain
          window: { unit: daily, count: 1 } # unit: hourly | daily | weekly
          threshold: 3

      - name: overflow-established
        overflow: true
        downstream:
          host: "smtp.sendgrid.net"
          port: 587
          tls: required_verify
          auth:
            username: "apikey"
            password: "${SENDGRID_KEY}"
          pool: { max_connections: 8, idle_ttl: 60s, max_messages_per_connection: 100 }
        identity:
          envelope_from: "bounce@mail.established.com"
          set_headers:
            From: "{{original.from.display_name}} <news@mail.established.com>"
            Reply-To: "{{original.from.address}}"
            Message-ID: "<{{uuid}}@mail.established.com>"
            X-Simmer-Route: "{{route.name}}"
          unstable_headers: ["Reply-To"]   # migration-only; see §6.6

  # A second ramp: its own groups, rules, routes, schedules and quota, sharing nothing with
  # `main`. Route names are scoped per ramp, so it may reuse them.
  partner:
    domain_groups:
      - { name: catchall, domains: ["*"] }
    senders:
      - { match: "partnerbrand.com", match_on: from_header, chain: [warming, overflow] }
    default_chain: [overflow]
    routes:
      - name: warming
        # ... downstream, identity, warmup { started, schedule } as above
      - name: overflow
        overflow: true
        # ... downstream, identity
```

*(Amended — `domain_groups`, `senders`, `default_chain`, `strict_senders`, `thread_affinity`,
`exhausted_chain_reply` and `routes` were top-level keys; they now sit, unchanged, inside each
entry of `ramps`, and a configuration that still carries any of them at the top level is refused
with a pointer to the new shape. `default_ramp`, a listener's `ramp` and
`header_overrides_affinity`, and `grants.ramps` are new. See `DECISIONS.md` D-099.)*

### 4.2 Startup validation

The process must refuse to start if any of the following hold. Report **all** violations,
not just the first.

- A referenced route name does not exist.
- A chain contains more than one overflow route, or an overflow route is not last.
- An overflow route carries a `warmup` block, or a non-overflow route omits one.
- No domain group contains `*`, or more than one does.
- A domain appears in more than one group.
- An `mx` suffix appears in more than one group, is on the catch-all group, or is not a host
  name of at least two labels (no `*`, no leading or trailing `.`). *(Added. See
  `DECISIONS.md` D-100.)*
- A `warmup.schedule` array is empty, or contains a negative value.
- An `overrides` key names a nonexistent domain group.
- `admin.metrics.idle_timeout` is shorter than one minute, whether or not `admin.metrics` is
  enabled. *(Added. See `DECISIONS.md` D-093.)*
- A `warmup.schedule.share` value is not above 0 and at most 1; or a route with a non-empty
  `share` is the last route of any chain (sender rules and `default_chain`), where every
  message it is not offered would be §10.3's reply by design.
  *(Added. See `DECISIONS.md` D-091.)*
- Under `share: {mode: auto}`: `floor`, `ceiling`, `fill_by` or `tail.ceiling` is not above 0
  and at most 1; `tail.below` is below 0 or at least 1 (0 disables the release); `gain` is not
  above 0 and finite; or `floor` is above `ceiling`. The chain-position rule above applies to
  an `auto` route too, and more so: there is no day on which the gate lifts.
  *(Added. See `DECISIONS.md` D-097.)*
- `${ENV_VAR}` interpolation cannot be resolved.
- `server.listeners` is empty, or two listeners share an address.
- A listener's `tls` is not `off` and `server.tls` is absent.
- `server.tls` names a certificate or key that is missing, unreadable, unparseable, or that do
  not belong together.
- A listener's `auth` is `required` and the user list is empty.
- A listener's `auth` is `required`, its `tls` is `off`, and `allow_insecure_auth` is false —
  AUTH could never succeed there, so every message would be refused.
- A user's `grants.send_as` is empty, or contains a pattern that can match nothing (§5.3).

*(Amended — the two auth rules replace "`auth.required: true` with an empty user list" and
"`allow_insecure_auth` is false (there is no inbound TLS, so AUTH would be unusable)", which
this inverts: plaintext AUTH is now refused unless allowed. The rest are new. See
`DECISIONS.md` D-070, D-071.)*
- A `body_rewrites.pattern` fails to compile.
- A `header_rewrites` entry names an identity field (`From:`, `Sender:`, `Message-ID:`). Not
  overridable. *(Added; see `DECISIONS.md` D-089.)*
- A `header_rewrites` entry has a `header` that is not a field name, a `pattern` that fails to
  compile, or a `replacement` that refers to a group the pattern does not define or that could
  not produce an RFC 5322-conformant value. *(Added; D-089.)*
- A route's `header_rewrites` fail the stability property (§6.6) — applied to their own output
  they change it again. Not overridable by `unstable_headers`. *(Added; D-089.)*
- Any route's **identity field** (`envelope_from`, `From:`, `Sender:`, `Message-ID:`) fails the
  stability property (§6.6) against a synthetic probe message. Not overridable.
- Any other header fails the stability property and is not declared in the route's
  `unstable_headers`.
- An identity field is named in `unstable_headers`.
- `strict_senders: false` and `default_chain` is absent or its final route is not an
  overflow route.
- Any route's `identity.envelope_from` has a **domain that is not a literal** — that is, the
  part after the final `@` contains a template variable. *(Added; see the note below.)*
- `thread_affinity: true` and any route in any chain (sender rules and `default_chain`) does not
  set `Message-ID:` with a literal domain, or two different routes in one chain set the same
  `Message-ID:` domain. A route whose IDs name no route can never be pinned, and a shared domain
  would pin every reply to whichever of the two is listed first. *(Added; see `DECISIONS.md`
  D-090.)*
- Any of the following, about ramps *(added, see `DECISIONS.md` D-099)*. The rules above that
  concern routes, chains, domain groups, senders and schedules apply **within each ramp**, and a
  violation names its ramp.
  - `ramps` is empty, or `default_ramp` is absent or names no ramp.
  - A ramp's name is not 1–64 characters of `[A-Za-z0-9][A-Za-z0-9._-]*`, or two are equal.
  - Any of `domain_groups`, `senders`, `default_chain`, `strict_senders`, `thread_affinity`,
    `exhausted_chain_reply` or `routes` appears at the top level, where it was before ramps.
    The message says where it now belongs.
  - A listener's `ramp` names no ramp, or a listener sets `header_overrides_affinity: true`
    without a `ramp`, where it could have no effect.
  - A user's `grants.ramps` names a ramp that does not exist, contains `*`, or repeats a name.
- `link_proxy` is present and any of the following hold *(added, see `DECISIONS.md` D-083)*:
  - `upstream` is not an absolute `http` or `https` URI with a host, or it carries
    credentials, a query or a fragment.
  - `listen` is not a valid address, or it duplicates `admin.listen` or an SMTP listener.
  - `allowed_cidrs` is empty or contains an invalid block.
  - `max_connections`, `max_request_bytes` or any timeout is zero.

**Note on `envelope_from`, added after implementation.** The example in §4.1 originally read
`bounce+{{original.envelope_from.local}}@newbrand.com`. That is a *relative transformation* —
the outgoing address is derived from the incoming one — which §1.1 constraint 1 prohibits by
name and which the stability rule above already makes a non-overridable startup error. The
example was wrong, not the rule; it has been corrected above. A VERP-shaped intent must be
expressed as a constant plus something the downstream supplies, never as a function of the
message. See `DECISIONS.md` D-036.

The literal-domain rule is stronger than stability alone, and is separate from it:
`bounce@{{original.envelope_from.domain}}` is perfectly *stable* — applying it twice gives the
same answer — and is nonetheless incoherent for a warming route. The ramp, the daily allowance
and reputation accrual all exist to build reputation for **one** domain; a route whose domain
varies per message warms nothing, and its quota row counts a mixture of domains under a single
label. It also cannot be preflighted (§6.7), because there is no name to look up. See
`DECISIONS.md` D-069.

---

## 5. Ingress

### 5.1 Listeners

One or more listeners, each with its own `tls` and `auth` mode. The deployment assumption in
§2.3 governs all of them.

| `tls` | Behaviour |
|---|---|
| `off` | Plaintext only. `STARTTLS` is not advertised |
| `starttls` | `STARTTLS` advertised and accepted (RFC 3207); a client may decline it |
| `starttls_required` | `STARTTLS` advertised; every command but `EHLO`, `NOOP`, `RSET`, `QUIT` and `STARTTLS` is `530 5.7.0` until the handshake completes |
| `implicit` | TLS from the first byte (RFC 8314). `STARTTLS` is never advertised |

| `auth` | Behaviour |
|---|---|
| `disabled` | `AUTH` is not advertised; the command is `503` |
| `optional` | Advertised where usable; an unauthenticated session may still send |
| `required` | `MAIL FROM` before a successful `AUTH` is `530 5.7.0` |

A listener may also carry `ramp`, which gives it **port affinity**: mail arriving on it is
routed within that ramp. With `header_overrides_affinity: true` an `X-Simmer-Ramp` header the
session may use takes precedence instead; the default, `false`, is that the affinity wins. A
listener with no `ramp` routes by the header or `default_ramp`. See §5.8. *(Added. See
`DECISIONS.md` D-099.)*

A listener that names only an address takes its port's RFC defaults: 465 is `implicit` and
`required` (RFC 8314), 587 is `starttls_required` and `required` (RFC 6409), and every other
port, 25 included, is `off` and `optional` (RFC 5321).

One certificate and key, PEM, read at startup. No ACME. A certificate that does not cover
`server.hostname`, or that has expired or expires within fourteen days, is a startup warning
rather than a failure — refusing to start would take the plaintext listeners down with it.

Bytes pipelined behind `STARTTLS` — sent in cleartext after the command and before the
handshake — drop the connection rather than being processed (RFC 3207 §6). The handshake
discards the greeting, any authentication and any transaction, and keeps the §5.3 failure
count, which is per connection.

Connections are refused (TCP close, or `554` then close) if the peer address is not within
`allowed_cidrs`, or if `max_concurrent_sessions` is reached (reply `421 4.3.2 too many
connections`). Both limits are shared by every listener. On an `implicit` listener the refusal
is a bare TCP close, since a plaintext reply there would arrive in place of a TLS handshake.

*(Amended — was "Plaintext TCP, default port 25. No STARTTLS, no implicit TLS, no ACME." See
`DECISIONS.md` D-070 and `docs/INGRESS.md`.)*

### 5.2 ESMTP surface

`EHLO` advertises exactly: `PIPELINING`, `8BITMIME`, `SMTPUTF8`, `SIZE <max_message_bytes>`,
`STARTTLS` on a listener that offers it until the handshake completes, and `AUTH PLAIN LOGIN`
when AUTH could succeed on this session right now — the listener allows it, there are users,
and the session is encrypted or `allow_insecure_auth` is true. Nothing else. `HELO` is
accepted.

Commands supported: `EHLO`, `HELO`, `AUTH`, `MAIL FROM`, `RCPT TO`, `DATA`, `RSET`, `NOOP`,
`QUIT`, `STARTTLS`, `VRFY` (always `252`), `EXPN` (always `502`). `BDAT` is `502 5.5.1 command
not implemented`, and so is `STARTTLS` on a listener that does not offer it. `AUTH` on an
unencrypted session where plaintext AUTH is not allowed is `538 5.7.11`.

*(Amended — `STARTTLS` and the conditions on `AUTH` are new. See `DECISIONS.md` D-070.)*

The SMTP state machine is hand-rolled rather than delegated to a server crate. The reason is
that quota-aware responses need to be emitted at specific points in the conversation, and the
reply mapping in §10 is not expressible through a generic callback interface.

### 5.3 Authentication

`AUTH PLAIN` and `AUTH LOGIN`. Credentials are argon2id hashes in configuration. Comparison
is constant-time. Failed attempts are rate-limited per connection (three failures then `421`
and disconnect).

Authentication is **authentication only**. The authenticated username plays no part in route
selection **within a ramp**. It has one bearing on which ramp: a user's `grants.ramps` lists the
ramps it may name in `X-Simmer-Ramp` (§5.8). *(Amended — was "plays no part in route
selection". See `DECISIONS.md` D-099.)*

Each user carries `grants.send_as`: the sender identities it may present, in §5.4's pattern
grammar. For an authenticated session, the `MAIL FROM` address and the first `From:` address
must both match one of the user's patterns, or the message is refused with `550 5.7.1 sender
not permitted` — at `MAIL FROM` for the envelope, at the final dot for the header, and at the
final dot for a message with no parseable `From:`. The null sender passes `MAIL FROM` and is
judged by its `From:`. Default deny: nothing outside the grants is permitted.

The ACL **gates acceptance and never routing**, which is why the sentence above still holds: a
message it admits is routed by §5.4 exactly as it would be without it, and two users granted
the same identity — and the same `ramps` — produce byte-identical output. `grants.ramps` is not
part of the ACL: it never refuses a message, it only decides whether a header is honoured, and a
header that is not is ignored (§5.8). It applies only to sessions that
authenticated; see §2.3 for `auth: optional`. `550` is safe here for the reason §10.3 gives
for `strict_senders`: it is a statement about the sender, not the recipient.

*(Amended — the ACL is new. See `DECISIONS.md` D-071.)*

### 5.4 Sender matching

`match` accepts three forms, evaluated as written:

| Form | Example | Matches |
|---|---|---|
| Exact domain | `oldbrand.com` | `anyone@oldbrand.com` |
| Subdomain wildcard | `*.oldbrand.com` | `x@mail.oldbrand.com`; **not** `x@oldbrand.com` |
| Full address | `marketing@newbrand.com` | that address only |

Matching is case-insensitive. Rules are evaluated in configuration order; first match wins.
Full-address rules should therefore be placed above domain rules where both could match.

`match_on` selects which sender the rule tests:

- `envelope` — the domain (or address) in `MAIL FROM`.
- `from_header` — the domain (or address) of the **first** address in the `From:` header.
- `either` — matches if either does.

**Consequence:** when any applicable rule uses `from_header` or `either`, the routing decision
cannot be made until the message body has been received, so rejections land on the final dot
rather than at `RCPT TO`. This is legal and accepted. When all rules use `envelope`, Simmer
should decide early and reject at `RCPT TO` to avoid a wasted body transfer — unless
`thread_affinity` is on, since a pinned reply can take a route past its cap and whether a
message is one is in headers that have not yet arrived. *(Amended — the exception is new. See
`DECISIONS.md` D-090.)* Nor unless the ramp is already fixed at `RCPT TO` — the listener has
affinity without an override, or nothing the session may name in `X-Simmer-Ramp` could change
the ramp — since that header, too, has not yet arrived (§5.8). *(Amended. See `DECISIONS.md`
D-099.)*

If the envelope and header senders disagree, log at `WARN` with both values and increment
`simmer_sender_mismatch_total`.

A `From:` header that is absent, unparseable, or contains a group syntax with no addresses
causes `550 5.6.0 malformed From header` when `match_on` requires it.

### 5.5 Limits

`max_message_bytes` is advertised via `SIZE` and enforced during `DATA`; exceeding it yields
`552 5.3.4 message too large`. `max_recipients` yields `452 4.5.3 too many recipients`.

**The end-of-data marker is `CRLF . CRLF` and nothing else** (RFC 5321 §4.1.1.4). A `.` line
whose own line ending is a bare CR or bare LF, or whose preceding line ended with one, does
**not** end `DATA`. It is consumed as data — so nothing following it is ever executed as a
command and the session stays in step — and the message is refused `554 5.6.0 bare CR or LF
adjacent to an end-of-data marker` when the real terminator arrives. A line that would reach a
downstream carrying `<CR>.<CR>` is refused the same way: Simmer has no bare-CR line semantics,
but it forwards bare CRs verbatim, and a receiver that honours them would split the message on
the way out instead.

The refusal is of the **whole payload**. A transaction smuggled inside a message is never
separated from the message carrying it and relayed on its own, and nothing is relayed at all.
`simmer_ambiguous_terminator_total` counts it (§9.1), and §10.3 records why the reply is
permanent. *(Added. See `DECISIONS.md` D-095.)*

### 5.6 Multiple recipients

**A transaction carries exactly one recipient.** A second `RCPT TO` is rejected with
`452 4.5.3 multiple recipients not permitted`, unconditionally — there is no configuration
key and no way to enable splitting.

An application that batches recipients into one transaction must send one message per
recipient instead, which is what it will be doing in any case once Simmer is unplugged.

**Why, since this removes a capability rather than deferring one.** SMTP permits exactly one
reply per transaction, so several per-recipient outcomes must be collapsed into a single
code, and every available collapse is wrong in a way §14.1 forbids: reporting success loses
the failures silently, and reporting failure records one recipient's permanent rejection
against every other recipient in the batch — putting deliverable addresses on suppression
lists that outlive Simmer by years. A `250 partially accepted` tells the client nothing it
can act on, because SMTP gives no way to say *which* recipients failed.

Splitting also breaks the §7.4 reservation protocol's one-reservation-one-outcome shape and
would make the quota ledger depend on a collapse rule that is itself unsound.

*(Amended — this section previously specified a `single_recipient_only` switch defaulting to
true, with a result-collapse table for the false case, and §13 scheduled the splitting work
as phase 9. The switch, the table and the phase are all deleted. `simmer_partial_delivery_total`
in §9.1 is consequently unreachable and is retained only so the list matches the original.
See `DECISIONS.md` D-047 and `docs/RECIPIENTS.md`, which is the long form.)*

### 5.7 Link proxy

*(Added. See `DECISIONS.md` D-083.)*

Route rewrites point a message's tracking and unsubscribe links at a public name
(`https://click.newbrand.com/…`), and that name has to answer somewhere. When `link_proxy` is
configured, Simmer binds an HTTP listener that forwards every request to one upstream and
relays the response. Deployment is §2.3's: behind a TLS-terminating load balancer.

**Forwarding.** The request's method, path, query, body, cookies and end-to-end headers reach
the upstream unchanged. A path on `upstream` is a prefix: with
`upstream: https://link.esp.example/tracking`, a request for `/test?abc=123` is forwarded to
`/tracking/test?abc=123`. The target is always `upstream`: an absolute-form request target
contributes only its path and query, so the proxy can never be aimed at another host.

- `Host` is the upstream's.
- Hop-by-hop headers (RFC 9110 §7.6.1) are removed, and so are `Proxy-Authorization` and
  `Upgrade`.
- `X-Forwarded-For` has the peer address appended. `X-Forwarded-Host` and
  `X-Forwarded-Proto` are kept from the load balancer, or else set from `Host` and
  `public_scheme`.
- `Via: 1.1 simmer` is added. A request that already carries it is answered `508`.

**Responses** are relayed and streamed, never buffered. Three headers are rewritten to the
public origin, meaning the load balancer's `X-Forwarded-Proto` and `X-Forwarded-Host`, or else
`public_scheme` and `Host`:

- `Location` and `Content-Location`, when they name the upstream: same host
  (case-insensitive), same effective port, and inside the prefix at a segment boundary. With a
  prefix, an absolute path inside it loses the prefix, and one outside it becomes an absolute
  upstream URL.
- `Set-Cookie`: a `Domain` equal to the upstream host or a parent domain becomes the public
  host (dropped when the public host is an IP literal). With a prefix, a `Path` inside it loses
  the prefix.

Anything else, and in particular the redirect to the click's real destination, passes through
byte for byte. Response bodies are never rewritten, and redirects are never followed.

**Limits and refusals.**

- Only HTTP/1.0 and HTTP/1.1 are accepted. HTTP/2 is refused.
- `CONNECT` is answered `501`. Upgrades are never completed: a WebSocket request is forwarded
  as a plain request.
- A request whose headers are not complete within `timeouts.header_read` is disconnected.
- A body over `max_request_bytes` is `413`.
- More than `max_connections` open connections is `503`.
- An unreachable upstream, or one whose certificate does not verify against the platform root
  store, is `502`. No response headers within `timeouts.upstream_response` is `504`.

**§14.1 applied to HTTP.** Every response the proxy generates itself carries
`Cache-Control: no-store`, so a browser or intermediary never remembers a bad minute as the
answer for a link in someone's inbox. Nothing is retried. A one-click unsubscribe (RFC 8058) is
a `POST`, and replaying it is the §10.2 hazard in another protocol.

### 5.8 Ramp selection

*(Added. See `DECISIONS.md` D-099.)*

Each message is routed within exactly one ramp (§3.4), chosen at the final dot, before §3.2
runs:

1. If the listener has a `ramp` and `header_overrides_affinity` is false, that ramp.
2. Otherwise, if `X-Simmer-Ramp` gives a usable value the session may name, that ramp.
3. Otherwise, the listener's `ramp`.
4. Otherwise, `default_ramp`.

**Rights.** An authenticated session may name the ramps in its user's `grants.ramps` (§5.3),
and no others. An unauthenticated session may name none, so on `auth: optional` the header is
always ignored.

**The header's value** is trimmed and compared exactly, case-sensitively, with the ramp names.
It is **unusable** — ignored, logged at `WARN` with the value truncated to 64 characters, and
counted in `simmer_ramp_header_rejected_total{reason}` — when it:

| `reason` | |
|---|---|
| `unknown` | names no ramp |
| `not_permitted` | names a ramp the session may not name |
| `malformed` | is empty, over 64 characters, not ASCII, or an RFC 2047 encoded-word |
| `conflicting` | occurs more than once with different values; identical repeats count once |
| `affinity_locked` | names a ramp other than the listener's under rule 1 |

An unusable header is never a refusal. Selection falls through to rule 3 or 4 exactly as if
there were no header. A `550` would be §14.1's hazard for a typo, and a `451` would defer a
message that has a ramp the operator chose.

**The header is always stripped** before relay, every occurrence, whether it was used, ignored
or overridden (§6.5). It is Simmer's input, not part of the message.

---

## 6. Rewriting engine

### 6.1 Order of operations

1. Buffer the full `DATA` payload (§8.1).
2. Parse into headers and MIME structure.
3. Select the ramp (§5.8); resolve incoming identity; select route (§3.2).
4. Strip authentication artefacts and `X-Simmer-Ramp` (§6.5).
5. Apply `remove_headers`.

   5a. Apply `header_rewrites` to the named headers' decoded values (§6.2).
6. Apply `set_headers`, rendering templates.
7. Apply `body_rewrites` to `text/*` parts (§6.4).
8. Prepend a `Received:` header naming Simmer.
9. Compute the outbound envelope sender.
10. Serialise and transmit.

*(Amended — step 5a is new; it is numbered so that no existing step reference changes. See
`DECISIONS.md` D-089. Steps 3 and 4 name the ramp; see D-099.)*

### 6.2 Rewritable fields

Each is independently configurable per route, and each is an absolute assignment.

| Field | Mechanism | Notes |
|---|---|---|
| Envelope `MAIL FROM` | `identity.envelope_from` | Templated |
| `From:` | `set_headers.From` | The reputation-bearing rewrite |
| `Sender:` | `set_headers.Sender` | Should be set when `From:` is rewritten and differs from envelope |
| `Reply-To:` | `set_headers.Reply-To` | Typically `{{original.from.address}}` so replies still reach a real mailbox. Migration-only and unstable; must be declared per §6.6 |
| `Return-Path:` | `remove_headers` | Strip inbound; downstream sets it |
| `Message-ID:` | `set_headers.Message-ID` | Domain part should match the outbound sending domain |
| `List-Unsubscribe`, `List-Unsubscribe-Post` | `set_headers` | Weighted heavily by mailbox providers for bulk |
| Part of any header's value | `header_rewrites` | Regex over the decoded value; below. Not for identity fields |
| Any other header | `set_headers` / `remove_headers` | Escape hatch |
| `text/*` body content | `body_rewrites` | Regex; §6.4 |

`remove_headers` is applied first, then `header_rewrites`, then `set_headers`. A header may be
replaced by naming it in `remove_headers` and `set_headers`, and an explicit `set_headers` value
wins over a `header_rewrites` entry for the same header. Setting a header that already exists
replaces all instances.

**`header_rewrites`** covers what a template cannot: changing part of a value while keeping the
rest verbatim — the canonical case is a `List-Unsubscribe` URL whose host must move to the link
proxy (§5.7) while its per-message token is kept. Each entry names a `header` and carries
`body_rewrites`' `pattern` and `replacement`; entries apply in order, to every instance of the
header, in place.

- Matching is on the **decoded** value: unfolded, with each whitespace-delimited RFC 2047
  encoded-word decoded. §6.4's reason for rejecting raw-byte matching applies to headers too.
- A header no entry names, or one no pattern changes, keeps its original bytes. Only a changed
  value is re-serialised, with non-ASCII runs RFC 2047-encoded.
- Text captured from the message is escaped as it is substituted into the replacement.
- A header that cannot be decoded (unknown charset, malformed encoded-word, raw 8-bit bytes) or
  whose result cannot be written as a conformant field is left untouched, logged at `WARN`, and
  counted in `simmer_header_rewrite_skipped_total`.
- Templates are unaffected: `original.header[…]` is the value as it arrived.

A pattern that keeps part of the value it replaces is not an absolute assignment in §1.1's
literal sense — no more than a `body_rewrites` entry is. It satisfies §1.1 in the sense that
matters: it must be stable (§6.6), and its target state is expressible as application-side
configuration, so in arrangement B it matches nothing and the header passes through unchanged.
Identity fields are the exception, and are never rewritable this way (§4.2).

*(Amended — `header_rewrites` is new. See `DECISIONS.md` D-089.)*

### 6.3 Templating

`set_headers` values and `envelope_from` are templates. Available variables:

| Variable | Value |
|---|---|
| `original.from.address` | Full address from the first `From:` address |
| `original.from.local` | Local part |
| `original.from.domain` | Domain part |
| `original.from.display_name` | Display name, empty string if absent |
| `original.envelope_from.address`, `.local`, `.domain` | Envelope sender parts |
| `original.message_id` | Original `Message-ID`, empty if absent |
| `original.subject` | Decoded subject |
| `original.header["X-Foo"]` | Arbitrary original header, empty if absent |
| `recipient.address`, `.local`, `.domain` | Recipient (single-recipient case only) |
| `route.name` | Selected route name |
| `correlation_id` | Per-message correlation identifier |
| `uuid` | Fresh UUID v4 per render |
| `now.rfc3339`, `now.date` | Current instant / date |

Rendered header values must be RFC 5322-conformant; non-ASCII in display names is
RFC 2047-encoded automatically. A template may reference `recipient.*` freely: §5.6 makes one
recipient per transaction unconditional, so there is exactly one value to render. *(Amended —
this previously warned that such a template forces per-recipient splitting.)*

### 6.4 Body rewriting

Scope is `text/*` parts only. Attachments and non-text parts are never touched.

For each `text/*` part: decode according to `Content-Transfer-Encoding` (handling
`quoted-printable` and `base64`), decode the charset to UTF-8, apply each `body_rewrites`
entry in order as a regex replacement, re-encode, and fix up `Content-Transfer-Encoding` and
any length-bearing headers.

Raw-byte matching is explicitly rejected as an approach: a URL written across a
quoted-printable soft line break (`https://old.=\r\nbrand.com/x`) would not match, which is
the common case in real mail rather than an edge case.

If a part cannot be decoded (unknown charset, malformed encoding), leave it untouched, log at
`WARN`, and increment `simmer_body_rewrite_skipped_total`.

Signed or encrypted parts (`multipart/signed`, `multipart/encrypted`, `application/pkcs7-*`)
are never rewritten; rewriting would invalidate them.

### 6.5 DKIM, SPF, and authentication artefacts

Simmer holds **no key material and performs no signing**. The downstream signs, exactly as it
would have if the client application had connected to it directly. This follows from the
cutover invariant: whatever the downstream would have done, it still does.

Because rewriting `From:` or a body invalidates any inbound signature, and a *failing*
signature is treated more harshly by filters than an absent one, Simmer unconditionally
strips before forwarding:

- `DKIM-Signature`
- `Authentication-Results`
- `ARC-Seal`, `ARC-Message-Signature`, `ARC-Authentication-Results`

and, for a different reason, `X-Simmer-Ramp` (§5.8): it is an instruction to Simmer, and never
reaches a downstream. A route's `set_headers` could set it again; that is a startup warning.
*(Added. See `DECISIONS.md` D-099.)*

SPF requires no per-message action: it authorises the *transmitting* IP, which belongs to the
downstream, against the envelope sender's domain. It is satisfied by DNS records published for
the outbound domain, not by anything Simmer does.

**The provisioning risk this creates is the single most important operational caveat in this
document.** Domain reputation accrues to the DKIM `d=` domain. If the downstream signs with
`d=sendgrid.net` rather than `d=newbrand.com` — which is what happens until the provider's
domain authentication is completed — then a route can ramp flawlessly for weeks and build no
domain reputation whatsoever, and nothing in the mail flow would reveal it. §6.7 exists to
catch this.

### 6.6 Rewrite stability

#### What this property is for

The check below does **not** model a real execution path. A message takes exactly one route:
chain fall-through skips ineligible routes before any rewriting occurs, and §3.3 rules out
failover after a downstream failure. Two rewrites never touch one message.

The property is a mechanical test for the cutover invariant (§1.1), and specifically for
arrangement B: *if the application has already been reconfigured to send the target identity,
does this route leave it alone?* A rewrite that is a true absolute assignment answers yes, and
composing it with itself is therefore a no-op. A rewrite whose output depends on a field it
also overwrites answers no, and self-composition exposes it.

Worked example. A route sets `From: {{original.from.display_name}} <sales@newbrand.com>` and
`Reply-To: {{original.from.address}}`:

| | Client sends | Route emits |
|---|---|---|
| Before app cutover | `From: Jane Smith <jane@oldbrand.com>` | `From: Jane Smith <sales@newbrand.com>`, `Reply-To: jane@oldbrand.com` |
| After app cutover | `From: Jane Smith <sales@newbrand.com>` | `From: Jane Smith <sales@newbrand.com>`, `Reply-To: sales@newbrand.com` |

The `From:` rewrite is stable — the display name is read and written through unchanged. The
`Reply-To:` rewrite is not: it reads `From:`, which the same pass overwrites, so reconfiguring
the application silently changes what the recipient sees. That is the invariant violation the
property is designed to catch.

#### The property

For every configured route:

```
rewrite(route, rewrite(route, m)) == rewrite(route, m)
```

Enforced by a property test over generated messages and by startup validation against a
synthetic probe. Volatile template variables (`uuid`, `now.*`, `correlation_id`) are excluded
from the comparison.

#### Field classes

**Identity fields** — `identity.envelope_from`, `From:`, `Sender:`, `Message-ID:`. A stability
violation here is a **fatal startup error with no override**. These fields determine which
domain accrues reputation and how the message is attributed; instability in them means
arrangement A and arrangement B produce materially different mail, which defeats the purpose
of the component.

**All other headers** — a stability violation is a startup error by default, downgradable to a
`WARN` by naming the header in the route's `unstable_headers` list.

**`header_rewrites` and `body_rewrites`** — a violation is a fatal startup error with no
override: a pattern that matches what it just wrote has no migration-only reading, so there is
nothing for `unstable_headers` to acknowledge. *(Added. See `DECISIONS.md` D-046 and D-089.)*

#### `unstable_headers`

Some constructs are legitimately unstable because they are **migration-only**: they exist to
bridge the changeover and have no expression in the application's long-term configuration.
Reply-To preservation is the canonical case — it keeps replies reaching the original mailbox
whilst the sending identity is in flux, and it is meant to disappear when Simmer does.

Such headers must be declared:

```yaml
identity:
  set_headers:
    Reply-To: "{{original.from.address}}"
  unstable_headers: ["Reply-To"]
```

Declaring a header is an acknowledgement that its behaviour will change at the moment the
application is reconfigured, and that it is not part of the target state. Startup logs a
`WARN` naming each declared header and the route. Naming an identity field in
`unstable_headers` is a fatal configuration error.

Naming a header that is in fact stable is also a startup `WARN` — it means either the
declaration is stale or the intent was misunderstood, and both are worth surfacing.

### 6.7 DNS preflight

For each route with `preflight.enabled: true` (default true), Simmer resolves and checks the
outbound identity's domain:

| Check | Pass condition |
|---|---|
| SPF | A `v=spf1` TXT record exists and contains the configured `spf_include` |
| DKIM | `<selector>._domainkey.<domain>` resolves to a TXT record containing `p=` with a non-empty key |
| DMARC | `_dmarc.<domain>` resolves to a `v=DMARC1` record (only if `require_dmarc: true`) |

Checks run at startup and on an interval (default 15 minutes). By default a failure produces
a `WARN` log and sets `simmer_preflight_ok{route,check} 0`; it does **not** prevent startup or
block mail, because a DNS blip would otherwise become an outage. With `preflight.strict: true`
a failing check makes the route ineligible for selection, causing traffic to fall to the next
link in the chain.

---

## 7. Quota model

### 7.1 Key

```
(ramp, route, domain_group) → daily allowance
```

The route carries the outbound sending identity, which is what accrues domain reputation, so
the route is the correct first axis; the ramp in front of it only scopes the route's name, since
two ramps may each have a route of the same name (§3.4). *(Amended — was `(route,
domain_group)`. See `DECISIONS.md` D-099.)* The domain group is the second axis because mailbox
providers throttle independently of one another.

### 7.2 Warm-up day index

```
day_index = floor((now - warmup.started) / 24h)
```

An elapsed-duration calculation from the configured instant, **not** calendar arithmetic. This
is deliberate: a route started at 14:00 has its boundary at 14:00 every day, is immune to DST
transitions, and cannot produce a 23- or 25-hour window. The process timezone governs only how
dates are rendered in logs and the admin API.

`day_index` is used to index the schedule array. When it exceeds the array bounds, the
**final value repeats indefinitely**. Routes do not auto-graduate to uncapped; a warm route is
made uncapped by editing the configuration and restarting, or by removing Simmer entirely.

**Partial ramp.** A schedule may carry `share`, a list indexed by `day_index` like the caps
and applying to every domain group. On day `i`, only `share[i]` of the messages that reach
the route in the walk are offered to it (§3.2 step 3c′); the rest go to the next link. The
cap is then reached later in the day, and the route's volume is spread across it rather than
spent in its first hours. **Past the end of `share` every message is offered** — the final
value does *not* repeat, as a cap does, because a list ending below 1 would then throttle the
route forever. A graduated route (§9.3) is offered every message too. The allowance itself
is unchanged: a partial ramp decides which messages may *try* the route, never how many it may
carry. *(Added. See `DECISIONS.md` D-091.)*

**A computed share.** `share: {mode: auto}` replaces the list with a controller, evaluated
per `(ramp, route, domain_group)` at each decision:

```
c  = (allowance - used) / allowance      the cap still to fill, 1 -> 0
t' = (fill_by - elapsed) / fill_by       the fill window still to run, 1 -> 0

remaining <= tail.below * allowance  ->  tail.ceiling   (the release)
t' == 0                              ->  ceiling        (past the deadline)
otherwise      clamp(floor, ceiling, (c / t') ^ gain)
```

`allowance` is the one §7.4 will enforce — the row's once a row exists, and §9.3's override
where set — and `used` is `committed + reserved`. `elapsed` is the fraction of the route's
current day that has passed, measured from the same origin `day_index` counts from.

Three things follow from the shape. **`fill_by` is below 1 on purpose:** pacing to land at
the day boundary loses the cap whenever the traffic is front-loaded, because Simmer cannot
delay a message (§8.1) and so cannot rely on later traffic existing. A shorter window
overstates the share throughout and opens it fully once the window closes. **The tail is
released, not floored:** finishing the cap needs the share to *rise* as the remaining cap
falls, which no monotone function of the fill does; `tail.below: 0` disables the release.
**`ceiling` is the only hard promise about the peak**, since every arm resolves to it.

Unlike a list, `auto` has no end: it applies for as long as the route is warming and not
graduated. It cannot throttle a route forever by accident, because once the cap is
comfortably above the traffic the ratio sits at the ceiling and every message is offered.
Everything else is unchanged — *which* messages are offered is still the keyed hash, the
allowance is still untouched, and §3.2 step 2a's pinned reply is still exempt.
*(Added. See `DECISIONS.md` D-097.)*

An unused allowance does not carry over. A route that sent nothing yesterday still advances
its `day_index`, because the index is a function of elapsed time only.

`warmup.started` in the future makes the route ineligible until it arrives; log at startup.

### 7.3 Recipient frequency

An optional per-route constraint. Over threshold makes the route **ineligible**, so the
message falls through to the next link — it is a steering rule, not a suppression rule.
Nothing is ever dropped by it.

- `mode: to_address` — keyed on the normalised recipient address.
- `mode: to_domain` — keyed on the recipient domain.

Normalisation for `to_address`: lowercase, strip everything from `+` to `@` in the local part,
and remove dots from the local part when the domain is a known dot-insensitive provider
(configurable list, defaulting to the `google` group's domains; one list for every ramp). `Bob.Smith+news@gmail.com`
and `bobsmith@gmail.com` are the same inbox and a determined recipient will complain about
both.

The stored key is a **salted hash** of the normalised value, never plaintext. The salt is
generated once and persisted. This bounds row size and avoids the container accumulating a
plaintext record of every address mailed, which is a data-protection liability with no
operational benefit.

A thread-affinity reply is not subject to the threshold on its pinned route (§3.2 step 2a): a
message the recipient prompted by replying is not the over-mailing this rule steers away from.
Its event is recorded as usual, so the window stays true for the next message that is not a
reply. *(Added. See `DECISIONS.md` D-090.)*

Window is `count × unit` (`hourly`, `daily`, `weekly`) evaluated as a **rolling** window, so
per-event timestamps are stored rather than a counter. Events are kept per ramp and route: a
recipient mailed by one ramp is unseen by another's threshold (§3.4). *(Added. See
`DECISIONS.md` D-099.)* A sweeper evicts rows older than the
longest configured window plus a margin, on an interval.

### 7.4 Reservation protocol

Counters increment on downstream success only. A failed send must not consume allowance. That
requires a three-phase protocol, because a post-hoc increment allows two concurrent sessions to
both observe the last remaining slot:

1. **Reserve.** In one transaction: lock the usage row for `(ramp, route, domain_group, day_index)`,
   read `committed + reserved`, compare against the allowance, and if there is headroom for
   `recipient_count`, insert a reservation row and increment `reserved`. Otherwise fail.
2. **Send.** Conduct the downstream transaction, holding the client connection.
3. **Commit or release.** On downstream `2xx`, move the count from `reserved` to `committed`
   and record recipient-frequency events. On any failure, decrement `reserved` and delete the
   reservation.

Reservations carry an expiry (default: downstream timeout budget + 60s). A sweeper releases
expired reservations, covering process crashes mid-send. Expiry release is logged and
counted, since a nonzero rate indicates crashes or a mistuned timeout.

Overshoot is not acceptable on a warm-up quota — avoiding overshoot is the entire purpose of
the component — so the reservation cost is justified.

**The one exception is a thread-affinity reply on its pinned route** (§3.2 step 2a), chosen over
a conversation changing identity mid-thread. It takes an ordinary reservation while there is
headroom, so it spends the cap like any message and the ramp's own traffic spills over sooner.
Only when the ordinary reservation is refused is it reserved past the cap: under the same row
lock, incrementing `reserved`, and committed or released exactly as any other. So it is
**counted** — `committed` may end above `allowance`, and the row, the §9.2 read API and
`simmer_thread_affinity_total{outcome="over_cap"}` all say so — but the ceiling itself is never
raised, and every message that is not a pinned reply is still refused at it. A reservation past
the cap that creates the day's row writes the schedule's allowance, as any first reservation
does. *(Added. See `DECISIONS.md` D-090.)*

### 7.5 Database unavailability

`fail_closed: true` (default). If Postgres is unreachable or a reservation cannot be taken,
reply `451 4.3.0 quota service unavailable` and send nothing. A quota enforcer that stops
enforcing under failure provides no guarantee at all.

---

## 8. Outbound leg

### 8.1 Buffering

The `DATA` payload cannot be streamed through, because headers are rewritten and the routing
decision may depend on the body's `From:` header. It is buffered in memory up to a threshold
(default 1 MiB) and spilled to a temporary file above it, deleted when the session ends.

This is a transient buffer, not a spool. No durability guarantee attaches to it and it is not
recovered after a crash — at the point of a crash, no `250` has been returned to the client.

### 8.2 TLS

Per-route `tls` mode:

| Mode | Behaviour |
|---|---|
| `off` | Plaintext; never issues `STARTTLS` |
| `opportunistic` | `STARTTLS` if advertised, continue in plaintext if not or if it fails |
| `required` | `STARTTLS` mandatory; certificate not validated (self-signed internal Postal) |
| `required_verify` | `STARTTLS` mandatory with full chain and hostname validation (default) |

`rustls` with the platform root store.

### 8.3 Connection pooling

Per-route pool with `max_connections`, `idle_ttl`, and `max_messages_per_connection`. A
connection is validated with `NOOP` before reuse if idle beyond a short threshold, and
discarded on any protocol error rather than returned to the pool. `RSET` between messages on a
reused connection. The pool bounds concurrency against each downstream.

### 8.4 Timeouts

Per-stage: connect, greeting, command, data, final-dot. Each expiry maps to `451` (§10.1) and
discards the connection. Defaults are configurable; the sum of the stage budgets should be
comfortably below the client's own timeout, and this relationship should be documented in the
README.

---

## 9. Control plane

### 9.1 Metrics

Prometheus exposition on the admin listener, **when `admin.metrics` enables it**. It is off by
default: every series is held only to answer a scrape, so with it off no recorder is installed,
nothing is held, and `GET /metrics` is not served (`404`). `admin.metrics` is `true`, `false`, or
`{ enabled, idle_timeout }`. A counter not incremented for `idle_timeout` (default 24h) is
dropped from the exposition and reappears from zero on its next increment, which Prometheus reads
as an ordinary counter reset. Without that, a series whose label the client controls —
`simmer_unmatched_sender_total{domain}` — is held for the life of the process. The exporter prunes
while rendering, so an enabled endpoint that nothing scrapes prunes nothing. Leaving
`admin.metrics` unset, rather than `false`, logs a startup warning that `/metrics` is off, because
a configuration from before this amendment loads cleanly and has lost it. *(Added. See
`DECISIONS.md` D-093.)*

At minimum the list below. **Every series labelled `route` also carries `ramp`**, first, since
a route name is only unique within its ramp (§3.4); the list omits it for brevity. *(Amended.
See `DECISIONS.md` D-099.)*

- `simmer_messages_total{route,domain_group,result}` — result: `delivered`, `deferred`, `rejected`
- `simmer_quota_allowance{route,domain_group}` — today's ceiling
- `simmer_quota_committed{route,domain_group}` — used today
- `simmer_quota_reserved{route,domain_group}`
- `simmer_warmup_day{route}`
- `simmer_route_skipped_total{route,reason}` — reason: `quota`, `frequency`, `paused`, `preflight`,
  `partial_ramp` *(added, D-091)*
- `simmer_preflight_ok{route,check}`
- `simmer_downstream_latency_seconds{route}` — histogram
- `simmer_downstream_errors_total{route,class}`
- `simmer_pool_connections{route,state}`
- `simmer_unmatched_sender_total{ramp,domain}`
- `simmer_ramp_selected_total{ramp,source}` — source: `affinity`, `header`, `default` (§5.8)
- `simmer_ramp_header_rejected_total{reason}` — §5.8's reasons. Deliberately no value label:
  the value is the client's
- `simmer_sender_mismatch_total`
- `simmer_ambiguous_terminator_total` — §5.5's end-of-data rule; raised whatever else the
  message was also refused for *(added, D-095)*
- `simmer_partial_delivery_total` — unreachable since §5.6 was amended; retained for
  continuity with the original list
- `simmer_reservation_expired_total{route}`
- `simmer_body_rewrite_skipped_total{route,reason}`
- `simmer_header_rewrite_skipped_total{route,header,reason}` — reason: `malformed_encoding`,
  `unsupported_charset`, `raw_8bit`, `unrepresentable`
- `simmer_link_proxy_requests_total{status_class,origin}` — origin: `upstream`, or `proxy` for
  a response the proxy generated (§5.7)
- `simmer_link_proxy_duration_seconds` — histogram, to response headers
- `simmer_link_proxy_connections`
- `simmer_link_proxy_connections_refused_total{reason}` — reason: `cidr`, `limit`
- `simmer_thread_affinity_total{route,outcome}` — outcome: `hit` (the pinned route carried it
  within its cap), `over_cap` (past its cap), `ineligible` (the pinned route was eliminated and
  the ordinary walk decided), `unmatched` (the message refers to IDs no chain route emitted;
  `route` is `-`). A message that refers to no ID counts nothing
- `simmer_mx_lookups_total{result}` — result: `ok`, `cached`, `error`, `timeout` (§3.2 step
  2). `error` and `timeout` each put a recipient in the catch-all

*(The four `simmer_link_proxy_*` metrics are added. See `DECISIONS.md` D-083.
`simmer_header_rewrite_skipped_total` is added; see D-089. `simmer_thread_affinity_total` is
added; see D-090. The two `simmer_ramp_*` metrics and the `ramp` label are added; see D-099.
`simmer_mx_lookups_total` is added; see D-100.)*

### 9.2 Read API

- `GET /health` — liveness; includes database reachability.
- `GET /ramps` — each ramp's name, whether it is `default_ramp`, the listeners with affinity
  to it, and its routes' names. `GET /ramps/{ramp}` — one of them.
- `GET /ramps/{ramp}/routes` — configuration summary plus live state: warm-up day, allowance
  and usage per domain group, paused flag, preflight results, pool statistics, and the `share`
  list with today's share *(added, D-091)*. Each domain-group window also carries the share in
  force for it right now, which under `mode: auto` differs between groups and moves as the cap
  fills; the route-level `today` is then `null`, and the `auto` parameters are reported beside
  the mode *(added, D-097)*.
- `GET /ramps/{ramp}/routes/{name}` — as above for one route.
- `GET /quota?ramp=&route=&group=` — current window detail. `ramp` is required once more than
  one ramp is configured.
- `GET /metrics` — Prometheus, when `admin.metrics` is enabled (§9.1).

The pre-ramp paths `/routes` and `/routes/{name}`, and every write path under them, answer
`410 Gone` with the path that replaces them, rather than acting on `default_ramp`: a mutation
that silently landed on the wrong ramp is the lie this section exists to prevent. *(Amended.
See `DECISIONS.md` D-099.)*

### 9.3 Write API

All require the bearer token. All mutations are logged at `INFO` with the acting token's
identifier.

- `POST /ramps/{ramp}/routes/{name}/pause` and `/resume` — makes a route ineligible for
  selection without a restart. Persisted, so it survives restart.
- `POST /ramps/{ramp}/routes/{name}/graduate` — pins the route to the final schedule value
  immediately.
- `POST /ramps/{ramp}/routes/{name}/allowance` — override today's allowance for a domain group;
  expires at the next day boundary.
- `POST /quota/reset` — reset counters for a ramp/route/group; `ramp` is required. Destructive;
  requires an explicit confirmation field in the body.

Every mutation acts within one ramp, and the chains it reports emptied are that ramp's.
*(Amended — the paths gained `/ramps/{ramp}`. See `DECISIONS.md` D-099.)*

### 9.4 Dry run

`POST /dryrun` accepts an envelope sender, a `From:` header value, and a recipient list, and
returns the routing decision — matched sender rule, chain evaluation with a per-route
skip reason, selected route, resolved outbound envelope and headers after template rendering,
and body rewrite matches against a supplied sample body. It sends nothing and takes no
reservation.

It also accepts optional `in_reply_to` and `references` values (or reads them from a supplied
full `message`), and reports what §3.2 step 2a made of them: the pinned route or `unmatched`,
the walk in pinned order, and a pinned route that would be reserved past its cap as selected
with reason `over_cap`. *(Added. See `DECISIONS.md` D-090.)*

A route passed over by its partial ramp (§3.2 step 3c′) is reported with reason
`partial_ramp`. The hash is the real walk's, so this is the real answer for that recipient,
not an estimate. *(Added. See `DECISIONS.md` D-091.)* Under `mode: auto` it is the real
answer **as of now**: the share is computed from the same row and the same clock the walk
would read, and that row moves. This is the standard the headroom check beside it has always
held to. *(Added. See `DECISIONS.md` D-097.)*

The ramp is either given as `ramp`, or selected from `listener` (an address from
`server.listeners`), an optional `auth_user` and an optional `ramp_header`, by the same §5.8
function the session calls. The response reports the ramp, which rule chose it, and what became
of the header. *(Added. See `DECISIONS.md` D-099.)*

Each recipient's domain group is reported with the basis it was chosen on: `literal`,
`mx:<host>` (the exchange host that matched), `fallback`, or `fallback:mx-unavailable`. The
lookup is the real one and fills the same cache the walk reads. *(Added. See `DECISIONS.md`
D-100.)*

This is the primary tool for validating a configuration before it carries live traffic, and
should be treated as a first-class feature rather than a debugging afterthought.

### 9.5 Logging

Structured JSON. Every message carries a `correlation_id` propagated through every log line
and emitted as `X-Simmer-Correlation-Id` when configured. Log the ramp and which §5.8 rule chose it, the incoming identity, matched
rule, chain evaluation with skip reasons, the thread-affinity pin (§3.2 step 2a) and whether
the selected route was reserved past its cap, selected route, downstream reply code and text,
and total latency. Message bodies are never logged; recipient addresses are logged only at `DEBUG`.

A link proxy request (§5.7) is logged with its method, path, status and latency only. The query
string, cookies and body are never logged, because a tracking token identifies a recipient.
*(Added. See `DECISIONS.md` D-083.)*

---

## 10. Failure semantics

### 10.1 Downstream to client reply mapping

| Downstream outcome | Client reply | Reservation |
|---|---|---|
| `2xx` on final dot | `250 2.0.0 accepted` | Committed |
| `5xx` at any stage | `550` with downstream code and text appended | Released |
| `4xx` at any stage | `451` with downstream code and text appended | Released |
| Connect failure | `451 4.4.1 downstream unavailable` | Released |
| Timeout at any stage | `451 4.4.2 downstream timeout` | Released |
| TLS negotiation failure | `451 4.7.0 downstream TLS failure` | Released |
| Protocol violation | `451 4.3.0 downstream protocol error` | Released |

Downstream text is included because it is frequently the only diagnostic the operator will
see, but is sanitised of control characters and truncated to a safe length.

### 10.2 Ambiguity at the final dot

If the connection drops after the terminating dot is written but before a reply is read, the
message may or may not have been delivered. Simmer replies `451`, releases the reservation, and
increments `simmer_ambiguous_delivery_total`. This risks a duplicate on client retry. That
trade-off is chosen deliberately: for warm-up traffic, a duplicate is a smaller harm than a
silently lost message, and the alternative (`250` on an unconfirmed send) would make Simmer
lie about a delivery it cannot vouch for.

### 10.3 No eligible route

Per §3.2 step 4. Default reply `451 4.7.1 no eligible route, try later`, configurable via
`exhausted_chain_reply: 451 | 550`.

The default is a temporary failure, and the reason is the cutover invariant rather than the
mere fact that quota resets at the next day boundary.

A `5xx` in SMTP is not simply "failed" — it asserts something permanent about the message or
its recipient, and clients act on that assertion. Bounce handlers add the address to
suppression lists, CRMs mark contacts invalid, downstream ESPs count it against a hard-bounce
rate. A `550` emitted because a warming route reached its daily ceiling would therefore
permanently suppress a perfectly deliverable recipient, in systems that outlive Simmer by
years. Temporary infrastructure must not write permanent state into the systems around it.

Tested against the invariant directly: were Simmer removed, the application talking to the
downstream would never see this reply at all — it would simply send. The refusal is an
artefact of Simmer's presence, so it must be reported in the form that least distorts the
client's view of the world. `451` is also what a real MTA emits under its own rate limiting,
so it is the code a client's error handling is already shaped to expect from a relay.

The objection that `451` silently loses mail for a non-spooling client does not favour `550`,
because `550` loses it as well and additionally poisons the recipient record. The designed
answer to that risk is an overflow route, not a permanent failure code.

`550` remains available for operators who want a chain exhaustion to fail loudly and
immediately, and is used unconditionally for `strict_senders` rejection (§3.2 step 1) — that
is a policy statement about the *sender*, will not trigger recipient suppression, and should
be loud because it indicates misconfiguration.

**One permanent reply is a statement about the message itself**: §5.5's end-of-data rule,
`554 5.6.0`, on a payload carrying a smuggling-shaped terminator. The test above — were Simmer
removed, the application would never see this reply — is true of it, and is nonetheless not
decisive, which is worth stating plainly so that the next failure path added here is weighed
rather than waved through. What this section protects against is Simmer's *own transient
state* reaching systems that outlive it: a day's ceiling, a paused chain, a quota that resets
at midnight. Nothing about a malformed terminator is transient. The permanent state a client
records is "this message was malformed", which was true before Simmer was in the path and
stays true once it is unplugged, so the reply does not distort the client's view of the world.

`554` rather than `550` because the recipient is not the problem and must not be suppressed
for it. Permanent rather than `451` because bytes do not become valid by waiting: a temporary
failure would hold the message in the client's queue, re-present the same payload on every
retry, and delay the bounce it was always going to get. Unlike `exhausted_chain_reply` this is
**not configurable** — there is no deployment for which turning a refusal of this shape into a
retry loop is the better answer. *(Added. See `DECISIONS.md` D-095.)*

### 10.4 Shutdown

On `SIGTERM`: stop accepting connections, allow in-flight sessions to complete up to a grace
period (default 30s), release any reservations still outstanding, drain pools, exit. Sessions
exceeding the grace period receive `421` and are closed.

The link proxy (§5.7) stops accepting at the same moment, and its in-flight requests share the
same grace period. Idle keep-alive connections close at once. Requests still running when the
grace period ends are cut. *(Added. See `DECISIONS.md` D-083.)*

---

## 11. Storage

Postgres, following the established hikari-systems Rust data service pattern for connection
management, migrations, and query organisation. Migrations are versioned and applied at
startup.

Indicative tables:

- `quota_usage(ramp, route, domain_group, day_index, allowance, committed, reserved, updated_at)`
  — primary key on the first four.
- `quota_reservation(id, ramp, route, domain_group, day_index, count, correlation_id, created_at,
  expires_at)` — indexed on `expires_at` for the sweeper.
- `recipient_event(recipient_hash, ramp, route, sent_at)` — indexed on `(recipient_hash, sent_at)`
  and on `sent_at` for the sweeper. This is the high-cardinality table.
- `route_state(ramp, route, paused, graduated, allowance_override, override_expires_at,
  updated_at)` — admin mutations, so they survive restart.
- `instance_config(key, value)` — the recipient hash salt and similar singletons.

*(Amended — `ramp` is new in every table but `instance_config`. The migration that adds it
fills existing rows with a sentinel that cannot be a ramp name and then drops the column
default, so a pre-ramp instance cannot write; startup moves the sentinel rows into
`default_ramp`, once, under a lock. See `DECISIONS.md` D-099 and §2.3.)*

The storage layer sits behind a trait so the concrete backend can be substituted, but no
alternative backend is implemented in v1.

---

## 12. Implementation notes

### 12.1 Crates

Suggested, not mandated: `tokio`, `rustls`/`tokio-rustls`, `sqlx` (or the pattern's
established client), `serde`/`serde_yaml`, `regex`, `argon2`, `hickory-resolver`, `tracing`,
`axum` for the admin listener, `metrics`/`prometheus`, and a MIME parsing/building library.
For §5.7, `axum-reverse-proxy` with default features off (its default TLS stack is `aws-lc-rs`)
over a `hyper-rustls` connector built on the same ring provider and platform roots as §8.2.
*(Added. See `DECISIONS.md` D-083 and `LICENSES.md` §8.)*

**Licence check required before adopting any parsing crate.** Several of the well-known mail
crates in the Rust ecosystem are AGPL-licensed or have changed licence between versions.
Verify the licence of the exact version pinned and record the finding in the repository.

### 12.2 Container

Multi-stage build: a Rust builder, then `debian:bookworm-slim` with only the compiled binary,
CA certificates, and a non-root user. A `docker-compose.yml` brings up Simmer plus Postgres for
local use. Configuration is mounted; secrets arrive via environment.

Health check hits `/health`. The container must not `EXPOSE` port 25 to a host interface by
default.

### 12.3 Testing

- **Unit** — sender matching including wildcard precedence; domain group resolution; day-index
  arithmetic across DST boundaries and leap seconds; template rendering; reply mapping.
- **Property** — rewrite idempotency (§6.6) over generated messages; normalisation stability.
- **Integration** — a scripted fake downstream that can be made to return arbitrary codes,
  stall, drop mid-`DATA`, and refuse TLS. Assert the reply mapping table exhaustively, and
  assert that a failed send leaves `committed` unchanged.
- **Concurrency** — N concurrent sessions against a route with N-1 remaining allowance; assert
  exactly N-1 delivered and no overshoot.
- **Acceptance** — compose stack with real Postgres; a warm-up walked across simulated day
  boundaries by manipulating `warmup.started`; verifying fall-through to overflow at
  exhaustion; verifying the cutover invariant by sending the same logical message under both
  arrangements of §1.1 and asserting byte-equivalent downstream output.

---

## 13. Build order

Each phase should end in a working, testable artefact.

1. Config loading, full validation, structured logging, container skeleton.
2. SMTP ingress: state machine, AUTH, limits, buffering. Downstream forwarding with no
   rewriting and no quota. Reply mapping. End-to-end plaintext relay.
3. Postgres, migrations, quota model, reservation protocol, day-index arithmetic. Chain
   selection by quota. Sweepers.
4. Rewriting engine: templates, header set/remove, authentication artefact stripping,
   idempotency property test.
5. Body rewriting with decode/re-encode.
6. Recipient frequency constraint including hashing, normalisation, and sweeper.
7. Admin API, metrics, dry-run.
8. DNS preflight.
9. ~~Multi-recipient splitting and result collapse.~~ **Void** — §5.6 refuses multi-recipient
   transactions outright, so there is nothing to split and no collapse rule to implement.
10. Hardening: pooling refinements, graceful shutdown, acceptance suite, README.
11. Inbound listeners on 25/465/587, inbound TLS, and the sender ACL (§5.1, §5.3).
    *(Added — beyond the original ten. See `DECISIONS.md` D-070, D-071 and `docs/INGRESS.md`.)*
12. The optional link proxy (§5.7). *(Added. See `DECISIONS.md` D-083.)*
13. Named ramps (§3.4, §5.8): the ramps-only schema, storage keyed by ramp with legacy
    adoption, the control plane per ramp, then selection by listener and header.
    *(Added. See `DECISIONS.md` D-099.)*

---

## 14. Open decisions

Carried forward deliberately; each has a default that works, but each deserves an explicit
call before production use.

1. *(Resolved — retained for the reasoning.)* **§10.3 — `550` versus `451` on an exhausted
   chain.** Settled as `451`. The generalisable principle: **Simmer must not emit a reply that
   causes a client to record permanent state about a message or recipient**, because Simmer is
   temporary and that state is not. Apply this test to any new failure path added later.
2. **§3.2 step 1 — unmatched senders route to overflow.** Chosen because invisibility is the
   point, but it means a typo in a sender rule sends unwarmed traffic at full volume via the
   established identity. The `WARN` and counter must actually be alerted on, or set
   `strict_senders: true`.
3. *(Resolved — see `DECISIONS.md` D-100.)* **§7.1 — no MX-based domain grouping.** Literal
   matching meant Google Workspace custom domains landed in the catch-all rather than being
   tracked against Google. Ramp #2 (22–24 Sep 2026) showed the cost: Google refused Workspace
   company domains with the same reputation error as gmail.com, and holding `google` at 0
   could not stop them. §3.2 step 2 now groups an unlisted domain by its MX host.
4. **Reputation cost of overflow.** Rewriting `From:` to an established domain spends that
   domain's reputation on new-brand traffic. A dedicated subdomain (`mail.established.com`)
   contains the blast radius and is what the example configuration uses.
