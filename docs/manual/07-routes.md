# Routes

A route is one way out of Simmer: a provider to relay to, the identity to give
the message, and for a warming route a daily schedule. Routes are listed under
their ramp's `routes`, and chains refer to them by name.

```yaml
routes:
  - name: warming
    downstream:
      host: "smtp.newprovider.com"
      port: 587
      tls: required_verify
      auth: { username: "${WARM_USER}", password: "${WARM_PASSWORD}" }
      pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }
    identity:
      envelope_from: "bounce@newbrand.com"
      set_headers:
        From: "{{original.from.display_name}} <{{original.from.local}}@newbrand.com>"
        Message-ID: "<{{uuid}}@newbrand.com>"
    preflight:
      spf_include: "spf.newprovider.com"
      dkim_selector: "s1"
    warmup:
      started: "2026-11-02T09:00:00Z"
      schedule:
        default: [50, 100, 200, 400, 800]
    recipient_frequency:
      mode: to_address
      window: { unit: daily, count: 1 }
      threshold: 3

  - name: overflow
    overflow: true
    downstream: { ... }
    identity: { ... }
```

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | Unique within the ramp. Used in chains, metrics and the admin API |
| `overflow` | no, default `false` | An overflow route: never capped, last in any chain, and has no `warmup` |
| `downstream` | yes | The provider to relay to ([below](#the-downstream)) |
| `downstream_overrides` | no | A different provider for some domain groups ([below](#a-different-provider-per-domain-group)) |
| `identity` | yes | How the message is rewritten ([below](#the-identity-rewrite)) |
| `preflight` | no | DNS checks of the route's domain ([below](#dns-preflight)) |
| `warmup` | on a warming route; not allowed on an overflow route | The daily caps ([The warm-up schedule](08-warmup.md)) |
| `recipient_frequency` | no | Avoid mailing one recipient too often from this route ([below](#recipient-frequency)) |

## The downstream

```yaml
downstream:
  host: "smtp.newprovider.com"
  port: 587
  tls: required_verify          # default
  auth:
    username: "${WARM_USER}"
    password: "${WARM_PASSWORD}"
  pool:
    max_connections: 4
    idle_ttl: 60s
    max_messages_per_connection: 100
  timeouts:                     # optional; these are the defaults
    connect: 10s
    command: 30s
    data: 120s
  smtputf8: false               # default
  assume_8bitmime: false        # default
```

| Key | Default | Meaning |
|---|---|---|
| `host`, `port` | required | The provider's SMTP server |
| `tls` | `required_verify` | See below |
| `auth` | none | `username` and `password` to log in with. Without it, Simmer does not log in |
| `pool` | required | Connection reuse ([below](#connection-pool)) |
| `timeouts` | 10s / 30s / 120s | Per stage ([below](#timeouts)) |
| `smtputf8` | `false` | The provider accepts internationalised addresses ([below](#international-addresses-and-8-bit-mail)) |
| `assume_8bitmime` | `false` | The provider accepts 8-bit message bodies without saying so ([below](#international-addresses-and-8-bit-mail)) |

`tls` is one of:

| Value | Behaviour |
|---|---|
| `off` | Never use TLS |
| `opportunistic` | Use `STARTTLS` if the provider offers it; carry on without TLS if it does not, or if it fails |
| `required` | `STARTTLS` must succeed, but the certificate is not checked. For an internal server with a self-signed certificate |
| `required_verify` | `STARTTLS` must succeed, and the certificate and hostname are verified against the system's trusted roots. Use this for any provider on the internet |

Simmer uses `STARTTLS`, typically on port 587. It does not speak implicit TLS
(port 465) to providers.

### Connection pool

Simmer keeps connections to each provider open between messages, rather than
connecting and logging in for every one.

| Key | Meaning |
|---|---|
| `max_connections` | The most connections this route has open to the provider at once, at least 1 |
| `idle_ttl` | How long an unused connection is kept before being closed |
| `max_messages_per_connection` | Close a connection after this many messages, at least 1 |

`max_connections` is a hard limit, held while a connection is in use. It
protects the provider from a burst as much as it saves Simmer a login. A message
that cannot get a connection within the `connect` timeout is answered
`451 4.4.5 downstream connection pool exhausted`. If you see that, raise
`max_connections` or ask whether the provider allows more.

Set `idle_ttl` **below** the provider's own idle timeout. If the provider closes
idle connections first, Simmer finds a dead one when it next tries to use it.
That is handled, since the message is retried once on a fresh connection and the
application never knows, but it costs a reconnect, and
`simmer_pool_retries_total` counts it. The retry never happens after the
message body has been sent: at that point the provider may already have
accepted it, and a retry could deliver it twice.

### Timeouts

Each stage of the conversation with the provider has its own timeout:

| Key | Default | Bounds |
|---|---|---|
| `connect` | `10s` | Opening the connection. It is also how long a message waits for a free connection when the pool is full |
| `command` | `30s` | The provider's greeting, and the reply to each command |
| `data` | `120s` | Sending the message body and getting the final reply |

Any of them expiring is answered `451 4.4.2 downstream timeout`, except a full
pool, which is `451 4.4.5`.

**Keep their sum well below your application's own SMTP timeout.** Simmer holds
the application's connection open for the whole conversation with the provider.
If the application gives up first, it sees a dropped connection instead of the
`451` Simmer was about to send it, and a `451` is much more useful. With the
defaults, the slowest possible conversation is about 170 seconds (waiting for a
connection, connecting, one command and the body), against the 300 seconds most
mail libraries wait for a reply to the body.

### What the provider's reply becomes

| The provider | The application gets |
|---|---|
| accepts the message (`2xx` after the body) | `250 2.0.0 accepted`, and the message is counted |
| refuses the recipient (`5xx` to `RCPT TO`) | `550`, with the provider's code and text |
| refuses at any other stage (`5xx`) | `451`, with the provider's code and text. Usually a misconfigured route; counted in `simmer_downstream_config_error_total` |
| defers (`4xx`) | `451`, with the provider's code and text |
| cannot be reached | `451 4.4.1 downstream unavailable` |
| times out | `451 4.4.2 downstream timeout` |
| fails the TLS handshake | `451 4.7.0 downstream TLS failure` |
| breaks the protocol | `451 4.3.0 downstream protocol error` |
| drops the connection after the body, before replying | `451`. The provider may or may not have accepted it, so a retry may deliver a duplicate. Counted in `simmer_ambiguous_delivery_total` |

Only an accepted message is counted against the route's allowance.

### International addresses and 8-bit mail

**`smtputf8`.** Addresses with non-ASCII characters (`jöhn@exämple.de`) need
the provider to support SMTPUTF8. Simmer decides which route a message takes
only after it has the message, but it has to tell the client whether such
addresses are accepted at the very start of the connection. So it offers
SMTPUTF8 only if **every** route that any chain can reach, and every
`downstream_overrides` entry, says `smtputf8: true`. Otherwise an internationalised
address is refused `550 5.6.7` at `MAIL FROM`, before anything is sent.

**`assume_8bitmime`.** A body with bytes above 127 (UTF-8 text sent as `8bit`,
which modern libraries do) can go only to a provider that accepts 8-bit mail.
Most say so, and Simmer checks. A few accept it without saying so; Postal is
one. For those, set `assume_8bitmime: true`. Without it, an 8-bit message to
such a provider is answered `451 4.3.5` and counted in
`simmer_downstream_config_error_total`. A body that is plain ASCII goes
anywhere either way.

## A different provider per domain group

Normally a route sends everything to its one `downstream`. `downstream_overrides`
sends mail for some domain groups to a different provider instead, for example a
dedicated IP pool for Google:

```yaml
downstream:
  host: smtp.newprovider.com
  port: 587
  pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }
downstream_overrides:
  google:
    host: smtp-google.newprovider.com
    port: 587
    tls: required_verify
    auth: { username: "${G_USER}", password: "${G_PASSWORD}" }
    pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }
```

- **An override is a complete downstream.** Nothing is inherited from
  `downstream`: give it its own `auth`, `tls`, `pool`, `timeouts`, `smtputf8` and
  `assume_8bitmime`. An override without `auth` does not log in.
- **It changes only where the message is sent.** The route, its identity, its
  allowance and its place in the chain are unchanged. A failure at the override
  is reported like any other; it is never retried at the route's own
  `downstream`.
- **Each override has its own pool.** Groups without an override share the
  route's pool.
- A key must name a domain group in the ramp, and a group named `default` cannot
  have one, because metrics use `downstream="default"` for the route's own.
- If [DNS preflight](#dns-preflight) is on, the route's `spf_include` must cover
  the override's servers as well.
- After the warm-up, the application, or its own mail server, has to make the
  same choice. For groups matched by domain that is simple. For groups matched
  by [mail server](06-routing.md#grouping-by-mail-server), it needs a mail
  server that can route on MX records.

## The identity rewrite

`identity` says what the message looks like when it leaves through this route.

```yaml
identity:
  envelope_from: "bounce@newbrand.com"
  remove_headers: ["Return-Path", "X-Mailer"]
  header_rewrites:
    - header: List-Unsubscribe
      pattern: 'https://oldbrand\.com/'
      replacement: "https://click.newbrand.com/"
  set_headers:
    From: "{{original.from.display_name}} <{{original.from.local}}@newbrand.com>"
    Reply-To: "{{original.from.address}}"
    Message-ID: "<{{uuid}}@newbrand.com>"
  unstable_headers: ["Reply-To"]
  body_rewrites:
    - pattern: 'https://oldbrand\.com/'
      replacement: "https://newbrand.com/"
```

They are applied in this order:

1. Simmer removes the authentication headers any inbound signature left
   (`DKIM-Signature`, `Authentication-Results`, `ARC-Seal`,
   `ARC-Message-Signature`, `ARC-Authentication-Results`) and `X-Simmer-Ramp`.
   This always happens. A rewritten message would fail the old signature, and a
   failing signature is judged more harshly than none; the provider signs the
   new one.
2. `remove_headers`
3. `header_rewrites`
4. `set_headers`
5. `body_rewrites`
6. Simmer adds a `Received:` header naming itself.
7. The envelope sender is set from `envelope_from`.

| Key | Meaning |
|---|---|
| `envelope_from` | Required. The envelope sender (`MAIL FROM`), which is where bounces go and what SPF checks. Its domain must be written out |
| `set_headers` | Headers to set, in order. Replaces every existing instance of a header. Values are templates |
| `remove_headers` | Headers to remove. Naming a header here and in `set_headers` replaces it |
| `header_rewrites` | Find-and-replace inside a header's value |
| `body_rewrites` | Find-and-replace inside the text of the message |
| `unstable_headers` | Headers allowed to differ before and after the application is reconfigured ([below](#stability-why-a-rewrite-may-be-refused)) |

A header you do not mention is passed through with its original bytes untouched.

### Which headers to set

| Header | Usually |
|---|---|
| `From:` | The reputation-bearing one. Set it to the route's domain |
| `Message-ID:` | `<{{uuid}}@yourdomain>`: a fresh ID at the route's domain. Required for [thread affinity](09-thread-affinity.md) |
| `Reply-To:` | `{{original.from.address}}`, so replies still reach the original mailbox while the `From:` domain changes. Migration-only, so declare it in `unstable_headers` |
| `Sender:` | Set it when `From:` differs from the envelope sender, if your provider wants it |
| `List-Unsubscribe`, `List-Unsubscribe-Post` | Mailbox providers weigh these heavily for bulk mail. Rewrite their domain to the route's |
| `Return-Path:` | Remove it; the provider sets it |

### Templates

`envelope_from` and every `set_headers` value can contain variables in
`{{ }}`:

| Variable | Value |
|---|---|
| `original.from.address` | The first `From:` address, as received |
| `original.from.local`, `original.from.domain` | Its parts before and after the `@` |
| `original.from.display_name` | Its display name, or empty |
| `original.envelope_from.address`, `.local`, `.domain` | The envelope sender, and its parts |
| `original.message_id` | The original `Message-ID:`, or empty |
| `original.subject` | The subject, decoded |
| `original.header["X-Foo"]` | Any header as received, or empty |
| `recipient.address`, `.local`, `.domain` | The recipient |
| `route.name` | The route's name |
| `correlation_id` | Simmer's ID for this message, also in its log lines |
| `uuid` | A new random UUID |
| `now.rfc3339`, `now.date` | The current time (`2026-11-02T09:00:00Z`), or date (`2026-11-02`) |

An unknown variable stops startup. Values taken from the message are escaped
where they are inserted, so a display name containing `<` or `"` cannot change
the header's structure, and non-ASCII text is encoded as headers require.

### `header_rewrites`

A template replaces a whole header. `header_rewrites` changes part of a value and
keeps the rest, which is what a `List-Unsubscribe` link with a per-message token
needs:

```yaml
header_rewrites:
  - header: List-Unsubscribe
    pattern: 'https://links\.oldbrand\.com/'
    replacement: "https://click.newbrand.com/"
```

- `pattern` is a regular expression, and `replacement` may use `$1`, `${name}`
  for groups it captured.
- Entries apply in order, to every instance of the header.
- Matching is against the header's decoded value: line folding undone, and
  encoded non-ASCII text decoded.
- A header whose value no pattern changes keeps its original bytes.
- A header that cannot be decoded is left alone, logged at `WARN` and counted in
  `simmer_header_rewrite_skipped_total`.
- Not allowed for `From`, `Sender`, `Message-ID` and the envelope sender. Set
  those with `set_headers`.
- If `set_headers` also sets the header, `set_headers` wins, and Simmer warns
  that the rewrite never takes effect.

### `body_rewrites`

```yaml
body_rewrites:
  - pattern: 'https://oldbrand\.com/'
    replacement: "https://newbrand.com/"
```

Applied in order to every `text/*` part (plain text and HTML) of the message,
after decoding it, so a link split across a quoted-printable line break still
matches. Attachments and other non-text parts are never touched, and nor are
signed or encrypted parts. A part that cannot be decoded is left alone, logged
at `WARN` and counted in `simmer_body_rewrite_skipped_total`. A part no pattern
matched keeps its original bytes.

Use it for links to the old domain. If those links go through a click-tracking
domain, see [The link proxy](10-link-proxy.md).

### Stability: why a rewrite may be refused

At startup, Simmer applies each route's rewrite to a sample message, then
applies it again to the result, and refuses to start if anything changed the
second time. That is what makes the rewrite safe to remove later: mail that the
application already sends in the new form must pass through unchanged. Some
consequences:

- **`envelope_from`'s domain must be written out.** `bounce@{{original.envelope_from.domain}}`
  is refused even though applying it twice gives the same result, because a
  route's whole purpose is to build reputation for one domain. The part before
  the `@` may use variables, as long as it is stable:
  `bounce+{{recipient.local}}@newbrand.com` is fine.
- **A rewrite may not build on the value it replaces.**
  `bounce+{{original.envelope_from.local}}@newbrand.com` is refused: applied to
  its own output, it gives `bounce+bounce+jane@newbrand.com`.
- **A `body_rewrites` or `header_rewrites` pattern may not match its own
  output.** Replacing `example.com` with `www.example.com` would match again on
  the next pass. Anchor the pattern, for example on `https://example\.com/`.
- **`unstable_headers`** names headers that are allowed to differ, because they
  are deliberately migration-only. The usual one is `Reply-To:
  "{{original.from.address}}"`: it reads `From:`, which the same route rewrites,
  so it changes the moment the application is switched to the new domain.
  Naming it acknowledges that. `From`, `Sender`, `Message-ID` and the envelope
  sender can never be named there.

The error message shows the value after one pass and after two, which is usually
enough to see the problem.

## DNS preflight

The worst way a warm-up can fail is silently: the provider signs your mail with
its own domain because its domain authentication for yours was never finished.
The route then ramps up perfectly for weeks and your domain builds no reputation
at all. Preflight checks for it, by looking up the DNS records of the domain in
`envelope_from`:

```yaml
preflight:
  enabled: true                   # default when the block is present
  spf_include: "spf.newprovider.com"
  dkim_selector: "s1"
  require_dmarc: true             # default false
  strict: false                   # default
```

| Check | Passes when |
|---|---|
| SPF | the domain has a `v=spf1` TXT record that contains `spf_include` |
| DKIM | `<dkim_selector>._domainkey.<domain>` has a TXT record with a non-empty `p=` key |
| DMARC | `_dmarc.<domain>` has a `v=DMARC1` record. Checked only with `require_dmarc: true` |

`spf_include` and `dkim_selector` are required when preflight is enabled. Your
provider's documentation for authenticating a domain gives both. Without a
`preflight` block, nothing is checked.

The checks run at startup and every 15 minutes. A failure is logged at `WARN`
and sets `simmer_preflight_ok{route,check}` to 0, and the route's state in the
admin API shows which check failed. By default that is all: a temporary DNS
problem should not stop mail. With `strict: true`, a route failing a check is
skipped, so its mail goes to the next route in the chain.

## Recipient frequency

A route can avoid mailing the same recipient too often. Over the threshold, the
route is skipped for that message, which goes to the next route in the chain.
Nothing is dropped: it is a way of steering mail, not of suppressing it.

```yaml
recipient_frequency:
  mode: to_address           # or to_domain
  window: { unit: daily, count: 1 }
  threshold: 3
```

| Key | Meaning |
|---|---|
| `mode` | `to_address` counts per recipient address; `to_domain` per recipient domain |
| `window` | A rolling window: `count` units of `hourly`, `daily` or `weekly`. `{unit: daily, count: 1}` is any 24 hours, not since midnight |
| `threshold` | At this many messages from this route in the window, the route is skipped. At least 1 |

With `to_address`, addresses are normalised first, so variants of one mailbox
count together: case is ignored, a `+tag` is removed (`jane+news@` counts as
`jane@`), and for the domains in the top-level `dot_insensitive_domains` list
(default `gmail.com` and `googlemail.com`) dots are removed too
(`j.ane@gmail.com` counts as `jane@gmail.com`).

Addresses are never stored: each is kept as a keyed hash, and only for the
length of the longest window. Each ramp and route counts separately.

Avoid putting a frequency limit on the last route in a chain. A recipient over
the limit there has nowhere left to go, and gets `451`. Simmer warns about it.
