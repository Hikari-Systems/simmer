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

macro_rules! ms_spool {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            with_database(|cfg| async move {
                migrated(&cfg).await;
                let stores = move || -> Arc<dyn simmer::spool::SpoolStore> {
                    Arc::new(MssqlQuotaStore::new(mssql::build_pool(&cfg).expect("pool")))
                };
                store_conformance::spool::$name(&stores).await;
            })
            .await;
        }
    };
}

spool_conformance_suite!(ms_spool);

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
        // Counted from the directory rather than written down: a literal here
        // went stale the moment the next migration landed, and the suite that
        // catches it is the one only the SQL Server CI leg runs.
        let files = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations-mssql"))
            .expect("migrations-mssql/")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
            .count();
        assert_eq!(
            n,
            i64::try_from(files).unwrap(),
            "one row per file in migrations-mssql/"
        );
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
    assert!(store.route_states("main").await.is_err());
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

// ---------------------------------------------------------------------------
// D-099 — the upgrade from a v0.8 schema that already holds rows
// ---------------------------------------------------------------------------

const V08: [&str; 3] = [
    include_str!("../migrations-mssql/20260807000000_baseline.sql"),
    include_str!("../migrations-mssql/20260807000001_quota.sql"),
    include_str!("../migrations-mssql/20260810000000_recipient_event.sql"),
];
const RAMP: &str = include_str!("../migrations-mssql/20260925000000_ramp.sql");

/// Run one batch on the test database, as `mssql::migrate` does: in a
/// transaction, with XACT_ABORT on.
async fn batch(pool: &mssql::Pool, sql: &str) -> Result<(), tiberius::error::Error> {
    let mut conn = pool.get().await.expect("connection");
    conn.broken = true;
    let client = &mut conn.client;
    client
        .simple_query("SET XACT_ABORT ON; BEGIN TRANSACTION")
        .await?
        .into_results()
        .await?;
    client.simple_query(sql).await?.into_results().await?;
    client
        .simple_query("COMMIT TRANSACTION")
        .await?
        .into_results()
        .await?;
    Ok(())
}

async fn v08_with_rows(pool: &mssql::Pool, route: &str) {
    for sql in V08 {
        batch(pool, sql).await.expect("v0.8 schema");
    }
    batch(
        pool,
        &format!(
            "INSERT INTO dbo.quota_usage \
                 (route, domain_group, day_index, allowance, committed, reserved) \
                 VALUES (N'{route}', N'google', 3, 100, 7, 1); \
             INSERT INTO dbo.quota_reservation \
                 (id, route, domain_group, day_index, [count], correlation_id, expires_at) \
                 VALUES (NEWID(), N'{route}', N'google', 3, 1, N'c', \
                         DATEADD(hour, 1, SYSUTCDATETIME())); \
             INSERT INTO dbo.route_state (route, paused) VALUES (N'{route}', 1); \
             INSERT INTO dbo.recipient_event (recipient_hash, route) \
                 VALUES (0x00, N'{route}');"
        ),
    )
    .await
    .expect("v0.8 rows");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_keeps_existing_rows_under_the_empty_ramp_and_adopts_them() {
    with_database(|cfg| async move {
        let pool = mssql::build_pool(&cfg).expect("pool");
        v08_with_rows(&pool, "warming").await;
        batch(&pool, RAMP).await.expect("the ramp migration");

        // D-099's fence: a v0.8 insert names no ramp, and there is no default.
        let v08_insert = batch(
            &pool,
            "INSERT INTO dbo.quota_usage (route, domain_group, day_index, allowance) \
             VALUES (N'warming', N'google', 4, 100);",
        )
        .await;
        assert!(v08_insert.is_err(), "a v0.8 write must fail");

        let store = MssqlQuotaStore::new(pool.clone());
        let adopted = store.adopt_legacy_rows("main").await.expect("adopt");
        assert_eq!(
            (
                adopted.quota_usage,
                adopted.quota_reservation,
                adopted.route_state,
                adopted.recipient_event
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(adopted.routes, vec!["warming"]);
        let usage = store.usage("main", "warming", "google", 3).await.unwrap();
        assert_eq!(
            (usage.allowance, usage.committed, usage.reserved),
            (Some(100), 7, 1)
        );
        assert!(store.route_states("main").await.unwrap()["warming"].paused);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_refuses_a_stored_name_longer_than_128() {
    with_database(|cfg| async move {
        let pool = mssql::build_pool(&cfg).expect("pool");
        v08_with_rows(&pool, &"r".repeat(129)).await;

        let err = batch(&pool, RAMP)
            .await
            .expect_err("a 129-character route must stop the migration");
        assert!(err.to_string().contains("128"), "{err}");

        // Nothing changed: the schema is still v0.8's.
        let mut conn = pool.get().await.unwrap();
        let has_ramp = conn
            .client
            .simple_query("SELECT COL_LENGTH(N'dbo.quota_usage', N'ramp') AS n")
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .unwrap()
            .get::<i16, _>("n");
        assert_eq!(has_ramp, None, "the migration rolled back whole");
    })
    .await;
}

// ---------------------------------------------------------------------------
// D-116 — commit_and_complete is one transaction (see store_postgres.rs)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_inside_commit_and_complete_leaves_nothing_behind() {
    use chrono::{Duration as ChronoDuration, Utc};
    use simmer::config::FrequencyMode;
    use simmer::frequency::Keyer;
    use simmer::quota::{ReserveRequest, Reserved};
    use simmer::spool::{ClaimRequest, NewSpooled, SpoolStore};
    use uuid::Uuid;

    with_database(|cfg| async move {
        let pool = migrated(&cfg).await;
        let store = MssqlQuotaStore::new(pool.clone());
        let keyer = Keyer::new(store.recipient_hash_salt().await.unwrap());
        let key = keyer.key_for("jane@example.com", FrequencyMode::ToAddress, &[]);
        let now = Utc::now();
        let id = Uuid::new_v4();
        store
            .enqueue(&NewSpooled {
                id,
                ramp: "main".into(),
                domain_group: "catchall".into(),
                group_basis: "literal".into(),
                received_at: now,
                expires_at: now + ChronoDuration::hours(1),
                next_attempt_at: now,
                envelope: "{}".into(),
                body_ref: format!("bodies/{id}"),
                body_bytes: 1,
                body_sha256: vec![0; 32],
                uuid_seed: Uuid::new_v4(),
            })
            .await
            .unwrap();
        let claim = store
            .claim_due(&ClaimRequest {
                owner: "t".into(),
                now,
                batch: 1,
                lease: ChronoDuration::seconds(1),
            })
            .await
            .unwrap()
            .remove(0);
        let Reserved::Taken(r) = store
            .reserve(&ReserveRequest {
                ramp: "main".into(),
                route: "warming".into(),
                domain_group: "catchall".into(),
                day_index: 0,
                allowance: Some(5),
                count: 1,
                correlation_id: "c".into(),
                expires_at: now + ChronoDuration::minutes(10),
                over_cap: false,
            })
            .await
            .unwrap()
        else {
            panic!("headroom")
        };

        let rename = |from: &'static str, to: &'static str| {
            let pool = pool.clone();
            async move {
                let mut conn = pool.get().await.unwrap();
                conn.client
                    .simple_query(format!("EXEC sp_rename 'dbo.{from}', '{to}'"))
                    .await
                    .unwrap()
                    .into_results()
                    .await
                    .unwrap();
            }
        };
        rename("recipient_event", "recipient_event_gone").await;
        let failed = store
            .commit_and_complete(&r, std::slice::from_ref(&key), id, claim.lease_token)
            .await;
        rename("recipient_event_gone", "recipient_event").await;
        assert!(failed.is_err(), "the injected failure must surface");

        let u = store.usage("main", "warming", "catchall", 0).await.unwrap();
        assert_eq!((u.committed, u.reserved), (0, 1), "the commit rolled back");
        let again = store
            .claim_due(&ClaimRequest {
                owner: "t".into(),
                now: now + ChronoDuration::seconds(2),
                batch: 1,
                lease: ChronoDuration::seconds(60),
            })
            .await
            .unwrap();
        assert_eq!(again.len(), 1, "the row is still there to retry");
        assert_eq!(again[0].id, id);
    })
    .await;
}
