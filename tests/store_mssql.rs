//! The §11 storage conformance suite against SQL Server (D-084). See
//! `tests/store_conformance/mod.rs`; this file is the harness plus the tests
//! only this backend needs — its own migration runner and connection strings.
//!
//! Needs a SQL Server and `MSSQL_URL`: an ADO.NET connection string for a login
//! that may `CREATE DATABASE`, with no `database=` of its own. Each test gets a
//! fresh database, the way `#[sqlx::test]` gives each Postgres test one, and
//! drops it afterwards:
//!
//! ```sh
//! docker compose --profile mssql up -d simmer-mssql-db
//! MSSQL_URL='server=tcp:127.0.0.1,1434;user id=sa;password=Simmer-dev-1!;TrustServerCertificate=true' \
//!   cargo test --no-default-features --features mssql --test store_mssql
//! ```

#![cfg(feature = "mssql")]

mod store_conformance;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use simmer::config::Database;
use simmer::db::mssql;
use simmer::quota::{MssqlQuotaStore, QuotaStore};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

fn server_url() -> String {
    std::env::var("MSSQL_URL").expect(
        "MSSQL_URL must name a SQL Server login that may CREATE DATABASE; \
         see the header of tests/store_mssql.rs",
    )
}

fn database(url: String) -> Database {
    Database {
        url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(15),
        fail_closed: true,
    }
}

/// One statement on a fresh connection to the server, outside any test database.
async fn admin(sql: &str) {
    let config = tiberius::Config::from_ado_string(&server_url()).expect("MSSQL_URL parses");
    let tcp = TcpStream::connect(config.get_addr())
        .await
        .expect("connect");
    tcp.set_nodelay(true).unwrap();
    let mut client = tiberius::Client::connect(config, tcp.compat_write())
        .await
        .expect("login");
    client
        .simple_query(sql)
        .await
        .expect("admin statement")
        .into_results()
        .await
        .expect("admin statement");
}

/// Create a database, migrate it, run `test` with a store factory over it, and
/// drop it — whether or not the test passed.
async fn with_database<F, Fut>(test: F)
where
    F: FnOnce(Database) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let name = format!("simmer_t_{}", uuid::Uuid::new_v4().simple());
    admin(&format!("CREATE DATABASE [{name}]")).await;

    let cfg = database(format!("{};database={name}", server_url()));
    let outcome = tokio::spawn(test(cfg)).await;

    admin(&format!(
        "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}]"
    ))
    .await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn migrated(cfg: &Database) -> mssql::Pool {
    let pool = mssql::build_pool(cfg).expect("pool");
    mssql::migrate(&pool).await.expect("migrate");
    pool
}

macro_rules! ms {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            with_database(|cfg| async move {
                migrated(&cfg).await;
                // Each call is a new pool, so two calls are two instances.
                let stores = move || -> Arc<dyn QuotaStore> {
                    Arc::new(MssqlQuotaStore::new(mssql::build_pool(&cfg).expect("pool")))
                };
                store_conformance::$name(&stores).await;
            })
            .await;
        }
    };
}

conformance_suite!(ms);

// ---------------------------------------------------------------------------
// This backend only
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migrations_are_idempotent_and_recorded() {
    with_database(|cfg| async move {
        let pool = migrated(&cfg).await;
        mssql::migrate(&pool)
            .await
            .expect("a second run is a no-op");

        let mut conn = pool.get().await.unwrap();
        let n = conn
            .client
            .simple_query("SELECT COUNT_BIG(*) AS n FROM dbo.simmer_migrations")
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .unwrap()
            .get::<i64, _>("n")
            .unwrap();
        assert_eq!(n, 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_migrating_together_both_succeed() {
    // sp_getapplock serialises them; without it both would try the DDL.
    with_database(|cfg| async move {
        let (a, b) = (
            mssql::build_pool(&cfg).unwrap(),
            mssql::build_pool(&cfg).unwrap(),
        );
        let (ra, rb) = tokio::join!(mssql::migrate(&a), mssql::migrate(&b));
        ra.expect("first");
        rb.expect("second");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edited_migration_refuses_to_start() {
    with_database(|cfg| async move {
        let pool = migrated(&cfg).await;
        {
            let mut conn = pool.get().await.unwrap();
            conn.client
                .simple_query(
                    "UPDATE dbo.simmer_migrations SET checksum = 0x00 \
                     WHERE version = 20260807000001",
                )
                .await
                .unwrap()
                .into_results()
                .await
                .unwrap();
        }
        let err = mssql::migrate(&pool).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("20260807000001"),
            "names the migration: {err:#}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_server_is_unavailable_not_a_panic() {
    // §7.5: the store reports it; the relay turns that into 451.
    let cfg = Database {
        connect_timeout: Duration::from_millis(500),
        ..database("server=tcp:127.0.0.1,1;user id=sa;password=x".into())
    };
    let store = MssqlQuotaStore::new(mssql::build_pool(&cfg).expect("lazy pool"));
    assert!(!store.is_available().await);
    assert!(store.route_states().await.is_err());
}

#[test]
fn a_postgres_url_names_the_other_image() {
    let err = mssql::parse_url("postgres://simmer:secret@db/simmer").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("-mssql"), "{msg}");
    assert!(!msg.contains("secret"), "never echoes the password: {msg}");
}

#[test]
fn ado_and_jdbc_strings_both_parse() {
    mssql::parse_url("server=tcp:db,1433;database=simmer;user id=sa;password=p;encrypt=true")
        .expect("ado");
    mssql::parse_url("jdbc:sqlserver://db:1433;databaseName=simmer;user=sa;password=p")
        .expect("jdbc");
}

#[test]
fn the_url_parser_never_echoes_a_password() {
    let err = mssql::parse_url("server=tcp:db,notaport;password=hunter2").unwrap_err();
    assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
}
