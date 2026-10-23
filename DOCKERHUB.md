# Simmer

An SMTP relay that warms up a new sending domain for you.

<!-- current-version: source of truth for the release number. Keep in sync with the git tag, README.md, and Cargo.toml; enforced by .githooks/pre-push and the release CI. -->
**Current version: `v0.13.1`** ([all releases](https://github.com/Hikari-Systems/simmer/releases))

Your application sends mail to Simmer instead of straight to its SMTP provider.
For each message, Simmer checks whether the new domain still has allowance today
for that recipient's mailbox provider. If it does, the message is rewritten to
the new domain and sent through the new domain's provider. If not, it goes out
under your established domain as before. The allowance grows each day on a
schedule you set, and when the new domain is carrying everything, you remove
Simmer.

📖 **[The manual](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/README.md)**
covers everything on this page in more detail, and every configuration option.

## Features

- **Daily caps per mailbox provider**, with company domains grouped by their mail
  servers. [Schedules](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/08-warmup.md)
- **Nothing is dropped.** What the warming route can't take goes out under your
  established domain. [Routing](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/06-routing.md)
- **Identity rewriting** of the envelope, headers and body links, checked at
  startup to be safe to apply twice. [Routes](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/07-routes.md)
- **Exact counting**, even across instances, and only temporary `451` failures
  when no route can take a message.
- **Spreading a route's traffic across the day**, optionally self-tuning.
  [Partial ramps](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/08-warmup.md#spreading-a-route-across-the-day)
- **DNS preflight** of each route's SPF, DKIM and DMARC.
  [Preflight](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/07-routes.md#dns-preflight)
- **Thread affinity**, so replies keep their sender.
  [Thread affinity](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/09-thread-affinity.md)
- **TLS on ports 25, 587 and 465**, logins, and per-login sender permissions.
  [Listeners](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/04-server.md)
- **Several warm-ups in one process**, and a different provider per mailbox group.
  [Ramps](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/06-routing.md#ramps)
- **An admin API** with live state, pause and resume, one-day allowance changes
  and a dry run. [Admin API](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/12-admin-api.md)
- **Prometheus metrics and OpenTelemetry export.**
  [Monitoring](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/11-observability.md)
- **An optional link proxy** for rewritten tracking links.
  [Link proxy](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/10-link-proxy.md)

Simmer is not a mail server: it has no queue, stores no messages and signs
nothing. It belongs on an internal network next to your applications, and each
message it relays has exactly one recipient.

## Tags

| Tag | Database |
|---|---|
| `:latest`, `:vX.Y.Z` | PostgreSQL |
| `:latest-mssql`, `:vX.Y.Z-mssql` | SQL Server 2017 or later, or Azure SQL |

Both are multi-arch (`linux/amd64`, `linux/arm64`), run as UID 1000, and publish
no ports by default.

## Quick start

You need a PostgreSQL database and two SMTP provider accounts: one that sends as
your new domain and one that sends as your current domain. The
[starter configuration](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/examples/starter.yaml)
takes every setting from an environment variable.

```sh
# 1. Hash the password your application will log in with.
printf '%s' 'app-password' | docker run -i --rm hikarisystems/simmer:latest hash-password

# 2. Get the starter configuration.
curl -fsSLo simmer.yaml \
  https://raw.githubusercontent.com/Hikari-Systems/simmer/main/docs/manual/examples/starter.yaml
chmod 644 simmer.yaml

# 3. Run it.
docker run -d --name simmer \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,size=512m \
  -v "$PWD/simmer.yaml:/etc/simmer/simmer.yaml:ro" \
  -e SIMMER_CONFIG=/etc/simmer/simmer.yaml \
  -e DATABASE_URL=postgres://simmer:password@db.internal:5432/simmer \
  -e SIMMER_HOSTNAME=simmer.internal \
  -e SIMMER_ADMIN_TOKEN=a-long-random-string \
  -e SIMMER_SMTP_USER=app \
  -e SIMMER_SMTP_PASSWORD_HASH='$argon2id$v=19$m=19456,t=2,p=1$...' \
  -e SIMMER_CURRENT_DOMAIN=oldbrand.com \
  -e SIMMER_NEW_DOMAIN=newbrand.com \
  -e SIMMER_WARMUP_STARTED=2026-11-02T09:00:00Z \
  -e SIMMER_WARM_SMTP_HOST=smtp.newprovider.com \
  -e SIMMER_WARM_SMTP_USER=... -e SIMMER_WARM_SMTP_PASSWORD=... \
  -e SIMMER_OVERFLOW_SMTP_HOST=smtp.oldprovider.com \
  -e SIMMER_OVERFLOW_SMTP_USER=... -e SIMMER_OVERFLOW_SMTP_PASSWORD=... \
  -p 127.0.0.1:2525:2525 -p 127.0.0.1:8080:8080 \
  hikarisystems/simmer:latest

# 4. Check it.
curl -s localhost:8080/health
```

| Variable | What it is |
|---|---|
| `DATABASE_URL` | The PostgreSQL URL (or SQL Server connection string, with `-mssql`) |
| `SIMMER_HOSTNAME` | Simmer's name in its SMTP greeting |
| `SIMMER_ADMIN_TOKEN` | Bearer token for the admin API on port 8080 |
| `SIMMER_SMTP_USER`, `SIMMER_SMTP_PASSWORD_HASH` | The login your application uses, and the hash from step 1 |
| `SIMMER_CURRENT_DOMAIN`, `SIMMER_NEW_DOMAIN` | The domain you send as today, and the one you are warming |
| `SIMMER_WARMUP_STARTED` | When day 0 begins (RFC 3339) |
| `SIMMER_WARM_SMTP_HOST`, `_USER`, `_PASSWORD` | The provider for the new domain (port 587, TLS) |
| `SIMMER_OVERFLOW_SMTP_HOST`, `_USER`, `_PASSWORD` | The provider for the current domain |

Then point your application at Simmer on port 2525 with that login.
[Getting started](https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/01-getting-started.md)
explains each step and how to try a message with the dry run first.

A few notes on running it:

- Keep the single quotes around the password hash, because it contains `$`. An
  env file (`--env-file simmer.env`) is easier with this many variables.
- `/tmp` holds message bodies over 1 MiB while they're being relayed. A tmpfs
  means nothing survives a crash, which is what you want.
- To listen on ports below 1024, such as 25 or 587, add
  `--cap-add NET_BIND_SERVICE`.
- Don't publish the SMTP port on an interface the internet can reach.

## Subcommands

| First argument | |
|---|---|
| *(none)* | Run Simmer |
| `hash-password` | Read a password on standard input and print its hash |
| `healthcheck` | Exit 0 if the admin port answers. The image's `HEALTHCHECK` uses it |
| `replay` | Re-send captured mail to another Simmer, for debugging. This delivers mail |

## Links

- **Manual:** <https://github.com/Hikari-Systems/simmer/blob/main/docs/manual/README.md>
- **Source and issues:** <https://github.com/Hikari-Systems/simmer>
- **Releases:** <https://github.com/Hikari-Systems/simmer/releases>
- **License:** Apache-2.0

This page is generated from
[`DOCKERHUB.md`](https://github.com/Hikari-Systems/simmer/blob/main/DOCKERHUB.md)
and updated on each push to `main`.
