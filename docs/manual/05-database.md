# The database

Simmer keeps its counters in a database: how much each route has sent today to
each domain group, which routes are paused or graduated, any allowance
overrides, and the recipient-frequency records. It never stores messages.

```yaml
database:
  url: "${DATABASE_URL}"
  max_connections: 10      # default
  connect_timeout: 5s      # required
  fail_closed: true        # default
```

| Key | Default | Meaning |
|---|---|---|
| `url` | required | The connection: a PostgreSQL URL, or a SQL Server connection string in the `-mssql` build |
| `max_connections` | `10` | The most connections Simmer opens to the database |
| `connect_timeout` | required | How long to wait for a connection |
| `fail_closed` | `true` | What to do when the database cannot be reached; leave it `true` (below) |

Simmer creates its tables on first start and applies any new migrations when a
new version starts, so the login needs permission to create tables. Several
instances starting at once apply them one at a time.

## Two builds

The database is chosen when Simmer is built, not in the configuration. Each
release is published as two Docker images:

| Image | Database |
|---|---|
| `hikarisystems/simmer:vX.Y.Z`, `:latest` | PostgreSQL |
| `hikarisystems/simmer:vX.Y.Z-mssql`, `:latest-mssql` | SQL Server 2017 or later, or Azure SQL |

Both are for `linux/amd64` and `linux/arm64`. Everything else, the configuration
file included, is identical. Given the other build's kind of `url`, each refuses
to start and names the image to use. Building from source, the default is
PostgreSQL, and `--no-default-features --features mssql` builds for SQL Server.

## PostgreSQL

`url` is a standard PostgreSQL URL:

```yaml
url: "postgres://simmer:${SIMMER_DB_PASSWORD}@db.internal:5432/simmer"
url: "postgres://simmer:${SIMMER_DB_PASSWORD}@db.internal:5432/simmer?sslmode=require"
```

Tested against PostgreSQL 18. Simmer needs its own database, or at least a
schema with nothing else in it.

## SQL Server

`url` is an ADO.NET or JDBC connection string:

```yaml
url: "server=tcp:sql.internal,1433;database=simmer;user id=simmer;password=${SIMMER_DB_PASSWORD}"
```

- **The connection is encrypted by default**, whether or not the string says
  `encrypt=true`. Only an explicit `encrypt=false` turns it off, and Simmer warns
  at startup that queries then cross the network in clear.
- **A server with a self-signed certificate** needs `TrustServerCertificate=true`,
  which also produces a startup warning.
- **SQL Server logins only.** Windows and Kerberos authentication are not
  supported.
- **Route and domain group names are limited to 128 characters** in this build,
  because SQL Server limits the size of an index key.
- **Names are case-sensitive**, as they are with PostgreSQL: the tables use a
  binary collation, so `Warming` and `warming` are different routes.

## When the database is unreachable

Simmer cannot enforce a cap without its counters, so with `fail_closed: true`, a
message that arrives while the database is unreachable is answered
`451 4.3.0 quota service unavailable` and nothing is sent. The application
retries later, as it would for any temporary failure.

`fail_closed: false` does **not** send without counting. A database failure is
then answered as if no route were eligible: `exhausted_chain_reply`, which is
`451 4.7.1` by default. There is no mode that sends unenforced.

`/health` answers `503` while the database is unreachable, so a load balancer can
take Simmer out of rotation. The container's built-in health check does **not**
look at the database, so an outage does not make Docker restart the container
in a loop.

Watch `simmer_quota_unavailable_total`, and `simmer_db_pool_connections{state="in_use"}`
against `simmer_db_pool_max`: a pool that stays full comes before `451 4.3.0`.
