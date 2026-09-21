# simmer

An SMTP relay facade that applies a domain reputation warm-up ramp.

<!-- current-version: source of truth for the release number. Keep in sync with the git tag, DOCKERHUB.md, and Cargo.toml; enforced by .githooks/pre-push and the release CI. -->
**Current version: `v0.6.0`** — [all releases](https://github.com/Hikari-Systems/simmer/releases).

Simmer sits between an application and one or more real SMTP providers. It
accepts a message, selects an outbound route according to quota state, rewrites
the message's identity to match that route, forwards it synchronously, and
returns the downstream's verdict to the client on the same connection.

**Simmer is temporary infrastructure.** It exists for the duration of a warm-up
and is then removed. `docs/SPEC.md` is the authoritative specification;
`DECISIONS.md` records divergences from it.

## What it is not

Simmer is **not an MTA**, and the distinction is load-bearing rather than
pedantic. There is no spool, no queue, no retry scheduler and no DSN generation.
The client connection is held for the duration of the downstream conversation; a
message is either delivered during that conversation or the client is told it was
not. The only things persisted are quota state and recipient-frequency events.

It also holds no DKIM key material and performs no signing. The downstream signs,
exactly as it would have if the application had connected to it directly.

## The cutover invariant

Every design decision is subordinate to §1.1 of the spec: **Simmer's output
identity must be exactly expressible as application-side configuration**, so it
can be removed in either order relative to the application being reconfigured.
Two consequences worth knowing before editing anything:

- Rewrites are **absolute assignments**, never relative transformations. "Set the
  `From:` domain to `newbrand.com`" is fine; "append `.new` to the sending
  domain" is not, because applying it to already-migrated traffic corrupts it.
- Rewrites are **stable**: applying one twice changes nothing. Identity fields
  (`envelope_from`, `From:`, `Sender:`, `Message-ID:`) must satisfy this with no
  override. Other headers may be exempted by naming them in the route's
  `unstable_headers`, which is an acknowledgement that the header is
  migration-only and will change the moment the application is cut over.

A corollary that catches people out: Simmer never emits a reply that would make a
client record *permanent* state about a message or recipient. A chain exhausted
by its daily quota returns `451`, not `550` — a `550` would put a perfectly
deliverable recipient on a suppression list that outlives Simmer by years.

## Status

All ten phases of `docs/SPEC.md` §13, except phase 9, which is void — see D-047
below — plus phase 11, which came after them.

**Phase 1** — configuration loading, full startup validation, structured
logging, the container skeleton.

**Phase 2** — SMTP ingress and the outbound leg. Simmer accepts a message on
port 25, authenticates the client, buffers the body, forwards it to a
downstream over TLS, and maps the downstream's verdict back on the same
connection.

**Phase 3** — the warm-up itself. Route selection now walks the chain by quota
state: a warming route carries traffic up to its daily allowance for that
recipient's domain group, and everything beyond it falls through to the overflow
route. Counters increment on downstream `2xx` only, via the §7.4
reserve/send/commit protocol, so a failed send never consumes allowance and
concurrent sessions cannot both claim the last slot.

**Phase 4** — the rewriting engine, which is what makes a route's *outbound
identity* mean anything. Authentication artefacts are stripped unconditionally
(§6.5), `remove_headers` and `set_headers` are applied with §6.3's templates
rendered against the message as it arrived, a `Received:` header is prepended, and
the envelope sender is computed from the route. §6.6's stability property —
`rewrite(rewrite(m)) == rewrite(m)` — is enforced at startup against a synthetic
probe and again as a property test over generated messages.

Phase 4 also built the §12.3 acceptance suite ahead of schedule: two Mailpit traps
on a compose profile, a ramp walked across simulated days by moving
`warmup.started`, and an assertion that both arrangements of the cutover invariant
produce byte-equal output.

**Phase 5** — body rewriting (§6.4), which completes §6. Each `text/*` part is
decoded according to its `Content-Transfer-Encoding` and charset, the route's
`body_rewrites` patterns are applied in order, and the part is written back in the
encoding and charset it arrived with. A URL split across a quoted-printable soft
line break matches, which is the case §6.4 exists for. Attachments, non-text
parts, and signed or encrypted parts are never touched, and a part that cannot be
decoded is left alone, warned about and counted.

A part that matches nothing is not re-encoded at all — a body in which nothing
matched is forwarded as the bytes it arrived as, MIME boundaries and trailing
whitespace included.

**Phase 6** — the recipient-frequency constraint (§7.3), preceded by a policy
reversal: **a transaction may now carry exactly one recipient** (D-047, below).

A route may declare `recipient_frequency`, and a recipient at or over its
threshold inside a rolling window makes that route **ineligible** — the message
steers to the next link in the chain and is never dropped. The recipient is
normalised first, so `Bob.Smith+news@gmail.com` and `bobsmith@gmail.com` count as
the one inbox they are: lowercased, everything from `+` to `@` removed, and dots
folded out of the local part at the providers listed in `dot_insensitive_domains`.

What is stored is a **keyed hash**, never the address: HMAC-SHA256 under a salt
minted once and persisted in `instance_config`, truncated to 16 bytes. No
plaintext address reaches the database, a log line or a metric label. Events are
recorded only on a downstream `2xx`, in the same transaction that commits the
quota, and only for routes that declare a constraint; an hourly sweeper evicts
anything past the longest configured window plus a margin.

**Phase 7** — the control plane (§9): a Prometheus exporter, a read API, a write
API and dry run. The endpoints are below under [Control plane](#control-plane).

Two things in it are worth knowing before you use it. The read API reports both
what the *schedule* says and what the *row* says, and flags the difference,
because `quota_usage.allowance` is authoritative once written (D-026) — a
schedule edit plus a restart does not raise today's ceiling, and an API that
reported the schedule would mislead you at exactly the wrong moment. And every
mutation tells you which chains it has just left with no eligible route: pausing
a route, or setting an allowance of zero, is a legitimate thing to do and also
the thing most likely to make every message on a chain `451` without anyone
meaning it.

**Phase 8** — the DNS preflight (§6.7), which completes §6. A route with a
`preflight` block has its outbound identity's domain checked for SPF, DKIM and
DMARC at startup and every fifteen minutes. It is **non-blocking by design**: a
failure is a `WARN` and a `0` gauge and nothing else, and only `strict: true`
makes the route ineligible — at which point the message *steers* to the next link
exactly as §7.3 does, and a chain with none left is §10.3's `451`, never a `5xx`.
A route with no verdict yet is eligible, so a slow resolver at boot cannot empty a
chain.

**Phase 10** — hardening. §8.3's connection pool: per route, `max_connections`
held as a bound rather than a hint, `RSET` between messages, `NOOP` validation of
a connection idle beyond a short threshold, and retirement at `idle_ttl` or
`max_messages_per_connection`. §10.4's shutdown drains it. Two things about it are
worth knowing and are below under [Connection pooling](#connection-pooling).

Phase 10 also fixed a defect carried since phase 2: the decoy hash that equalises
the cost of a failed login now takes its parameters from the credentials actually
configured, rather than from a fixed guess that only matched them by coincidence
(`DECISIONS.md` D-066).

**Phase 11** — listeners on 25, 587 and 465, inbound TLS, and a sender ACL
(`docs/INGRESS.md`, D-070, D-071). Each listener has its own `tls` and `auth`
policy, defaulting to what its port's RFC says. `STARTTLS` and implicit TLS use one
PEM certificate read at startup. Each user carries `grants.send_as`, the sender
identities it may present, and an authenticated message whose envelope or `From:`
falls outside them is refused `550 5.7.1`. Details under
[Listeners and TLS](#listeners-and-tls) and [Sender grants](#sender-grants).

What works today: **an end-to-end relay that applies the ramp, rewrites both the
identity and the body, paces how often one recipient hears from a warming route,
keeps a conversation on the route that started it, pools its downstream
connections, accepts submissions over verified TLS from
applications limited to their own sender identities, and can be inspected and
steered without a restart.** What does not:

- **No scopes on admin tokens.** Every token can do everything; a token that
  could read but not mutate is a plausible ask and is not built.
- **No pre-authentication limits.** A client that never authenticates is bounded
  by `max_concurrent_sessions` and the timeouts like any other, but there is no
  separate, tighter budget for it (D-072). That matters only once Simmer listens
  somewhere `allowed_cidrs` cannot be tight.

And one thing that will not arrive, because it is a decision rather than a gap:

- **A transaction may carry exactly one recipient.** A second `RCPT TO` is
  refused `452 4.5.3 multiple recipients not permitted`, and no configuration
  changes that. An application that batches recipients into one transaction must
  send one message per recipient instead — which is what it will be doing anyway
  once Simmer is unplugged. The reason is that SMTP allows one reply per
  transaction, so several per-recipient outcomes have to be collapsed into a
  single code, and every way of doing that either drops mail silently or records
  one recipient's permanent failure against the others. See `DECISIONS.md` D-047
  and `docs/RECIPIENTS.md`.

```sh
$ openssl s_client -quiet -starttls smtp -connect simmer:587 -servername simmer.internal
EHLO me
250-simmer.internal greets me
250-PIPELINING
250-8BITMIME
250-SIZE 26214400
250-AUTH PLAIN LOGIN
250 AUTH=PLAIN LOGIN
```

## Quick start

```sh
cp .env.example .env        # fill in the values
docker compose up -d --build
curl -s localhost:8080/health | jq
```

Or locally, against a Postgres you already have:

```sh
export DATABASE_URL=postgres://simmer:simmer@localhost/simmer
export SIMMER_ADMIN_TOKEN=dev SIMMER_CFAPP_HASH='$argon2id$...'
export POSTAL_USER=x POSTAL_PASS=y SENDGRID_KEY=z
cargo run --bin server
```

`SIMMER_CONFIG` overrides the config path (default `simmer.yaml`).

## Configuration

YAML, with `${ENV_VAR}` interpolation resolved at startup. An unresolvable
reference is a fatal startup error, as is any validation violation — and **all of
them are reported together**, because a service that takes minutes to build
should not be debugged one error at a time.

Interpolation happens over the parsed YAML tree rather than the raw text, so a
secret containing a newline and a colon cannot restructure the document.

Unknown keys are rejected. There is no hot reload (spec §2.2): changes need a
restart.

**One rule catches people out.** A route's `identity.envelope_from` must have a
**constant domain** — the part after the `@` cannot contain a template. The local
part can (`bounce+{{recipient.local}}@newbrand.com` is fine); the domain cannot.
This is not fussiness about templates: a route exists to build reputation for one
domain, so a route whose domain varies per message warms nothing while its quota
row still reads like a healthy ramp. Startup refuses it rather than letting you
discover that in six weeks of unchanged deliverability. See `DECISIONS.md` D-069.

See `simmer.yaml`, which is commented throughout, and `.env.example` for the
variables it expects.

### Listeners and TLS

`server.listeners` has one entry per port. An entry that names only an address
takes its port's RFC defaults, and startup logs each listener's effective policy:

| Port | `tls` | `auth` | |
|---|---|---|---|
| 25 | `off` | `optional` | RFC 5321 transfer — what Simmer did before listeners existed |
| 587 | `starttls_required` | `required` | RFC 6409 submission |
| 465 | `implicit` | `required` | RFC 8314 submissions |
| any other | `off` | `optional` | |

`tls` is one of `off`, `starttls` (offered, a client may decline), `starttls_required`
(nothing but `EHLO`, `NOOP`, `RSET`, `QUIT` and `STARTTLS` until the handshake) and
`implicit`. `auth` is one of `disabled`, `optional` and `required`.

```yaml
server:
  listeners:
    - address: "0.0.0.0:25"
      auth: required
    - address: "0.0.0.0:587"
    - address: "0.0.0.0:465"
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"   # leaf first
    private_key: "/etc/simmer/tls/privkey.pem"
  auth:
    allow_insecure_auth: false
```

Four things worth knowing:

- **`allow_insecure_auth` means what it says, and defaults false.** AUTH over an
  unencrypted session is refused `538 5.7.11` and is not advertised there. Before
  phase 11 this key had to be *true*, because there was no TLS to use instead; a
  config carried over will still work, but you probably want it false now.
- **The certificate is checked at startup as the listener will load it.** A
  missing, unreadable, unparseable or mismatched file refuses to start, alongside
  every other violation. The container runs as **UID 1000**, so mount the key
  readable by that user — the error says so if you do not. A certificate that
  does not cover `server.hostname`, or that expires within fourteen days, is a
  startup warning rather than a failure, because refusing to start would take the
  plaintext listeners down too. Rotation is a restart.
- **Plaintext pipelined behind `STARTTLS` drops the connection.** Bytes sent in the
  same packet as `STARTTLS` came from whoever is on the wire, not the TLS peer.
- **TLS does not make Simmer a public MX** (spec §2.3). It lets Simmer sit on a
  segment where cleartext credentials are unacceptable, which is a much smaller
  claim. `allowed_cidrs` still applies to every listener, and so does one shared
  `max_concurrent_sessions`.

`server.listen` and `server.auth.required` were removed; a config using either
fails to start with a message saying what replaced it.

### Sender grants

Every user in `server.auth.users` carries the sender identities it may present,
in the same pattern grammar as `senders`: an exact domain, `*.subdomain`, or a full
address.

```yaml
    users:
      - username: "cfapp"
        password_hash: "${SIMMER_CFAPP_HASH}"
        grants:
          send_as: ["oldbrand.com", "*.oldbrand.com", "newbrand.com"]
```

For an authenticated session, the `MAIL FROM` address is checked at `MAIL FROM` and
the first `From:` address at the final dot; either outside the grants is `550 5.7.1
sender not permitted`, and so is a message with no parseable `From:`. Anything not
granted is refused. Watch `simmer_sender_not_permitted_total`: it means either a
misconfigured application or somebody else's credentials.

**The grants decide whether a message is accepted, never where it goes.** Routing
is still `senders`, exactly as before, and the username still plays no part in it.
Two users granted the same identity send byte-identical mail.

**They apply only to sessions that authenticated.** A listener with `auth:
optional` — port 25's default — accepts unauthenticated mail with any sender from
any address in `allowed_cidrs`, and startup warns about each such listener once
users exist. Set `auth: required` wherever that is too much trust.

To mint a password hash, pipe the password in; it is never taken as an argument,
where it would land in shell history:

```sh
printf '%s' "$PASSWORD" | docker run -i --rm simmer:local hash-password
```

### The ramp

A warming route's `warmup.schedule` is indexed by **elapsed days from
`warmup.started`**, not by calendar date. A route started at 09:00 rolls over at
09:00 every day, is immune to DST, and can never see a 23- or 25-hour window. Past
the end of the array the final value repeats — routes never auto-graduate to
uncapped; that is a deliberate act (`POST /routes/{name}/graduate`, or editing the
config).

Quota is keyed on `(route, domain_group)` because mailbox providers throttle
independently of one another, and bucketed per day. **An unused allowance does not
carry over**, and a route that sent nothing yesterday still advances its day
index.

Overflow routes are never capped, but they *are* counted — "how much is spilling
to overflow" is the number that tells you whether the ramp is set too low.

### Partial ramp

Without it, a warming route takes **every** eligible message from the day
boundary until its cap is met, and then none. Early in a ramp that is a burst in
the first hours of each day. A `share` list in the schedule offers the route only
part of its traffic for its first days, so the cap is reached later and the
route's volume is spread across the day (§3.2 step 3c′, §7.2, D-091):

```yaml
    warmup:
      started: "2026-08-01T09:00:00Z"
      schedule:
        default: [50, 100, 200, 400, 800]
        share:   [0.1, 0.25, 0.5]   # day 0: a tenth of the messages try it; day 3 on: all
```

`share` is indexed by day like the caps and applies to every domain group. Past
its end every message is offered: unlike a cap, the last value does **not**
repeat.

A message not offered skips with reason `partial_ramp` and goes to the next link,
as `recipient_frequency` steers. Nothing is dropped, and the cap itself is
unchanged. Which messages are offered is a keyed hash of the normalised
recipient, the route and the day index under the §7.3 salt, not a dice roll.
Every instance agrees, and `POST /dryrun` reports the real answer for a
recipient. A pinned thread-affinity reply is exempt, and a graduated route is
offered everything. §4.2 refuses a share outside `(0, 1]`, and a route with a
`share` list that is last in any chain, where every message it turned away
would be a `451`.

Watch `simmer_route_skipped_total{reason="partial_ramp"}` against
`reason="quota"`. `/routes` reports the list and `today`'s share (`null` when
every message is offered).

### Thread affinity

Off by default. With it on, a reply the application sends into a conversation
Simmer started leaves via **the route that started it** — the same outbound
identity the recipient has already seen — instead of whichever route the ramp
would pick today (§3.2 step 2a, D-090).

```yaml
thread_affinity: true      # top level, beside strict_senders
```

**How a reply is recognised.** Nothing is stored. Each route's `Message-ID:`
carries its own domain (`<{{uuid}}@newbrand.com>`), so an ID Simmer emitted names
the route that emitted it. When the recipient replies, their mail client puts
that ID in `In-Reply-To:`/`References:`; when the application answers, its
`References:` carries it forward, and Simmer reads it. With `thread_affinity`
on, startup refuses any route in any chain that does not set a `Message-ID:`
with a literal domain, and any two routes in one chain that share one.

**What the application must send.** A correctly threaded reply (RFC 5322
§3.6.4), built from the recipient's inbound message:

| New outbound header | Built from the inbound | Rule |
|---|---|---|
| `In-Reply-To:` | its `Message-ID:` | Exactly that one ID, in angle brackets |
| `References:` | its `References:` + its `Message-ID:` | If it has no `References:`, its `In-Reply-To:` (when it holds one ID) + its `Message-ID:`; if neither, just its `Message-ID:` |
| `Subject:` | its `Subject:` | Prefix `Re: ` unless already present; otherwise unchanged |
| `To:` | its `Reply-To:` if present, else its `From:` | The recipient's address |

`From:` is the application's usual sending address, so the same sender rule —
and so the same chain — matches. Carry `References:` forward on every turn:
drop it once and the link back to Simmer's first ID is gone for the rest of the
thread; if you must trim a long one, keep the first ID and the most recent. Copy
IDs byte for byte. Check that your inbound parser or webhook actually hands you
`References:` and `Message-ID:`; some drop them unless asked for raw headers. A
new message that is not a reply carries no `References:`, and is routed
ordinarily.

**What a pinned reply is exempt from, and what it is not.**

- **The day's cap — only once it has been met.** A pinned reply first takes an
  ordinary slot, so replies within the cap spend it like any message and new
  conversations spill to overflow sooner. When the cap is already met, the reply
  still goes out on its route, **past the cap, and counted**: `committed` reads
  above `allowance` in `/routes` and `/quota`, the allowance itself never
  changes, and every message that is not a pinned reply is still refused at it.
  `simmer_thread_affinity_total{outcome="over_cap"}` counts them — watch it,
  because each is a send the ramp did not schedule.
- **`recipient_frequency`** on the pinned route: a conversation is not
  over-mailing. The event is still recorded.
- **Not exempt:** a pause, a strict preflight failure, or a `warmup.started` in
  the future. Those mean the route cannot send; the reply then takes the
  ordinary walk (`outcome="ineligible"`) and the thread changes identity. A
  downstream failure is still a failure — no failover (§3.3).

Only the matched chain's routes can be pinned, and with thread affinity on,
envelope-only configurations no longer reject at `RCPT TO` (§5.4): whether a
message is a reply is in its headers.

### SMTPUTF8

`EHLO` advertises `SMTPUTF8` only when **every** route reachable from a chain
sets `downstream.smtputf8: true`, and the default is `false`. Simmer cannot know
at `EHLO` time which route a message will take, so it can only promise what all
of them can deliver. A UTF-8 address presented when the capability was not
advertised is refused `550 5.6.7` at `MAIL FROM`, rather than discovered
mid-relay. See `DECISIONS.md` D-018.

### 8BITMIME

`BODY=8BITMIME` is passed on only to a downstream that advertises `8BITMIME`. A body
with no byte above 0x7F is 7-bit whatever the client declared, so it goes to any
downstream, without the parameter. A body that really is 8-bit goes to a downstream
without the extension only if its route sets `downstream.assume_8bitmime: true` —
Postal is the reason: it advertises no `8BITMIME` but accepts 8-bit bodies, and the
shipped config declares it on the Postal route. Otherwise that message is deferred
`451 4.3.5` and counted in `simmer_downstream_config_error_total`. See
`DECISIONS.md` D-074.

### Timeout budget

The sum of the per-stage downstream timeouts (`connect` + `command` + `data`)
must sit comfortably below the **client's** own SMTP timeout. Simmer holds the
client connection open for the whole downstream conversation, so if Simmer is
still waiting on the downstream when the client gives up, the client sees a
dropped connection rather than the `451` Simmer was about to send — and a `451`
it can act on is much more useful than a socket error it cannot.

With the shipped defaults the downstream budget is 10s + 30s + 120s = 160s
against a client `data` timeout of 300s. If you raise the downstream timeouts,
check that relationship still holds.

### Connection pooling

Each route has its own pool, configured under `downstream.pool`. Two things about
it are not obvious from the keys:

**`max_connections` is a bound, not a hint.** It is held for the whole time a
connection is in use, so a route can never have more connections open than it
says — which is the point: it protects the downstream from a burst as much as it
saves Simmer a handshake. A session that cannot get one within the route's
`connect` budget is answered `451 4.4.5 downstream connection pool exhausted`,
its own error class so that a dashboard can tell it apart from a downstream that
is refusing connections. The two call for opposite responses. If you see it,
raise `max_connections`.

**`idle_ttl` should sit below whatever the downstream's own idle timeout is.** If
it does not, Simmer will regularly pick up connections the provider has already
closed. That is survivable — the message is retried once on a fresh connection and
the client never sees it — but it costs a reconnect, and
`simmer_pool_retries_total` is the metric that tells you it is happening. The
retry is deliberately never attempted after the message body has been sent
(`DECISIONS.md` D-068): past that point a delivery may already have happened, and
retrying would send it twice.

### Link proxy

An optional HTTP forwarder for the tracking and unsubscribe links your routes
rewrite (spec §5.7, `DECISIONS.md` D-083). The links point at a
public name such as `click.newbrand.com`; a TLS-terminating load balancer sends
that name to `link_proxy.listen`; Simmer sends every request to one upstream
and relays the response:

```yaml
link_proxy:
  listen: "0.0.0.0:80"
  upstream: "https://link.domain2.com/tracking"   # a path prefix is optional
  allowed_cidrs: ["10.0.0.0/8"]                   # the load balancer's subnets
```

`http://click.newbrand.com/test?abc=123` arrives at the upstream as
`GET /tracking/test?abc=123` with `Host: link.domain2.com`.

- **Unchanged:** method, path, query, body, cookies and every end-to-end header.
- **Added:** `X-Forwarded-For` (the load balancer's value, with the load
  balancer's address appended), `X-Forwarded-Host` and `X-Forwarded-Proto`,
  where the load balancer did not set them, and `Via: 1.1 simmer`.
- **Rewritten on the way back:** a `Location` or `Content-Location` naming the
  upstream, a `Set-Cookie` with `Domain=` the upstream (or a parent domain), and,
  with a prefix, a `Location: /tracking/…` or cookie `Path=/tracking…`. Each is
  pointed at the public name the browser used. Anything naming another host —
  the redirect to the click's real destination — passes through untouched.
  Redirects are never followed.
- **Refused:** HTTP/2 (HTTP/1.0 and 1.1 only), `CONNECT` (`501`), bodies over
  `max_request_bytes` (`413`), and connections from outside `allowed_cidrs`,
  which are closed before anything is read. WebSocket upgrades are forwarded as
  plain requests, never tunnelled.
- **Failures** are `502` (upstream unreachable, or its certificate does not
  verify against the platform roots) and `504` (no response headers within
  `timeouts.upstream_response`). Anything Simmer answers itself carries
  `Cache-Control: no-store`, and nothing is retried: a one-click unsubscribe is
  a POST.

It logs method, path and status, never the query string or cookies, which carry
recipient-identifying tokens. It exports metrics as
`simmer_link_proxy_requests_total{status_class,origin}`,
`simmer_link_proxy_duration_seconds`, `simmer_link_proxy_connections` and
`simmer_link_proxy_connections_refused_total{reason}`.

**Deploying it:** put only the link proxy's port in the load balancer's target
group, never an SMTP port, and point the target group's health check at the
admin port's `/health`. The proxy reserves no path of its own. At cutover,
repoint the public name at the upstream directly and remove the block.

### Database: Postgres or SQL Server

Every release ships two images of the same code, differing only in where quota
state lives (`DECISIONS.md` D-084):

| Image | Database | Platforms |
|---|---|---|
| `hikarisystems/simmer:vX.Y.Z` | PostgreSQL | amd64, arm64 |
| `hikarisystems/simmer:vX.Y.Z-mssql` | SQL Server 2017 or later, or Azure SQL | amd64 |

The choice is made when the image is built, not by configuration: each image
contains one storage layer and refuses the other's `database.url` at startup,
naming the image to use instead. Everything else — the config file, the control
plane, the metrics, the behaviour — is identical.

For the `-mssql` image, `database.url` is an ADO.NET or JDBC connection string:

```yaml
database:
  url: "server=tcp:sql.internal,1433;database=simmer;user id=simmer;password=${SIMMER_DB_PASSWORD}"
```

- **The connection is encrypted by default**, queries included, whether or not
  the string says `encrypt=true`. Only an explicit `encrypt=false` opts out, and
  startup warns that queries then cross the network in cleartext. A server with a
  self-signed certificate needs `TrustServerCertificate=true`, which also warns.
- **The login needs to create tables** in its database on first start. Migrations
  are applied at startup, as with Postgres, and serialised across replicas.
- **Route and domain-group names are limited to 200 characters** in this build,
  and startup refuses longer ones. SQL Server caps a clustered key at 900 bytes.
- **Names stay case-sensitive.** The tables use a binary collation, so `Warming`
  and `warming` are two routes, as they are in Postgres, even though SQL
  Server's default collation would merge them.
- SQL Server authentication only: no Windows or Kerberos integrated login.

## Capturing and replaying traffic

Optional, off by default, and a **debugging mode rather than a spool** — see
`docs/CAPTURE.md` for the long form and `DECISIONS.md` D-085/D-086 for why it is
allowed to exist alongside "Simmer is not an MTA".

With a `capture:` block configured, every accepted message is appended to a JSONL
file rotated every ten minutes:

```
/var/lib/simmer/capture/2026-09-20T14.10.jsonl
```

Each line opens with the four fields that identify a message to a person —
timestamp, recipient, sender, subject — so a bucket is scannable without a
parser:

```console
$ cut -c1-140 2026-09-20T14.10.jsonl
{"at":"2026-09-20T14:13:02.418Z","rcpt_to":["bob@gmail.com"],"mail_from":"news@oldbrand.com","subject":"Your September statement","v":1,
```

A record is on disk within **ten buffered lines or 500 ms of quiet**, whichever
comes first (D-088), so `tail -f` on the current bucket keeps up and a replay of a
range that has just ended finds its last records. Those are flushes, not `fsync`s:
`on_error: defer` is the only mode that puts a record on the platter before the
client is told anything.

`server replay` reads a range back out and sends it to another Simmer, as the
original client did:

```sh
# always count first — this connects to nothing
server replay --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host app2 --dry-run

# and then mean it
SIMMER_REPLAY_PASSWORD_CFAPP=... server replay \
  --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host app2 --confirm
```

### Before you enable this

- **The directory is a mailbox.** Every recipient address and every body, in
  plaintext. §7.3 hashes recipients precisely to avoid the container
  accumulating that. It is `0700`, the files are `0600`, retention defaults to
  24h, and startup warns on every boot. Delete it when the investigation ends.
- **A captured body may hold anything** your application sends — password-reset
  links, one-time codes, session tokens. Nothing can filter that.
- **It costs disk.** A 25 MiB message is a ~34 MiB line. `max_body_bytes`
  (default 1 MiB) and `retention` bound it; alert on
  `simmer_capture_disk_bytes`, which is incremented as the writer flushes and
  recounted from the directory by each retention sweep. Measured on mixed traffic
  at 10 msg/s: about 1 GiB an hour per instance. **On 0.3.0 that gauge only moved
  on the hourly sweep** and so read 0 for the first hour — if you are on 0.3.0,
  size a volume from `simmer_capture_bytes_total` instead, or upgrade.
- **Replay delivers mail twice, on purpose**, and spends the target's warm-up
  quota (§7.4) doing so. Point it at a test instance, never production.
  `--confirm` is required and has no default, and so is `--host`.
- **A capture write failure does not stop mail** under the default
  `on_error: continue`. Set `on_error: defer` only when a gap would invalidate
  the run, and know that a full disk then answers `451`.

Nothing about the capture is reachable through the control plane, deliberately:
a `/routes` or `/quota` response carrying a recipient would undo §7.3's whole
reason for hashing, and a capture browser would turn a debugging mode into a
permanent one.

## Control plane

On `admin.listen`, port 8080 by default. `/health`, `/healthcheck` and `/metrics`
are open; everything else needs `Authorization: Bearer <token>`, including the
reads — `/routes` discloses every downstream hostname and the whole routing
shape. See `DECISIONS.md` D-055.

| | |
|---|---|
| `GET /health` | Liveness plus database reachability. `503` when the database is down |
| `GET /metrics` | Prometheus exposition (§9.1) |
| `GET /routes`, `GET /routes/{name}` | Configuration plus live state: warm-up day, per-group allowance and usage, paused, graduated, preflight results, pool statistics |
| `GET /quota?route=&group=` | The same windows, filtered. Both filters optional and independent |
| `POST /routes/{name}/pause`, `/resume` | Make a route ineligible without a restart. Persisted |
| `POST /routes/{name}/graduate` | Pin to the final schedule value. `{"graduated": false}` reverses it |
| `POST /routes/{name}/allowance` | `{"domain_group": …, "allowance": N\|null}`. Expires at the route's next day boundary |
| `POST /quota/reset` | Destructive. Needs `"confirm": "reset"` |
| `POST /dryrun` | What *would* happen. Sends nothing, reserves nothing, writes nothing |

```sh
TOKEN=$SIMMER_ADMIN_TOKEN

# Why is mail deferring for Google?
curl -sH "Authorization: Bearer $TOKEN" localhost:8080/quota?group=google | jq

# Let the warming route send more to Google, today only.
curl -sXPOST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"domain_group":"google","allowance":500}' \
  localhost:8080/routes/warming/allowance | jq

# What would this message do?
curl -sXPOST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"envelope_from":"jane@oldbrand.com","from_header":"Jane <jane@oldbrand.com>",
       "recipients":["bob@gmail.com"],"body":"see https://oldbrand.com/x"}' \
  localhost:8080/dryrun | jq
```

**Reading `/routes`.** Each route also reports its `preflight` verdict — `null`
means nothing has been checked, which is not the same as a pass and is reported
differently on purpose — and its `pool`: `max_connections` and the live `idle` and
`active` counts, plus lifetime `opened`, `reused`, `retired` and `discarded`.
`reused` far below `opened` means connections are not surviving between messages;
a climbing `discarded` means the downstream is closing them underneath you.

Each domain group reports `scheduled` (what the
configuration says today), `allowance` (what the row says, which is what the
reservation protocol enforces), `override`, `committed`, `reserved`, `headroom`
and `drift`. `drift: true` means the row and the schedule disagree — the expected
state for the rest of the day after a schedule change, and the thing to look at
first when the ramp is not doing what the YAML says. `row_exists: false` means
nothing has been sent on that route and group today.

**Dry run** is the tool for validating a configuration before it carries traffic.
It runs the real sender match, the real chain walk and the real rewrite engine, so
what it shows is what would go out; the tests assert its chain evaluation is
identical to the relay's across every skip reason. It reports each
`body_rewrites` pattern's match count **including the ones that matched nothing**,
which is usually the answer to "why is my rewrite not firing". Give it
`in_reply_to` and `references` (or a full `message`) to see where a reply would
go: the response's `thread` names the pinned route, and a pinned route that
would go past its cap shows as selected with reason `over_cap`.

**Every mutation is audited** at `INFO` with the acting token's name, the target,
and the value it replaced. Name your tokens (`admin.tokens`) and that line names
somebody; leave `auth_token` alone and it says `default`. See D-053.

**Watch `simmer_admin_auth_failures_total{reason="invalid"}`.** The write API can
pause a route, and a run of those is somebody guessing.

### Process and runtime gauges

`/metrics` also reports the process and runtime on every scrape (D-075):
`process_resident_memory_bytes`, `process_open_fds`, `process_max_fds`,
`process_threads` and `process_start_time_seconds` under their standard names;
`simmer_sessions_active` against `simmer_sessions_max`;
`simmer_reservations_in_flight`, which should be 0 whenever the instance is idle;
`simmer_db_pool_connections{state}` against `simmer_db_pool_max`, where `in_use`
pinned at the maximum comes before `451 4.3.0`; and `simmer_tasks_alive`. The
exporter's buffered histogram samples are drained every 5 s whether or not anything
scrapes (D-076).

## Releasing

Releases follow slater's scheme. The version is a literal in three places — the
"Current version" line above, the same line in `DOCKERHUB.md`, and `Cargo.toml` —
because `git tag` sorts lexically and buries the newest tag mid-list. A `vX.Y.Z`
tag releases the commit it points at:

```sh
git config core.hooksPath .githooks        # once per clone
# bump all three to X.Y.Z, commit, and push to main; wait for build.yml
git tag -a vX.Y.Z -m "what this release is"
git push origin vX.Y.Z
```

`.githooks/pre-push` refuses the tag push if any of the three disagree with it, and
`release.yml` checks again (the hook can be skipped; CI cannot). The release does
**not rebuild**: it promotes the image `build.yml` already built and tested for that
commit on `main`, so the tagged commit must be on `main`, and the job waits for
that build if it is still running. It tags both images on Docker Hub
(`hikarisystems/simmer`): `:vX.Y.Z` and `:latest` for Postgres, and `:vX.Y.Z-mssql`
and `:latest-mssql` for SQL Server. It tags both on GHCR too, then creates the
GitHub release, with the tag annotation followed by the generated changelog as its
notes. A release gets both images or neither.

Docker Hub carries releases only. GHCR still gets `:<sha>`, `:<branch>` and
`:latest` (and the same with `-mssql`) from every branch push.

## Development

```sh
docker compose up -d simmer-db   # the quota tests need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check                 # licences, advisories, sources
docker compose up -d --build     # not optional before pushing
```

**Every dependency comes from crates.io.** There are no git dependencies, so
`deny.toml` has no `allow-git` allow-list and `unknown-git = "deny"` rejects all
of them; `cargo build` needs no credentials for a private repository. `hs-utils`
was the one exception until phase 7 — it supplied a single stdlib-only function,
which now lives in `src/healthcheck.rs` behaving identically (D-060). If you need
something from the estate's shared crates, copy it and record why, rather than
taking the dependency back.

**The SQL Server build** is `--no-default-features --features mssql` on any cargo
command (D-084). Its storage tests need a SQL Server; the compose file has one
behind a profile:

```sh
docker compose --profile mssql up -d simmer-mssql-db
export MSSQL_URL='server=tcp:127.0.0.1,1434;user id=sa;password=Simmer-dev-1!;TrustServerCertificate=true'

cargo test --no-default-features --features mssql
cargo clippy --all-targets --no-default-features --features mssql -- -D warnings
cargo deny --no-default-features --features mssql check
docker build --target runtime --build-arg CARGO_FEATURES="--no-default-features --features mssql" .
```

`tests/store_conformance/` is the storage contract both backends must meet,
run by `tests/store_postgres.rs` and `tests/store_mssql.rs`. A change to either
store belongs in that suite first. Race tests there warm their pools and start
behind a barrier, because contenders that each open a fresh connection never
actually overlap, and such a test passes with the lock removed (D-084).

The §12.3 acceptance suite runs against its own stack and is not part of
`cargo test` — it needs Docker and about two minutes of container restarts:

```sh
docker compose -f docker-compose.yml -f test/compose/acceptance.yml --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

It walks a warm-up across simulated days by moving `warmup.started` and
re-creating the container, and it is the only tier that proves both arrangements
of the cutover invariant (§1.1) produce byte-equal output. It also submits over
587 with `STARTTLS`, verifying the certificate a one-shot `tls-init` service mints
for each run. `docs/ACCEPTANCE.md`
explains the topology; `DECISIONS.md` D-042 explains what will bite.

`DATABASE_URL` is needed only by `tests/quota*.rs`, which use `#[sqlx::test]` to
get a fresh database per test. Faking Postgres there would defeat the point:
§12.3's overshoot test is a claim about what two transactions do to one row at the
same time, and a post-hoc increment passes every other test in the suite and fails
that one. Nothing about the Docker build depends on it — the queries are checked
at runtime, never by `query_as!` (see `CLAUDE.md`).

CI's `check` job runs `fmt`, `clippy`, `cargo test` and `cargo deny` on every
push. Note this is *not* the house norm — the standard hikari-systems `build.yml`
builds and pushes the image with no test or lint gate. Simmer adds one because its
central correctness argument is a property test. The compose steps do **not** run
on push: the acceptance suite is the manually dispatched `acceptance.yml` workflow
(D-042), so run `docker compose up -d --build` yourself before pushing.

The image CI pushes is built with `--target runtime`. Anything else that builds the
shipped image must pass it too — the Dockerfile's last stage is the acceptance
loadgen, and an unpinned build produces that instead of the server (D-073).

The `docker compose up` step is not ceremony: it is the only thing that exercises
the privileged port-25 bind, the tmpfs the §8.1 buffer spills onto, and the
platform root store the §8.2 `required_verify` mode needs.

## Layout

```
src/config/     the §4.1 schema, ${ENV_VAR} interpolation, §4.2 validation
src/routing/    sender matching (§5.4), domain groups (§3.2.2), the chain walk
  thread.rs       §3.2 step 2a's thread affinity: IDs in, a pinned route out (D-090)
  partial.rs      §3.2 step 3c′'s partial ramp: a keyed hash picks the share (D-091)
src/smtp/       §5 ingress: listeners, state machine, AUTH, DATA buffer, replies
  tls.rs          §5.1's certificate: loaded once, checked by §4.2 the same way
  acl.rs          §5.3's sender grants. Gates acceptance, never routing (D-071)
src/downstream/ §8 outbound: TLS, the SMTP client, the §10.1 reply mapping
  pool.rs         §8.3's per-route pool. max_connections is a bound, not a hint
src/quota/      §7 day index, allowance, the reserve/commit protocol, sweeper
src/frequency/  §7.3 normalisation, the keyed hash, the rolling window, eviction
src/models/     runtime sqlx over &PgPool, house pattern (Postgres build only)
src/rewrite/    §6 the rewriting engine: templates, headers, encoding, stability
src/preflight/  §6.7 the DNS preflight: three checks, a registry, an interval
src/capture/    D-085's debugging capture and D-086's `server replay`.
                Write-only from the delivery path's side; a record carries no
                outcome, which is what keeps it from being a spool
src/relay.rs    decide -> reserve -> rewrite -> relay -> commit/release
src/metrics.rs  §9.1 counters and the Prometheus recorder
src/admin/      the §9 control plane: reads, writes, dry run, /metrics
  view.rs         §9.2's projections, as pure functions. D-026's drift flag
  auth.rs         §9.3's bearer token, and O-11's answer to whose it was
  mutate.rs       the four mutations, the audit line, §14.1's warnings
  dryrun.rs       §9.4, over the real engine
src/db/         one storage backend per build (D-084)
  postgres.rs     the default: sqlx pool and migrations
  mssql.rs        the `mssql` feature: tiberius over bb8, its migration runner
src/quota/mssql.rs  §7.4 over SQL Server: the same protocol, translated
src/healthcheck.rs  the `healthcheck` subcommand the container's HEALTHCHECK runs
src/hash_password.rs  `server hash-password`: argon2id from stdin, never argv
src/bin/loadgen.rs  the acceptance suite's bulk sender; not in the shipped image
migrations/     plain SQL, applied at startup
migrations-mssql/  the same schema in T-SQL, for the `mssql` build
tests/support/  a scripted fake downstream (§12.3)
tests/store_conformance/  §11's storage contract, one suite for both backends
tests/rewrite_stability.rs  §6.6 as a property test over generated messages
tests/admin_api.rs   §9 against the real router and real Postgres
tests/thread_affinity.rs  D-090 end to end, including replies past the cap
tests/partial_ramp.rs     D-091 through the real walk, and dry run against it
tests/pool.rs        §8.3 from the downstream's side: connections, not intentions
tests/ingress_tls.rs §5.1 and §5.3 end to end: STARTTLS, implicit TLS, the ACL
tests/metrics_endpoint.rs  §9.1 against a real recorder; its own binary
tests/acceptance.rs  §12.3 against real mail servers; behind --ignored
simmer.acceptance.yaml  config for the acceptance stack
docs/SPEC.md    the specification
docs/STATE.md   where the build has got to (snapshot, for session handover)
docs/ACCEPTANCE.md  the §12.3 acceptance harness: design, and now built
DECISIONS.md    divergences from it, and the questions still open
LICENSES.md     dependency licence findings
```

## Deployment

Simmer is **not** deployed on the hikari-systems spot fleet. The fleet's roll
method requires target capacity ≥2 and replaces instances one at a time, so a
standard deploy would run two Simmers against one database for the minutes a
replacement takes to build.

**That window is not a quota-overshoot window**, and an earlier version of this
section said it was. The warm-up counters are safe across instances: §7.4's
reservation does its headroom check and its write inside one transaction holding a
row lock, and Postgres serialises contenders for that row whether they are two
tasks in one process or two processes on different hosts. Nothing in the quota
path is per-instance — the counters, the reservations and the route states are all
rows. `tests/quota_multi_instance.rs` races two independent connection pools to
show it.

What the window actually costs is narrower, and neither part is fixed by a lock:

- **Config skew.** `quota_usage.allowance` is authoritative once written (D-026),
  so two instances running different schedules will have whichever writes the
  day's first row set that day's ceiling, and the other will silently honour it.
- **The recipient-frequency race** (D-049). §7.3's count is read outside the
  reservation transaction, so `C` sends that all read before any of them commits
  can take the window to `threshold + (C - 1)`. It is a reputation-shaping
  heuristic rather than an accounting invariant, and it is already true within one
  instance — a second instance widens the window rather than introducing the bug.

Spec §2.2 says one instance owns its quota state, and simmer stays single-instance,
but the reason is the two constraints above rather than the counters. See
`DECISIONS.md` D-061 and D-007, and `docs/MULTI_INSTANCE.md` — which the spec's
author needs to rule on, since §2.2 rules multi-instance out in as many words.

The container publishes no ports by default. Simmer belongs on a trusted internal
segment (spec §2.3) whatever its listeners are configured to do: inbound TLS lets it
sit where cleartext credentials are unacceptable, not where hostile peers can reach
it. Exposing an SMTP port to a host interface must be a deliberate act. To serve
587 or 465, mount a certificate and key readable by UID 1000 and name them in
`server.tls`; see [Listeners and TLS](#listeners-and-tls).
