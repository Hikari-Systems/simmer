# simmer

An SMTP relay that warms up a new sending domain for you.

<!-- current-version: source of truth for the release number. Keep in sync with the git tag, DOCKERHUB.md, and Cargo.toml; enforced by .githooks/pre-push and the release CI. -->
**Current version: `v0.13.1`** ([all releases](https://github.com/Hikari-Systems/simmer/releases))

Your application sends mail to Simmer instead of straight to its SMTP provider.
For each message, Simmer checks whether the new domain still has allowance today
for that recipient's mailbox provider. If it does, the message is rewritten to
the new domain and sent through the new domain's provider. If not, it goes out
under your established domain as before. The allowance grows each day on a
schedule you set. When the new domain is carrying everything, you point the
application at the provider directly and remove Simmer.

Simmer is meant to be temporary, and it is built so that removing it changes
nothing about the mail: everything it does to a message is something the
application could do itself with a configuration change.

**[Read the manual](docs/manual/README.md)** for the full guide.

## Features

- **Daily caps per mailbox provider.** Google, Microsoft, Yahoo and everyone
  else each get their own allowance, because they each judge you separately.
  Company domains hosted at Google or Microsoft can be grouped by their mail
  servers. [Schedules](docs/manual/08-warmup.md),
  [domain groups](docs/manual/06-routing.md#domain-groups)
- **Nothing is dropped.** A message the warming route can't take goes to the next
  route in its chain, usually your established domain.
  [Routing](docs/manual/06-routing.md)
- **Identity rewriting.** The envelope sender, `From:`, `Message-ID:`, any other
  header, and links in the body, with a check at startup that every rewrite is
  safe to apply twice. [Routes](docs/manual/07-routes.md#the-identity-rewrite)
- **Exact counting.** Caps are never overshot, even with many messages in flight
  or several instances sharing a database.
  [The warm-up schedule](docs/manual/08-warmup.md#how-days-are-counted)
- **Safe failures.** When no route can take a message, the application gets a
  temporary `451` and retries later, never a permanent `550` that would get a
  good address suppressed.
  [How Simmer works](docs/manual/02-concepts.md#never-a-permanent-failure-for-a-temporary-reason)
- **Spreading the load.** Optionally offer a warming route only part of its
  traffic, so its allowance is spread across the day instead of being used up
  in the first hour. It can tune itself.
  [Spreading a route across the day](docs/manual/08-warmup.md#spreading-a-route-across-the-day)
- **DNS preflight.** Checks each route's SPF, DKIM and DMARC records, so a
  provider that signs as itself instead of as your domain is caught early.
  [DNS preflight](docs/manual/07-routes.md#dns-preflight)
- **Thread affinity.** Replies stay on the route, and the sender, that started
  the conversation. [Thread affinity](docs/manual/09-thread-affinity.md)
- **Secure intake.** Ports 25, 587 and 465 with TLS, logins, and a list of
  sender identities each login may use.
  [Listeners, TLS and SMTP users](docs/manual/04-server.md)
- **More than one warm-up at a time**, each with its own routes and schedule,
  chosen by port or by header. [Ramps](docs/manual/06-routing.md#ramps)
- **A different provider per mailbox group**, for example a dedicated IP pool for
  Gmail. [Per-group providers](docs/manual/07-routes.md#a-different-provider-per-domain-group)
- **Admin API.** Live state for every route, pause and resume, one-day allowance
  changes, and a dry run that shows what a message would do without sending it.
  [The admin API](docs/manual/12-admin-api.md)
- **Monitoring.** Prometheus metrics and an optional OpenTelemetry export of
  traces, metrics and logs, with opt-in recipient details on traces.
  [Logs, metrics and OpenTelemetry](docs/manual/11-observability.md)
- **Link proxy.** An optional HTTP forwarder for tracking and unsubscribe links
  moved to the new domain. [The link proxy](docs/manual/10-link-proxy.md)
- **PostgreSQL or SQL Server** for its counters. [The database](docs/manual/05-database.md)
- **Capture and replay** of traffic, for debugging.
  [Capture and replay](docs/manual/13-capture.md)

## What it isn't

Simmer is not a mail server. It has no queue and never stores a message: the
application's connection stays open while Simmer talks to the provider, and the
application gets the provider's answer. It doesn't sign mail either. Your
provider does that, as it would without Simmer. And it isn't meant to face the
internet. Run it on an internal network, next to your applications.

Each message must have exactly one recipient. A second `RCPT TO` is refused
with `452`, so applications that batch recipients need to send one message per
recipient. [Here's why.](docs/manual/02-concepts.md#one-recipient-per-message)

## Quick start

You need a PostgreSQL database and two SMTP provider accounts: one that sends as
your new domain and one that sends as your current domain. They can be the same
provider. The [starter configuration](docs/manual/examples/starter.yaml) takes
every setting from an environment variable, so you don't need to edit it.

### With Docker

```sh
# 1. Hash the password your application will log in with.
printf '%s' 'app-password' | docker run -i --rm hikarisystems/simmer:latest hash-password

# 2. Get the starter configuration.
curl -fsSLo simmer.yaml \
  https://raw.githubusercontent.com/Hikari-Systems/simmer/main/docs/manual/examples/starter.yaml
chmod 644 simmer.yaml

# 3. Run it.
docker run -d --name simmer \
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

Keep the single quotes around the password hash, because it contains `$`. With
this many variables, an env file (`--env-file simmer.env`) is tidier. For SQL
Server, use the `hikarisystems/simmer:latest-mssql` image.

### From the command line

There are no prebuilt binaries, so build one with a recent stable Rust:

```sh
git clone https://github.com/Hikari-Systems/simmer.git && cd simmer
cargo build --release --bin server

printf '%s' 'app-password' | ./target/release/server hash-password

export SIMMER_CONFIG=docs/manual/examples/starter.yaml
export DATABASE_URL=postgres://simmer:password@localhost:5432/simmer
# ...and the other SIMMER_* variables from the Docker example
./target/release/server
```

The binary has the database migrations built in, so you can copy it anywhere.

### Then

Point your application's SMTP settings at Simmer on port 2525 with the username
and password from step 1. [Getting started](docs/manual/01-getting-started.md)
covers each variable, how to try a message with the dry run before sending real
mail, and what to do next.

## Documentation

- [The manual](docs/manual/README.md): getting started, concepts, every
  configuration option, operations.
- [Configuration reference](docs/manual/reference.md): every key on one page.
- [`docs/SPEC.md`](docs/SPEC.md): the specification.
- [`DECISIONS.md`](DECISIONS.md): why things are the way they are.
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md): building, testing and releasing
  Simmer.

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE) for the
full text and [`NOTICE`](NOTICE) for attribution. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this work,
as defined in the Apache 2.0 license, shall be licensed as above, without any
additional terms or conditions.

SPDX-License-Identifier: Apache-2.0
