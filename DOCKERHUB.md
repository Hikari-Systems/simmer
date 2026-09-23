# Simmer

**An SMTP relay facade that applies a domain-reputation warm-up ramp — and can be
removed without anyone noticing it was there.**

Simmer sits between your application and one or more real SMTP providers. It
accepts a message, picks an outbound route by quota state, rewrites the message's
identity to match that route, forwards it **synchronously**, and returns the
downstream's verdict to the client on the same connection.

It is **temporary infrastructure**: it exists for the length of a warm-up and is
then taken out, in either order relative to reconfiguring the application.

<!-- current-version: source of truth for the release number. Keep in sync with the git tag, README.md, and Cargo.toml; enforced by .githooks/pre-push and the release CI. -->
**Current version: `v0.7.1`** — [all releases](https://github.com/Hikari-Systems/simmer/releases).

📦 **Source, issues & full documentation:**
[github.com/Hikari-Systems/simmer](https://github.com/Hikari-Systems/simmer)

---

## Features

- **A per-provider warm-up ramp.** Quota is keyed on `(route, domain group)` —
  Google, Microsoft, Yahoo, a catch-all — because mailbox providers throttle
  independently. The schedule is indexed by *elapsed days* from `warmup.started`,
  so it is immune to DST and calendar edges.
- **Fall-through, never drop.** A route that is out of allowance, paused, or over
  its recipient-frequency threshold is skipped and the message moves to the next
  route in its chain, ending at an uncapped (but counted) overflow route.
- **Identity rewriting** — `envelope_from`, `From:`, `Message-ID:`, arbitrary
  headers and body URL rewrites — checked at startup to be **stable** (applying it
  twice changes nothing), so already-migrated traffic is never corrupted.
- **Exact quota under concurrency.** Reservations are made inside one Postgres
  transaction holding a row lock; a chain exhausted by quota answers `451`, never
  `550`, so no recipient lands on a suppression list that outlives Simmer.
- **Listeners with real TLS**: port 25 (transfer), 587 (`STARTTLS` required) and
  465 (implicit TLS), each with its own `tls` and `auth` policy, plus per-user
  sender grants.
- **DNS preflight** per route (SPF include, DKIM selector, DMARC), so a
  downstream that signs with its own domain instead of yours is caught before the
  ramp spends weeks building no reputation.
- **Per-route connection pools**, a bounded timeout budget, and correct
  `SMTPUTF8` / `8BITMIME` negotiation.
- **An optional HTTP link proxy** for the tracking and unsubscribe links your
  routes rewrite.
- **A control plane** on port 8080: health, Prometheus metrics, live route and
  quota state, pause / resume / graduate / one-day allowance overrides, and a
  **dry run** that shows exactly what a message would do without sending it.

## What it is not

Simmer is **not an MTA**. There is no spool, no queue, no retry scheduler and no
DSN generation. The client connection is held for the whole downstream
conversation: a message is either delivered during it or the client is told it
was not. The only persisted state is quota and recipient-frequency events.

It holds **no DKIM keys** and signs nothing — the downstream signs, exactly as it
would if the application connected to it directly.

It is **not a public MX**. It belongs on a trusted internal segment; TLS lets it
sit where cleartext credentials are unacceptable, not where hostile peers can
reach it.

---

## What's in the image

| Path | |
|---|---|
| `/app/server` | The binary. It is the `ENTRYPOINT` |
| `/app/simmer.yaml` | An example configuration, commented throughout |
| `/app/migrations` | Schema migrations, applied automatically at startup |

- Base: `debian:bookworm-slim` with `ca-certificates`; no shell tools beyond that.
- Runs as **non-root, UID 1000**.
- **No `EXPOSE`** — publishing an SMTP port is a deliberate act in your run
  command or compose file.
- `HEALTHCHECK` runs `/app/server healthcheck` (liveness of the admin port, no
  database round-trip), so a database outage does not turn into a restart loop.

Subcommands, as the first argument:

| | |
|---|---|
| *(none)* | Run the relay |
| `healthcheck` | Exit 0 if the admin port answers `GET /healthcheck` |
| `hash-password` | Read a password on stdin, print an argon2id PHC string |
| `replay` | Re-send captured messages to another Simmer (D-086). **Delivers mail.** |

## Two images: Postgres and SQL Server

Each release is published twice, in the same repository:

| Tag | Database | Platforms |
|---|---|---|
| `:vX.Y.Z`, `:latest` | PostgreSQL | linux/amd64, linux/arm64 |
| `:vX.Y.Z-mssql`, `:latest-mssql` | SQL Server 2017+ / Azure SQL | linux/amd64, linux/arm64 |

Same code, same configuration file, same control plane and metrics; only the
storage layer differs, and each image contains exactly one. Give either image
the other's `database.url` and it refuses to start, naming the image to use.

## Requirements

- **PostgreSQL** (tested against 18) for the plain tags, or **SQL Server** (2017
  or later, or Azure SQL; tested against 2022) for the `-mssql` tags. The login
  needs to be able to create the schema on first start.
- One or more downstream SMTP providers with credentials.
- A configuration file — mount your own; the bundled `/app/simmer.yaml` is an
  example.

## Quick start

### 1. Mint a password hash for an SMTP user

The password is read from stdin, never taken as an argument where it would land in
shell history:

```sh
printf '%s' "$PASSWORD" | docker run -i --rm hikarisystems/simmer:latest hash-password
# → $argon2id$v=19$m=19456,t=2,p=1$....
```

### 2. Write a configuration

Start from the bundled example:

```sh
docker run --rm --entrypoint cat hikarisystems/simmer:latest /app/simmer.yaml > simmer.yaml
```

It is YAML with `${ENV_VAR}` interpolation for secrets. The example expects:

| Variable | |
|---|---|
| `DATABASE_URL` | Postgres connection URL |
| `SIMMER_ADMIN_TOKEN` | Bearer token for the admin API (16+ random characters) |
| `SIMMER_CFAPP_HASH` | argon2id hash for the example SMTP user `cfapp` |
| `POSTAL_USER`, `POSTAL_PASS` | Credentials for the example warming route |
| `SENDGRID_KEY` | Credential for the example overflow route |

Rename and replace these to suit your own routes.

### 3. Run it

```sh
docker run -d --name simmer \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,size=512m \
  --cap-add NET_BIND_SERVICE \
  -v "$PWD/simmer.yaml:/etc/simmer/simmer.yaml:ro" \
  -e SIMMER_CONFIG=/etc/simmer/simmer.yaml \
  -e DATABASE_URL=postgres://simmer:…@db:5432/simmer \
  -e SIMMER_ADMIN_TOKEN=… -e SIMMER_CFAPP_HASH='$argon2id$…' \
  -e POSTAL_USER=… -e POSTAL_PASS=… -e SENDGRID_KEY=… \
  --network your-internal-network \
  -p 127.0.0.1:8080:8080 \
  hikarisystems/simmer:latest
```

- `NET_BIND_SERVICE` is needed because the process is non-root and binds port 25.
- `/tmp` holds message bodies above 1 MiB while they are in flight. It is a
  transient buffer, not a spool — a tmpfs guarantees nothing survives a crash.
- Reach the SMTP listener over the container network. If you must publish it to a
  host, bind loopback (`-p 127.0.0.1:2525:25`), never a routable interface.

### 4. Check it

```sh
curl -s localhost:8080/health | jq     # liveness + database reachability
```

---

## Configuration

Every key is validated at startup, **unknown keys are rejected**, and **all
violations are reported together** rather than one per restart. An unresolvable
`${ENV_VAR}` is a startup error. There is no hot reload: changes need a restart.

`SIMMER_CONFIG` sets the config path (default `simmer.yaml` in `/app`).

### Listeners and TLS

`server.listeners` has one entry per port; an entry naming only an address takes
its port's RFC defaults:

| Port | `tls` | `auth` | |
|---|---|---|---|
| 25 | `off` | `optional` | RFC 5321 transfer |
| 587 | `starttls_required` | `required` | RFC 6409 submission |
| 465 | `implicit` | `required` | RFC 8314 submissions |
| any other | `off` | `optional` | |

```yaml
server:
  listeners:
    - address: "0.0.0.0:25"
      auth: required
    - address: "0.0.0.0:587"
    - address: "0.0.0.0:465"
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"   # leaf first
    private_key: "/etc/simmer/tls/privkey.pem"     # readable by UID 1000
  auth:
    allow_insecure_auth: false
```

The certificate is loaded and checked at startup. `allow_insecure_auth` defaults
to false: AUTH over an unencrypted session is refused `538 5.7.11`.

### Sender grants

Each SMTP user carries the sender identities it may present — an exact domain,
`*.subdomain`, or a full address. Anything outside them is `550 5.7.1 sender not
permitted`. Grants decide whether a message is **accepted**, never where it goes;
routing is the `senders` list.

```yaml
    users:
      - username: "cfapp"
        password_hash: "${SIMMER_CFAPP_HASH}"
        grants:
          send_as: ["oldbrand.com", "*.oldbrand.com", "newbrand.com"]
```

### SQL Server (`-mssql` images)

`database.url` is an ADO.NET or JDBC connection string instead of a Postgres URL:

```yaml
database:
  url: "server=tcp:sql.internal,1433;database=simmer;user id=simmer;password=${SIMMER_DB_PASSWORD}"
```

- The connection is **encrypted by default**, queries included, whether or not
  the string says `encrypt=true`. An explicit `encrypt=false` opts out, with a
  startup warning. A self-signed server certificate needs
  `TrustServerCertificate=true`, which also warns.
- Route and domain-group names are limited to **200 characters**.
- Names stay **case-sensitive**, as with Postgres: the tables use a binary
  collation.
- SQL Server authentication only; no Windows/Kerberos integrated login.

### Routes and the ramp

```yaml
senders:                       # first match wins
  - match: "oldbrand.com"
    match_on: from_header
    chain: [warming-newbrand, overflow-established]
default_chain: [overflow-established]

routes:
  - name: warming-newbrand
    downstream: { host: "smtp.postal.internal", port: 587, tls: required_verify, … }
    identity:
      envelope_from: "bounce@newbrand.com"       # absolute; the domain must be literal
      set_headers:
        From: "{{original.from.display_name}} <sales@newbrand.com>"
    warmup:
      started: "2026-08-01T09:00:00Z"
      schedule:
        default: [50, 100, 200, 400, 800, 1500, 3000, 5000]
        overrides:
          google: [20, 50, 100, 250, 500, 1000, 2000, 4000]
  - name: overflow-established
    overflow: true                                # uncapped, counted, must be last
    …
```

Past the end of a schedule the final value repeats; a route never auto-graduates.
Unused allowance does not carry over.

**Two rules that catch people out:**

- Rewrites are **absolute assignments**, never relative transformations —
  `bounce+{{original.envelope_from.local}}@…` is refused at startup because
  applying it to already-migrated mail prepends `bounce+` again.
- A route's `envelope_from` must have a **constant domain**. A route whose domain
  varies per message warms nothing while its quota row still looks like a healthy
  ramp.

### Timeout budget

Keep the sum of a route's downstream timeouts (`connect` + `command` + `data`;
160 s by default) comfortably below your **client's** SMTP timeout, or the client
sees a dropped connection instead of the `451` Simmer was about to send.

### Link proxy (optional)

An HTTP/1.x forwarder for rewritten tracking and unsubscribe links, meant to sit
behind a TLS-terminating load balancer:

```yaml
link_proxy:
  listen: "0.0.0.0:80"
  upstream: "https://link.domain2.com/tracking"
  allowed_cidrs: ["10.0.0.0/8"]      # the load balancer's subnets
```

Put **only** this port in the load balancer's target group — never an SMTP port —
and health-check the admin port's `/health`.

---

## Control plane

On `admin.listen` (port 8080 by default). `/health`, `/healthcheck` and `/metrics`
(served only with `admin.metrics: true`, off by default since v0.7.0) are open; everything else needs `Authorization: Bearer <token>`.

| | |
|---|---|
| `GET /health` | Liveness plus database reachability; `503` when the database is down |
| `GET /metrics` | Prometheus exposition, when `admin.metrics: true` (off by default since v0.7.0) |
| `GET /routes`, `GET /routes/{name}` | Config plus live state: warm-up day, allowance and usage per group, preflight, pool stats |
| `GET /quota?route=&group=` | The same windows, filtered |
| `POST /routes/{name}/pause`, `/resume` | Take a route out of rotation without a restart |
| `POST /routes/{name}/graduate` | Pin to the final schedule value |
| `POST /routes/{name}/allowance` | Override one group's allowance until the route's next day boundary |
| `POST /quota/reset` | Destructive; needs `"confirm": "reset"` |
| `POST /dryrun` | What *would* happen — sends, reserves and writes nothing |

```sh
# Why is mail deferring for Google?
curl -sH "Authorization: Bearer $TOKEN" localhost:8080/quota?group=google | jq

# What would this message do?
curl -sXPOST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"envelope_from":"jane@oldbrand.com","from_header":"Jane <jane@oldbrand.com>",
       "recipients":["bob@gmail.com"],"body":"see https://oldbrand.com/x"}' \
  localhost:8080/dryrun | jq
```

Every mutation is audited at `INFO` with the acting token's name.

---

## Deployment notes

- **Run one instance.** The quota counters are safe across instances, but two
  instances with different schedules, or a recipient-frequency window read by
  both, will disagree. A rolling deploy that briefly runs two is tolerable; a
  standing pair is not.
- **Leave `database.fail_closed: true`.** A quota enforcer that stops enforcing
  when its database is unreachable provides no guarantee at all.
- **A captured record reaches the file within ten buffered lines or 500 ms of
  quiet** (0.3.1 and later), so `tail -f` on the current bucket is useful. Those
  are flushes, not `fsync`s; only `on_error: defer` is durable before the client
  is answered.
- **If you enable `capture:`, mount a writable volume for it and remember what
  is in it.** The image runs as UID 1000 with a read-only root filesystem, so the
  directory must be a volume that UID owns — a host bind mount usually is not.
  Simmer creates it `0700` and its files `0600`. It holds every recipient
  address and every message body in plaintext: treat the volume as you would a
  mailbox, alert on `simmer_capture_disk_bytes` (live from 0.3.1; on 0.3.0 it
  only moved on the hourly retention sweep, so it read 0 for the first hour and
  `simmer_capture_bytes_total` was the number to use), and delete it when the
  investigation is over. Capture is a debugging mode and is off unless
  configured; startup warns on every boot while it is on.
- **Alert on `simmer_unmatched_sender_total`** (a sender typo sends unwarmed
  traffic at full volume via the default chain) and
  **`simmer_sender_not_permitted_total`** (a misconfigured app, or someone
  else's credentials).
- **Alert on `simmer_ambiguous_terminator_total`**. A message
  whose `DATA` held an end-of-data marker with a bare CR or LF beside it is
  refused `554` and relayed nowhere — the SMTP-smuggling shape (D-095). A steady
  low rate is usually one application emitting bare line endings; anything else
  is an attempt to inject a second envelope through your warming identity, and
  the sender is worth finding.

## docker compose example

```yaml
services:
  simmer:
    image: hikarisystems/simmer:latest
    read_only: true
    cap_add: [NET_BIND_SERVICE]
    tmpfs:
      - /tmp:rw,noexec,nosuid,size=512m
    volumes:
      - ./simmer.yaml:/etc/simmer/simmer.yaml:ro
      # - ./tls:/etc/simmer/tls:ro          # for 587 / 465
    environment:
      SIMMER_CONFIG: /etc/simmer/simmer.yaml
      DATABASE_URL: postgres://simmer:simmer@simmer-db:5432/simmer
      SIMMER_ADMIN_TOKEN: ${SIMMER_ADMIN_TOKEN}
      SIMMER_CFAPP_HASH: ${SIMMER_CFAPP_HASH}
      POSTAL_USER: ${POSTAL_USER}
      POSTAL_PASS: ${POSTAL_PASS}
      SENDGRID_KEY: ${SENDGRID_KEY}
    ports:
      - "127.0.0.1:8080:8080"                # admin API; SMTP is not published
    depends_on:
      simmer-db:
        condition: service_healthy

  simmer-db:
    image: postgres:18
    environment:
      POSTGRES_DB: simmer
      POSTGRES_USER: simmer
      POSTGRES_PASSWORD: simmer
    volumes:
      - simmer-db:/var/lib/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -q -U simmer -d simmer"]
      interval: 2s
      retries: 20

volumes:
  simmer-db:
```

Your applications then send to `simmer:25` on the same network.

---

## Tags

- `:latest` — the most recent release, Postgres.
- `:vX.Y.Z` — a specific release, Postgres (e.g. `:v0.2.0`).
- `:latest-mssql` — the most recent release, SQL Server.
- `:vX.Y.Z-mssql` — a specific release, SQL Server.

Both images are multi-arch (**linux/amd64**, **linux/arm64**). The SQL Server
image was amd64 only in releases up to and including 0.7.1. Its arm64 conformance
gate runs against Azure SQL Edge rather than SQL Server, because Microsoft
publishes no arm64 SQL Server container — see `DECISIONS.md` D-096 for what that
does and does not prove.

```sh
docker pull hikarisystems/simmer:latest
docker pull hikarisystems/simmer:latest-mssql
```

---

## Links

- **GitHub repository:** <https://github.com/Hikari-Systems/simmer>
- **Releases & changelog:** <https://github.com/Hikari-Systems/simmer/releases>
- **Full README, specification and decision log:**
  <https://github.com/Hikari-Systems/simmer#readme>

This page is generated from
[`DOCKERHUB.md`](https://github.com/Hikari-Systems/simmer/blob/main/DOCKERHUB.md)
in the repository and synced on each push to `main`.
