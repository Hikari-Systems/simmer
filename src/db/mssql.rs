//! SQL Server connection management and migrations — the `mssql` feature's
//! half of [`crate::db`] (D-084).
//!
//! The same shape as `postgres.rs`: a lazily connecting pool sized and timed by
//! §4.1's `database` block, and migrations applied at startup. What differs is
//! what sqlx would otherwise have done for us:
//!
//! - **The pool** is bb8 over tiberius, with the adapter below. A connection is
//!   marked broken for the whole of every storage call and cleared only when the
//!   call completes, so one that errored — or whose future was dropped mid-query,
//!   leaving an unread response or an open transaction on the wire — is discarded
//!   rather than handed to the next caller.
//! - **Every session** runs `SET XACT_ABORT ON`, so any error inside a transaction
//!   rolls the whole transaction back server-side, and `SET NOCOUNT ON`, so the
//!   only result sets a batch produces are the ones its final `SELECT` asks for.
//! - **Migrations** are the files in `migrations-mssql/`, compiled in, recorded in
//!   `simmer_migrations` with a SHA-256 checksum — sqlx's contract, reproduced:
//!   an applied migration whose file has since changed refuses to start.

use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tiberius::{Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::Database;
use crate::db::{PoolGauge, PoolStats};

pub type Pool = bb8::Pool<Manager>;

/// The width of every text key column in `migrations-mssql/`. §4.2 refuses a
/// route or domain group name longer than this under this build.
pub const MAX_NAME_CHARS: usize = 128;
pub type PooledConn<'a> = bb8::PooledConnection<'a, Manager>;

/// One pooled session.
pub struct Conn {
    pub client: Client<Compat<TcpStream>>,
    /// Set for the duration of every use; see the module docs.
    pub broken: bool,
}

pub struct Manager {
    config: Config,
}

/// Parse `database.url` for this build.
///
/// An ADO.NET connection string (`server=tcp:host,1433;database=simmer;user
/// id=…;password=…;encrypt=true`) or a JDBC one (`jdbc:sqlserver://host:1433;…`),
/// which are the two forms SQL Server tooling hands out. A Postgres URL is
/// refused by name: it means the wrong image, and the ADO parser would otherwise
/// read it as a server called `localhost`.
pub fn parse_url(url: &str) -> anyhow::Result<Config> {
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        anyhow::bail!(
            "database.url is a Postgres URL, but this is the SQL Server build of simmer \
             (the `-mssql` image). Use the plain `hikarisystems/simmer:<version>` image for \
             Postgres, or give an ADO.NET or JDBC SQL Server connection string"
        );
    }
    // Deliberately never includes the string in the error: it carries the
    // password, and this error goes straight to a log.
    let config = if lower.starts_with("jdbc:") {
        Config::from_jdbc_string(url)
    } else {
        Config::from_ado_string(url)
    };
    let mut config = config.map_err(|e| {
        anyhow::anyhow!(
            "database.url is not a valid SQL Server connection string ({}). \
             Expected ADO.NET (`server=tcp:host,1433;database=…;user id=…;password=…`) \
             or JDBC (`jdbc:sqlserver://host:1433;databaseName=…;user=…;password=…`)",
            redact(&e)
        )
    })?;

    // tiberius, like SQL Server's older drivers, reads a string that says
    // nothing about `encrypt` as "encrypt the login only" — every query after it
    // travels in cleartext. Microsoft's current drivers default to encrypting
    // everything, and so does this build: only an explicit `encrypt=false` opts
    // out, and `build_pool` warns when it does.
    if !names_key(url, "encrypt") {
        config.encryption(EncryptionLevel::Required);
    }
    Ok(config)
}

/// Does the connection string set `key` at all? Both ADO.NET and JDBC strings
/// are `;`-separated `key=value` pairs (JDBC's after its URL prefix), and keys
/// are case-insensitive.
fn names_key(url: &str, key: &str) -> bool {
    url.split(';').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key))
    })
}

/// The value the connection string gives `key`, if any.
fn value_of<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split(';').find_map(|pair| {
        pair.split_once('=')
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            .map(|(_, v)| v.trim())
    })
}

/// tiberius' parse errors can quote the offending fragment, which may be the
/// password. Keep the kind, drop the detail.
fn redact(e: &tiberius::error::Error) -> &'static str {
    match e {
        tiberius::error::Error::Conversion(_) => "a value could not be parsed",
        _ => "malformed",
    }
}

impl Manager {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl bb8::ManageConnection for Manager {
    type Connection = Conn;
    type Error = tiberius::error::Error;

    async fn connect(&self) -> Result<Conn, Self::Error> {
        let mut config = self.config.clone();
        // Azure SQL's gateway answers the first login with a redirect to the
        // node that owns the database. Follow it once; a second redirect is an
        // error rather than a loop.
        let client = match open(&config).await {
            Err(tiberius::error::Error::Routing { host, port }) => {
                config.host(&host);
                config.port(port);
                open(&config).await?
            }
            other => other?,
        };
        let mut conn = Conn {
            client,
            broken: false,
        };
        conn.client
            .simple_query("SET XACT_ABORT ON; SET NOCOUNT ON;")
            .await?
            .into_results()
            .await?;
        Ok(conn)
    }

    async fn is_valid(&self, conn: &mut Conn) -> Result<(), Self::Error> {
        conn.client
            .simple_query("SELECT 1")
            .await?
            .into_row()
            .await?;
        Ok(())
    }

    fn has_broken(&self, conn: &mut Conn) -> bool {
        conn.broken
    }
}

async fn open(config: &Config) -> Result<Client<Compat<TcpStream>>, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(config.clone(), tcp.compat_write()).await
}

/// Build the pool. Does not connect eagerly — §7.5, exactly as the Postgres
/// build: an unreachable database is a `451` per message, not a refusal to start.
pub fn build_pool(cfg: &Database) -> anyhow::Result<Pool> {
    let mut config = parse_url(&cfg.url)?;
    config.application_name("simmer");

    // Neither is refused — a lab SQL Server with a self-signed certificate is a
    // real deployment — but neither should be silent.
    let on = |v: Option<&str>| {
        v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes"))
    };
    let off = |v: Option<&str>| {
        v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "false" | "no"))
    };
    if off(value_of(&cfg.url, "encrypt")) {
        tracing::warn!(
            "database.url sets encrypt=false: SQL Server encrypts the login only, and every \
             query and quota row after it crosses the network in cleartext"
        );
    }
    if on(value_of(&cfg.url, "trustservercertificate")) {
        tracing::warn!(
            "database.url sets TrustServerCertificate=true: the SQL Server's certificate is \
             not verified, so the connection is encrypted but not authenticated"
        );
    }
    Ok(bb8::Pool::builder()
        .max_size(cfg.max_connections)
        // §4.1's `connect_timeout` bounds acquiring a usable connection, as the
        // Postgres build maps it to sqlx's acquire timeout.
        .connection_timeout(cfg.connect_timeout)
        .idle_timeout(Some(Duration::from_secs(600)))
        .build_unchecked(Manager::new(config)))
}

/// Take a connection from the pool, as a storage error on failure.
pub async fn get(pool: &Pool) -> Result<PooledConn<'_>, crate::quota::QuotaError> {
    pool.get()
        .await
        .map_err(|e| crate::quota::QuotaError::Storage(format!("sql server pool: {e}")))
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// `migrations-mssql/`, compiled in, in version order. Each file is one batch —
/// no `GO` separators — and idempotent, like its Postgres counterpart.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        20260807000000,
        "baseline",
        include_str!("../../migrations-mssql/20260807000000_baseline.sql"),
    ),
    (
        20260807000001,
        "quota",
        include_str!("../../migrations-mssql/20260807000001_quota.sql"),
    ),
    (
        20260810000000,
        "recipient event",
        include_str!("../../migrations-mssql/20260810000000_recipient_event.sql"),
    ),
    (
        20260925000000,
        "ramp",
        include_str!("../../migrations-mssql/20260925000000_ramp.sql"),
    ),
    (
        20261003000000,
        "route rate",
        include_str!("../../migrations-mssql/20261003000000_route_rate.sql"),
    ),
];

/// Apply migrations. Runs at startup, per §11.
///
/// Serialised across instances by `sp_getapplock`, which is what sqlx's
/// advisory lock does for the Postgres build: two replicas starting together
/// must not both decide a migration is missing.
pub async fn migrate(pool: &Pool) -> anyhow::Result<()> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| anyhow::anyhow!("sql server pool: {e}"))
        .context("applying migrations")?;
    // Whatever happens below, this session held an application lock and may
    // hold an open transaction. It does not go back in the pool.
    conn.broken = true;

    let client = &mut conn.client;
    let lock = client
        .simple_query(
            "DECLARE @r INT; \
             EXEC @r = sp_getapplock @Resource = N'simmer_migrations', \
                  @LockMode = N'Exclusive', @LockOwner = N'Session', @LockTimeout = 60000; \
             SELECT @r AS r",
        )
        .await?
        .into_row()
        .await?
        .and_then(|r| r.get::<i32, _>("r"))
        .unwrap_or(-999);
    if lock < 0 {
        anyhow::bail!("could not take the migration lock (sp_getapplock returned {lock})");
    }

    // Inside the lock, and that is the point. `IF OBJECT_ID(...) IS NULL CREATE
    // TABLE` is not atomic: two replicas starting together both evaluate the
    // guard as true and both run the DDL, and the loser gets
    //
    //   There is already an object named 'simmer_migrations' in the database.
    //
    // which is a failed startup for a server that had nothing wrong with it.
    // Creating the table before taking the lock left the one piece of DDL the
    // lock exists to serialise outside it. `sp_getapplock` needs no table of its
    // own, so there was never a reason for the old order.
    client
        .simple_query(
            "IF OBJECT_ID(N'dbo.simmer_migrations', N'U') IS NULL \
             CREATE TABLE dbo.simmer_migrations ( \
                 version     BIGINT        NOT NULL PRIMARY KEY, \
                 description NVARCHAR(200) NOT NULL, \
                 checksum    VARBINARY(32) NOT NULL, \
                 applied_at  DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME())",
        )
        .await?
        .into_results()
        .await
        .context("creating simmer_migrations")?;

    for (version, description, sql) in MIGRATIONS {
        let checksum: Vec<u8> = Sha256::digest(sql.as_bytes()).to_vec();
        let applied = client
            .query(
                "SELECT checksum FROM dbo.simmer_migrations WHERE version = @P1",
                &[version],
            )
            .await?
            .into_row()
            .await?
            .and_then(|r| r.get::<&[u8], _>("checksum").map(<[u8]>::to_vec));

        match applied {
            Some(stored) if stored == checksum => continue,
            Some(_) => anyhow::bail!(
                "migration {version} ({description}) was applied from a different file than \
                 the one in this build; migrations-mssql/ files must never change once released"
            ),
            None => {}
        }

        // One transaction per migration: its DDL and its bookkeeping row land
        // together or not at all (XACT_ABORT is on for the session).
        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;
        client
            .simple_query(*sql)
            .await?
            .into_results()
            .await
            .with_context(|| format!("applying migration {version} ({description})"))?;
        client
            .execute(
                "INSERT INTO dbo.simmer_migrations (version, description, checksum) \
                 VALUES (@P1, @P2, @P3)",
                &[version, description, &checksum.as_slice()],
            )
            .await?;
        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;
    }

    client
        .simple_query(
            "EXEC sp_releaseapplock @Resource = N'simmer_migrations', @LockOwner = N'Session'",
        )
        .await?
        .into_results()
        .await?;

    // Everything completed cleanly: no open transaction, lock released.
    conn.broken = false;
    Ok(())
}

/// Is the database reachable? Used by `GET /health` (§9.2).
pub async fn is_reachable(pool: &Pool) -> bool {
    // `get` already runs bb8's `is_valid` probe, which is a `SELECT 1`.
    pool.get().await.is_ok()
}

// ---------------------------------------------------------------------------
// Gauges
// ---------------------------------------------------------------------------

/// `simmer_db_pool_*` (D-075). bb8 reports its size and idle count; the
/// ceiling is the configured one, which is what bb8 was built with.
pub struct Gauge {
    pool: Pool,
    max: u32,
}

impl Gauge {
    pub fn new(pool: Pool, max: u32) -> Self {
        Self { pool, max }
    }
}

impl PoolGauge for Gauge {
    fn stats(&self) -> PoolStats {
        let state = self.pool.state();
        let size = u64::from(state.connections);
        let idle = u64::from(state.idle_connections);
        PoolStats {
            in_use: size.saturating_sub(idle),
            idle,
            max: u64::from(self.max),
        }
    }
}
