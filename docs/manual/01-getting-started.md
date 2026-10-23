# Getting started

This chapter runs Simmer with the [starter configuration](examples/starter.yaml)
and sends a message through it. You can run it with Docker or as a plain
command-line program; the configuration is the same either way.

The starter configuration has:

- one **warming route**, which sends as your new domain through one SMTP
  provider, up to a daily cap that grows from 50 messages on day 0 to 12,000 on
  day 9;
- one **overflow route**, which sends everything the warming route cannot take
  under your current, established domain through another provider;
- one SMTP listener on port 2525 that requires a username and password;
- the admin API on port 8080.

[How Simmer works](02-concepts.md) explains each of those pieces.

## What you need

- **A database.** PostgreSQL (tested against 18) for the standard build, or SQL
  Server 2017 or later / Azure SQL for the `-mssql` build. Simmer creates its
  own tables on first start, so the login must be allowed to create them.
- **Two SMTP provider accounts**, with a hostname, username and password each,
  accepting submissions on port 587 with TLS:
  - one that sends as your **new** domain. Finish the provider's domain
    authentication first (its DKIM key and SPF include published in your DNS).
    Until then, the provider signs as itself and the domain warms nothing.
  - one that already sends as your **current** domain, for the overflow.

  They can be the same provider.
- **Docker**, or a Rust toolchain to build from source.

## The settings

Every site-specific value in the starter configuration is an environment
variable, so the file itself needs no editing. All of them must be set.

| Variable | Example | What it is |
|---|---|---|
| `DATABASE_URL` | `postgres://simmer:pw@db:5432/simmer` | The database connection |
| `SIMMER_HOSTNAME` | `simmer.internal` | The name Simmer gives itself in its SMTP greeting and `Received:` headers |
| `SIMMER_ADMIN_TOKEN` | 32 random characters | The bearer token for the admin API |
| `SIMMER_SMTP_USER` | `app` | The username your application logs in with |
| `SIMMER_SMTP_PASSWORD_HASH` | `$argon2id$v=19$…` | That user's password, hashed (see below) |
| `SIMMER_CURRENT_DOMAIN` | `oldbrand.com` | The domain your application sends as today |
| `SIMMER_NEW_DOMAIN` | `newbrand.com` | The domain you are warming up |
| `SIMMER_WARMUP_STARTED` | `2026-11-02T09:00:00Z` | When day 0 of the ramp begins, as an RFC 3339 instant |
| `SIMMER_WARM_SMTP_HOST`, `_USER`, `_PASSWORD` | `smtp.newprovider.com` | The provider that sends as the new domain |
| `SIMMER_OVERFLOW_SMTP_HOST`, `_USER`, `_PASSWORD` | `smtp.oldprovider.com` | The provider that sends as the current domain |

The application can send as either domain. A message from `jane@oldbrand.com`
that takes the warming route leaves as `jane@newbrand.com`; one that takes the
overflow route leaves as `jane@oldbrand.com`. The local part (`jane`) and the
display name are kept.

Generate the admin token with anything that produces random text, for example
`openssl rand -hex 24`.

## Option 1: Docker

### 1. Hash a password for your application

The password is read from standard input, never from the command line, where it
would be saved in your shell history:

```sh
printf '%s' 'the-password-your-app-will-use' \
  | docker run -i --rm hikarisystems/simmer:latest hash-password
# $argon2id$v=19$m=19456,t=2,p=1$....
```

The output is the value for `SIMMER_SMTP_PASSWORD_HASH`.

### 2. Start a database

Skip this if you already have one Simmer can reach.

```sh
docker network create simmer
docker run -d --name simmer-db --network simmer \
  -e POSTGRES_DB=simmer -e POSTGRES_USER=simmer -e POSTGRES_PASSWORD=change-me \
  -v simmer-db:/var/lib/postgresql \
  postgres:18
```

### 3. Fetch the starter configuration

```sh
curl -fsSLo simmer.yaml \
  https://raw.githubusercontent.com/Hikari-Systems/simmer/main/docs/manual/examples/starter.yaml
chmod 644 simmer.yaml     # the container runs as UID 1000 and must be able to read it
```

### 4. Run Simmer

```sh
docker run -d --name simmer --network simmer \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,size=512m \
  -v "$PWD/simmer.yaml:/etc/simmer/simmer.yaml:ro" \
  -e SIMMER_CONFIG=/etc/simmer/simmer.yaml \
  -e DATABASE_URL=postgres://simmer:change-me@simmer-db:5432/simmer \
  -e SIMMER_HOSTNAME=simmer.internal \
  -e SIMMER_ADMIN_TOKEN=replace-with-a-random-token \
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
```

- Put the password hash in **single quotes**. It contains `$`, which the shell
  would otherwise expand.
- With this many variables, an env file is easier: put one `NAME=value` per line
  in `simmer.env` (no quotes needed there) and replace the `-e` lines with
  `--env-file simmer.env`. Keep that file out of version control.
- `--read-only` and the `/tmp` tmpfs are recommended, not required. Message
  bodies above 1 MiB are held in `/tmp` while they are being relayed, and a
  tmpfs guarantees none of them survive a crash.
- The `-p` flags publish the ports on the host's loopback interface only, so
  you can test from the host. Applications on the `simmer` network reach it as
  `simmer:2525` without them. Never publish the SMTP port on an interface the
  internet can reach: Simmer is built to sit on a trusted internal network.
- The `-mssql` image (`hikarisystems/simmer:latest-mssql`) works the same way,
  with a SQL Server connection string in `DATABASE_URL`; see
  [The database](05-database.md).

If startup fails, `docker logs simmer` lists every problem with the
configuration at once, for example an unset variable:

```
1 unresolved ${ENV_VAR} reference in config:
  - $.ramps.main.routes[0].downstream.host references ${SIMMER_WARM_SMTP_HOST}, which is not set
```

## Option 2: the command line

No binaries are published, so build one. You need a recent stable Rust
toolchain (from <https://rustup.rs>).

```sh
git clone https://github.com/Hikari-Systems/simmer.git
cd simmer
cargo build --release --bin server
# for SQL Server instead:
#   cargo build --release --bin server --no-default-features --features mssql
```

The program is `target/release/server`. It is self-contained (the database
migrations are compiled in), so you can copy it anywhere.

```sh
printf '%s' 'the-password-your-app-will-use' | ./target/release/server hash-password

export SIMMER_CONFIG=docs/manual/examples/starter.yaml
export DATABASE_URL=postgres://simmer:change-me@localhost:5432/simmer
export SIMMER_HOSTNAME=simmer.internal
export SIMMER_ADMIN_TOKEN=replace-with-a-random-token
export SIMMER_SMTP_USER=app
export SIMMER_SMTP_PASSWORD_HASH='$argon2id$v=19$m=19456,t=2,p=1$...'
export SIMMER_CURRENT_DOMAIN=oldbrand.com SIMMER_NEW_DOMAIN=newbrand.com
export SIMMER_WARMUP_STARTED=2026-11-02T09:00:00Z
export SIMMER_WARM_SMTP_HOST=smtp.newprovider.com SIMMER_WARM_SMTP_USER=... SIMMER_WARM_SMTP_PASSWORD=...
export SIMMER_OVERFLOW_SMTP_HOST=smtp.oldprovider.com SIMMER_OVERFLOW_SMTP_USER=... SIMMER_OVERFLOW_SMTP_PASSWORD=...

./target/release/server
```

- `SIMMER_CONFIG` is the configuration file's path. Without it, Simmer reads
  `simmer.yaml` in the current directory.
- Large message bodies are spilled to the system temporary directory (`TMPDIR`,
  else `/tmp`) while they are being relayed.
- It logs JSON to standard output. Add `logging: { format: text }` to the
  configuration for something easier to read in a terminal.
- `SIGTERM` or Ctrl-C shuts it down cleanly, letting messages in progress
  finish (see [Operations](14-operations.md#shutdown)).

## Check that it is running

```sh
curl -s localhost:8080/health
```

`/health` answers `200` when Simmer is up and can reach its database, and `503`
when it cannot reach the database. Everything else on the admin port needs the
token:

```sh
TOKEN=replace-with-a-random-token
curl -s -H "Authorization: Bearer $TOKEN" localhost:8080/ramps/main/routes | jq
```

That shows both routes, the warm-up day the warming route is on (`day_index`),
and each domain group's `allowance` and `committed` count for today. A route with
`"status": "not_started"` and a negative `day_index` has a
`SIMMER_WARMUP_STARTED` still in the future: until then the warming route has no
allowance, and everything takes the overflow route.

## Ask what a message would do

The dry run runs a message through the real routing and rewriting without
sending anything or using any allowance:

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"envelope_from":"jane@oldbrand.com","from_header":"Jane <jane@oldbrand.com>",
       "recipients":["someone@gmail.com"]}' \
  localhost:8080/dryrun | jq
```

The answer names the domain group the recipient is in, every route considered
and why it was or was not chosen, and the rewritten envelope and headers. See
[The admin API](12-admin-api.md#the-dry-run).

## Send a message

This **sends real mail**, through whichever provider the route picks. Here it is
with [swaks](https://www.jetmore.org/john/code/swaks/):

```sh
swaks --server localhost:2525 \
  --auth-user app --auth-password 'the-password-your-app-will-use' \
  --from jane@oldbrand.com --to you@gmail.com \
  --header 'Subject: simmer test'
```

A `250` at the end means the provider accepted it. Run the `/ramps/main/routes`
request again and the warming route's `committed` count for the `google` group
has gone up by one.

The starter listener has no TLS, so swaks sends the password in clear. That is
acceptable on a trusted internal network and nowhere else;
[Listeners, TLS and SMTP users](04-server.md) adds TLS.

## Point your application at it

Configure the application's SMTP settings as:

| Setting | Value |
|---|---|
| Host and port | Simmer's address, port 2525 |
| Username and password | `SIMMER_SMTP_USER` and the password you hashed |
| Encryption | none, with the starter configuration |
| From address | an address at either `SIMMER_CURRENT_DOMAIN` or `SIMMER_NEW_DOMAIN` |

Two things the application must cope with:

- **One recipient per message.** A second `RCPT TO` in the same SMTP
  transaction is refused with `452`. Send one message per recipient. Most
  libraries already do when the recipients are not on one `To:` line. See
  [How Simmer works](02-concepts.md#one-recipient-per-message).
- **Temporary failures.** When no route can take a message, Simmer answers
  `451`, and the application must try again later. With the starter
  configuration that happens only if the overflow provider itself is failing,
  because the overflow route is never capped.

## Next

- Read [How Simmer works](02-concepts.md) before changing the starter
  configuration.
- Then work through the chapters for what you need: TLS and more users
  ([04](04-server.md)), more providers or brands ([06](06-routing.md),
  [07](07-routes.md)), a different schedule ([08](08-warmup.md)), and
  monitoring ([11](11-observability.md)).
- Before production, read [Operations](14-operations.md).
