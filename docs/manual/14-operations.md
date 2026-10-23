# Operations

## The Docker image

| | |
|---|---|
| Images | `hikarisystems/simmer:vX.Y.Z` and `:latest` (PostgreSQL); `:vX.Y.Z-mssql` and `:latest-mssql` (SQL Server). Also on GHCR as `ghcr.io/hikari-systems/simmer` |
| Platforms | `linux/amd64`, `linux/arm64` |
| Base | `debian:bookworm-slim` with CA certificates and nothing else |
| User | UID 1000, not root |
| Entry point | `/app/server` |
| Ports | none published by default |
| Health check | `/app/server healthcheck`, which checks the admin port answers but not the database, so a database outage does not cause a restart loop |

Subcommands, given as the first argument:

| | |
|---|---|
| *(none)* | Run Simmer |
| `hash-password` | Read a password on standard input and print its argon2id hash |
| `healthcheck` | Exit 0 if the admin port answers |
| `replay` | Send captured mail again ([Capture and replay](13-capture.md)). Delivers mail |

The image contains `/app/simmer.yaml`, which is the repository's development
configuration and not usable as it is. Mount your own and set `SIMMER_CONFIG`.

## Running it with Docker Compose

```yaml
services:
  simmer:
    image: hikarisystems/simmer:latest
    read_only: true
    tmpfs:
      - /tmp:rw,noexec,nosuid,size=512m
    # cap_add: [NET_BIND_SERVICE]          # only for ports below 1024
    volumes:
      - ./simmer.yaml:/etc/simmer/simmer.yaml:ro
      # - ./tls:/etc/simmer/tls:ro         # for TLS listeners
    env_file: simmer.env
    environment:
      SIMMER_CONFIG: /etc/simmer/simmer.yaml
      DATABASE_URL: postgres://simmer:${DB_PASSWORD}@simmer-db:5432/simmer
    ports:
      - "127.0.0.1:8080:8080"               # admin API, host loopback only
    depends_on:
      simmer-db:
        condition: service_healthy

  simmer-db:
    image: postgres:18
    environment:
      POSTGRES_DB: simmer
      POSTGRES_USER: simmer
      POSTGRES_PASSWORD: ${DB_PASSWORD}
    volumes:
      - simmer-db:/var/lib/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -q -U simmer -d simmer"]
      interval: 2s
      retries: 20

volumes:
  simmer-db:
```

Applications on the same network send to `simmer:2525` (or whichever port the
configuration listens on). `simmer.env` holds the variables from
[Getting started](01-getting-started.md#the-settings).

## Where to run it

- **On an internal network**, close to the applications. Simmer is not a public
  mail server, and `allowed_cidrs` should list only your applications' networks.
- **Close to the providers, too.** Every message waits for the provider's reply,
  so latency to the provider is latency for the application.
- **Sizing.** Simmer itself is small. Memory is mostly messages in flight: up to
  1 MiB each in memory, the rest in `/tmp`, so the `/tmp` tmpfs should hold
  `max_concurrent_sessions` times your largest messages beyond 1 MiB, within
  reason. Each password check briefly uses about 19 MiB.
- **Run one instance.** The counters are safe with several instances on one
  database: a reservation is checked and taken in a single locked transaction,
  so caps are never overshot. But two instances with different configurations
  disagree about schedules, and the recipient frequency limit can be exceeded
  slightly when several instances send to one recipient at the same moment. A
  rolling deploy that briefly runs two is fine; a standing pair is not
  recommended. [`docs/MULTI_INSTANCE.md`](../MULTI_INSTANCE.md) has the details.

## Before production

- [ ] The new domain's DNS is published and the provider's domain authentication
      is complete. Turn on [DNS preflight](07-routes.md#dns-preflight) to keep
      checking it.
- [ ] The dry run gives the routes and rewritten headers you expect, for each
      sender and for a recipient at each provider.
- [ ] Applications send one recipient per message, and retry on `451`.
- [ ] Each route's provider timeouts add up to less than the applications' SMTP
      timeout ([Routes](07-routes.md#timeouts)).
- [ ] TLS on the listeners, or a network on which plaintext passwords are
      acceptable.
- [ ] `database.fail_closed` is `true`.
- [ ] Something scrapes `/metrics` or receives the OpenTelemetry export, with the
      alerts in [Logs, metrics and OpenTelemetry](11-observability.md#worth-an-alert).
- [ ] Named admin tokens for each person or system, so the audit log says who
      changed what.

## Shutdown

On `SIGTERM` (what `docker stop` sends) or Ctrl-C, Simmer:

1. stops accepting connections, on every listener and the link proxy;
2. lets the messages being relayed finish, for up to 30 seconds;
3. answers anything still running after that with `421` and closes it, releasing
   its reservations;
4. closes its connections to providers and exits.

`docker stop` waits 10 seconds by default before killing the process. Give it
longer, such as `docker stop -t 40` or `stop_grace_period: 40s` in Compose, so
messages in progress are not cut off.

## Upgrading

Read the release notes for each version you are skipping. Start the new version
against the same database: migrations are applied automatically at startup, and
anything in the configuration that a new version no longer accepts is refused
with a message saying what replaced it.

Things that have caught people out:

- **Before 0.7.0, `/metrics` was always on.** Now it is off unless
  `admin.metrics` is set. A configuration that does not mention it still loads,
  without `/metrics`, and Simmer warns at startup. Set `true` to keep it, or
  `false` to silence the warning.
- **0.9.0 introduced ramps.** Everything that used to be at the top level
  (`domain_groups`, `senders`, `default_chain`, `strict_senders`,
  `thread_affinity`, `exhausted_chain_reply`, `routes`) moved under
  `ramps.<name>`, with `default_ramp` naming it. An old configuration is refused,
  naming each key that moved. Existing counters and pauses are adopted into
  `default_ramp` at the first start. This upgrade needs every 0.8 instance
  stopped first, there is no downgrade, so take a database backup. Metrics
  labelled `route` gained a `ramp` label: add it to `sum by (...)` clauses.
- **`server.listen` and `server.auth.required` were replaced** by
  `server.listeners` when inbound TLS arrived, and `allow_insecure_auth` changed
  meaning: it now defaults to `false` and does what its name says. A
  configuration still using the old keys is refused with a message naming their
  replacement.

## Removing Simmer at the end

1. Check that the warming route's cap is well above your real volume, so it is
   carrying everything and the overflow route carries nothing.
2. Configure the application to send directly to the warming route's provider,
   as the new domain, with the same headers the route was setting (the dry run's
   `outbound` shows exactly what they are).
3. Move the application's SMTP settings from Simmer to the provider.
4. Point any link proxy names at their real destinations.
5. Stop Simmer. Its database can be deleted; it holds only counters.

Because Simmer's rewrites are written so the application can reproduce them, the
order of steps 2 and 3 does not matter: mail that is already in the new form
passes through Simmer unchanged.
