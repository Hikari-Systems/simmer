# Listeners, TLS and SMTP users

The `server` block is the SMTP side that your applications connect to: which
ports Simmer listens on, whether they use TLS, who may log in and what each user
may send as, and the limits that protect Simmer from a misbehaving client.

```yaml
server:
  listeners:
    - address: "0.0.0.0:587"
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"
    private_key: "/etc/simmer/tls/privkey.pem"
  hostname: "simmer.internal"
  max_message_bytes: 26214400
  max_recipients: 1
  max_concurrent_sessions: 64
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth:
    users:
      - username: "app"
        password_hash: "${SIMMER_APP_HASH}"
        grants:
          send_as: ["oldbrand.com", "newbrand.com"]
```

## Simmer belongs on an internal network

Simmer is a relay for your own applications. It is **not** a public mail server,
and nothing about it, including TLS, makes it safe to expose to the internet.
`allowed_cidrs` limits which addresses may connect at all, and should list only
your applications' networks. TLS lets Simmer sit on a network where passwords in
clear are unacceptable; it does not make hostile clients safe.

## Listeners

`listeners` has one entry per port. Each has an `address`, and optionally `tls`
and `auth` settings. An entry with only an address takes the standard settings
for its port:

| Port | `tls` | `auth` | Standard |
|---|---|---|---|
| 25 | `off` | `optional` | SMTP relay (RFC 5321) |
| 587 | `starttls_required` | `required` | Message submission (RFC 6409) |
| 465 | `implicit` | `required` | Submission over TLS (RFC 8314) |
| any other | `off` | `optional` | |

Startup logs the effective settings of each listener.

`tls` is one of:

| Value | Meaning |
|---|---|
| `off` | No TLS. `STARTTLS` is not offered |
| `starttls` | `STARTTLS` is offered; a client may use it or not |
| `starttls_required` | `STARTTLS` is offered, and nothing but `EHLO`, `NOOP`, `RSET`, `QUIT` and `STARTTLS` is accepted until the client has used it |
| `implicit` | TLS from the first byte, as on port 465 |

`auth` is one of:

| Value | Meaning |
|---|---|
| `disabled` | Logging in is not offered |
| `optional` | Logging in is offered, but a client that does not log in may still send |
| `required` | A client must log in before `MAIL FROM` |

```yaml
listeners:
  - address: "0.0.0.0:25"
    auth: required            # port 25, but only for logged-in clients
  - address: "0.0.0.0:587"    # STARTTLS required, login required
  - address: "0.0.0.0:465"    # implicit TLS, login required
  - address: "0.0.0.0:2525"
    tls: starttls
    auth: required
```

Two listeners may not share an address. A listener can also carry `ramp` and
`header_overrides_affinity`, which choose the routing setup for mail arriving on
it; see [Routing](06-routing.md#choosing-a-ramp-per-port-or-per-message).

**Ports below 1024.** The Docker image runs as an unprivileged user (UID 1000).
To listen on 25, 465 or 587, add `--cap-add NET_BIND_SERVICE` to `docker run`
(or `cap_add: [NET_BIND_SERVICE]` in compose). Outside Docker, either run on a
port above 1024 or grant the binary the capability with
`setcap 'cap_net_bind_service=+ep' ./server`.

## TLS

Any listener whose `tls` is not `off` uses the certificate in `server.tls`:

```yaml
tls:
  certificate: "/etc/simmer/tls/fullchain.pem"   # PEM; the server certificate first, then the chain
  private_key: "/etc/simmer/tls/privkey.pem"     # PEM
```

- **It is loaded and checked at startup**, exactly as the listener will use it.
  A missing, unreadable or unparseable file, or a key that does not match the
  certificate, stops startup with the other configuration errors.
- **In Docker, the files must be readable by UID 1000.** Mount the directory
  read-only (`-v "$PWD/tls:/etc/simmer/tls:ro"`) and make sure the key is
  readable by that user; the startup error says so if it is not.
- **A certificate that does not cover `server.hostname`**, or that expires within
  14 days, is a warning rather than an error: refusing to start would also take
  down the listeners that need no certificate.
- **Renewal needs a restart.** There is no automatic certificate management and
  no reload. Restart Simmer after replacing the files.
- **Plaintext sent straight after `STARTTLS` closes the connection.** Bytes a
  client sends in the same packet as `STARTTLS`, before the handshake, could have
  been injected by anyone on the network, so they are not processed.

## Logging in: `auth`

```yaml
auth:
  allow_insecure_auth: false     # default
  mechanisms: [PLAIN, LOGIN]     # default
  users:
    - username: "app"
      password_hash: "${SIMMER_APP_HASH}"
      grants:
        send_as: ["oldbrand.com", "*.oldbrand.com", "newbrand.com"]
        ramps: []                # default
```

| Key | Default | Meaning |
|---|---|---|
| `allow_insecure_auth` | `false` | Allow logging in on a connection that is not encrypted. Leave it `false` wherever TLS is available; set it `true` only for a listener with `tls: off` on a trusted network, as the starter configuration does |
| `mechanisms` | `[PLAIN, LOGIN]` | The SMTP AUTH mechanisms offered |
| `users` | none | The accounts applications log in with |

A connection that fails to log in three times is disconnected with `421`.

A listener with `auth: required` and `tls: off` needs `allow_insecure_auth: true`,
or nobody could ever log in on it; Simmer refuses to start rather than run it.
With `allow_insecure_auth: false`, logging in on an unencrypted connection is
refused `538 5.7.11` and is not offered there.

### Passwords

Passwords are stored as argon2id hashes. Make one with the `hash-password`
subcommand, which reads the password from standard input:

```sh
printf '%s' "$PASSWORD" | docker run -i --rm hikarisystems/simmer:latest hash-password
# or, with a binary you built:
printf '%s' "$PASSWORD" | ./server hash-password
```

It prints a string starting `$argon2id$`. Put it in the configuration, ideally
through an environment variable, and quote it in YAML (`"${SIMMER_APP_HASH}"`).
The password is never accepted as a command-line argument, where it would end up
in your shell history and the process list.

### What a user may send as: `grants`

Each user lists the sender identities it may use, in `grants.send_as`. Each entry
is one of:

| Pattern | Example | Allows |
|---|---|---|
| A domain | `oldbrand.com` | any address `@oldbrand.com` |
| A subdomain wildcard | `*.oldbrand.com` | `@mail.oldbrand.com`, `@a.b.oldbrand.com`; **not** `@oldbrand.com` itself |
| An address | `alerts@oldbrand.com` | that address only |

For a logged-in client, both the envelope sender (`MAIL FROM`) and the first
address in the `From:` header must match one of the user's patterns. Anything
else is refused `550 5.7.1 sender not permitted`: the envelope at `MAIL FROM`,
the header once the message has been sent. A message without a readable
`From:` is refused too. An empty envelope sender (`MAIL FROM:<>`) is allowed, and
judged by its `From:` alone.

**Grants decide whether a message is accepted, never where it goes.** Routing is
decided by `senders` (see [Routing](06-routing.md#sender-rules)), and which user
sent a message plays no part in it: two users allowed the same identity produce
identical mail. The one exception is `grants.ramps`, which lists the ramps this
user may pick with an `X-Simmer-Ramp` header (see
[Routing](06-routing.md#choosing-a-ramp-per-port-or-per-message)).

**Grants apply only to clients that logged in.** On a listener with `auth:
optional`, which includes port 25 by default, a client that does not log in may
use any sender, and Simmer warns about each such listener at startup once users
exist. Set `auth: required` wherever that is more trust than you want.

Watch `simmer_sender_not_permitted_total`: it means either an application is
misconfigured or someone is using its credentials.

## Identity and limits

| Key | Example | Meaning |
|---|---|---|
| `hostname` | `simmer.internal` | Simmer's name in its SMTP greeting and in the `Received:` header it adds. The TLS certificate should cover it |
| `max_message_bytes` | `26214400` | The largest message accepted, in bytes. Advertised to clients as `SIZE`; a larger one is refused `552 5.3.4`. Keep it at or below your providers' limits |
| `max_recipients` | `1` | Recipients per message. Must be at least 1, and has no effect above 1: a message may have only one recipient (see [How Simmer works](02-concepts.md#one-recipient-per-message)). Simmer warns if it is set higher |
| `max_concurrent_sessions` | `64` | SMTP connections open at once, across every listener. Further connections get `421 4.3.2 too many connections` |
| `allowed_cidrs` | `["10.0.0.0/8"]` | The networks clients may connect from, across every listener. Any other address is disconnected before a greeting. Must not be empty |

All five are required.

`max_concurrent_sessions` also bounds memory: each connection can hold one
message, up to 1 MiB of it in memory and the rest in the temporary directory.

## Client timeouts

```yaml
timeouts:
  command: 30s      # waiting for the client's next command
  data: 300s        # receiving the message body
  session: 600s     # the whole connection
```

All three are required. A client that exceeds one is disconnected.

These are about the **client**. Simmer's own timeouts towards the provider are
set per route, and they must fit inside the client's patience rather than these
values; see [Routes](07-routes.md#timeouts).

## The SMTP Simmer speaks

For reference, when debugging an application's mail library:

- `EHLO` offers `PIPELINING`, `8BITMIME`, `SIZE`, `STARTTLS` where the listener
  offers it, `AUTH PLAIN LOGIN` where logging in could succeed on this
  connection, and `SMTPUTF8` only when every route can deliver it (see
  [Routes](07-routes.md#international-addresses-and-8-bit-mail)).
- `HELO`, `RSET`, `NOOP` and `QUIT` work as usual. `VRFY` always answers `252`,
  `EXPN` and `BDAT` answer `502`.
- A message's end is the standard `CRLF.CRLF` and nothing else. A message
  containing a `.` line next to a bare CR or LF is refused `554 5.6.0`: that is
  the pattern of an SMTP smuggling attack, and `simmer_ambiguous_terminator_total`
  counts it.
