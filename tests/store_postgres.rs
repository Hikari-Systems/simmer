//! The §11 storage conformance suite against Postgres (D-084). See
//! `tests/store_conformance/mod.rs`.
//!
//! Postgres passing this suite is what makes the SQL Server run of it mean
//! something: the suite encodes the Postgres store's behaviour, and the other
//! backend is held to it.

#![cfg(feature = "postgres")]

mod store_conformance;

use std::sync::Arc;

use simmer::quota::{PgQuotaStore, QuotaStore};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

macro_rules! pg {
    ($name:ident) => {
        #[sqlx::test]
        async fn $name(pool: PgPool) {
            // Each call is a new pool on the per-test database, so two calls are
            // two instances' worth of connections.
            let options = (*pool.connect_options()).clone();
            let stores = move || -> Arc<dyn QuotaStore> {
                Arc::new(PgQuotaStore::new(
                    PgPoolOptions::new()
                        .max_connections(8)
                        .connect_lazy_with(options.clone()),
                ))
            };
            store_conformance::$name(&stores).await;
        }
    };
}

conformance_suite!(pg);

macro_rules! pg_spool {
    ($name:ident) => {
        #[sqlx::test]
        async fn $name(pool: PgPool) {
            let options = (*pool.connect_options()).clone();
            let stores = move || -> Arc<dyn simmer::spool::SpoolStore> {
                Arc::new(PgQuotaStore::new(
                    PgPoolOptions::new()
                        .max_connections(8)
                        .connect_lazy_with(options.clone()),
                ))
            };
            store_conformance::spool::$name(&stores).await;
        }
    };
}

spool_conformance_suite!(pg_spool);

// ---------------------------------------------------------------------------
// D-099 — the upgrade from a v0.8 schema that already holds rows
// ---------------------------------------------------------------------------

mod upgrade {
    use simmer::quota::{PgQuotaStore, QuotaStore};
    use sqlx::PgPool;

    const V08: [&str; 3] = [
        include_str!("../migrations/20260807000000_baseline.sql"),
        include_str!("../migrations/20260807000001_quota.sql"),
        include_str!("../migrations/20260810000000_recipient_event.sql"),
    ];
    const RAMP: &str = include_str!("../migrations/20260925000000_ramp.sql");

    /// A database as v0.8 left it: its schema, and one row in every table.
    async fn v08_with_rows(pool: &PgPool) {
        for sql in V08 {
            sqlx::raw_sql(sql).execute(pool).await.expect("v0.8 schema");
        }
        sqlx::raw_sql(
            "INSERT INTO quota_usage (route, domain_group, day_index, allowance, committed, reserved)
                 VALUES ('warming', 'google', 3, 100, 7, 1);
             INSERT INTO quota_reservation
                 (id, route, domain_group, day_index, count, correlation_id, expires_at)
                 VALUES (gen_random_uuid(), 'warming', 'google', 3, 1, 'c', now() + interval '1 hour');
             INSERT INTO route_state (route, paused) VALUES ('warming', true);
             INSERT INTO recipient_event (recipient_hash, route) VALUES ('\\x00', 'warming');",
        )
        .execute(pool)
        .await
        .expect("v0.8 rows");
    }

    #[sqlx::test(migrations = false)]
    async fn existing_rows_are_kept_under_the_empty_ramp_and_adopted(pool: PgPool) {
        v08_with_rows(&pool).await;
        sqlx::raw_sql(RAMP)
            .execute(&pool)
            .await
            .expect("the ramp migration");

        let legacy: i64 = sqlx::query_scalar("SELECT count(*) FROM quota_usage WHERE ramp = ''")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(legacy, 1, "the migration fills '' and moves nothing itself");

        let store = PgQuotaStore::new(pool.clone());
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
        let usage = store.usage("main", "warming", "google", 3).await.unwrap();
        assert_eq!(
            (usage.allowance, usage.committed, usage.reserved),
            (Some(100), 7, 1)
        );
        assert!(store.route_states("main").await.unwrap()["warming"].paused);
    }

    #[sqlx::test(migrations = false)]
    async fn a_v08_binary_cannot_write_to_the_migrated_schema(pool: PgPool) {
        // D-099's fence: v0.8's upsert names a conflict target that no longer
        // exists, so it fails (§7.5's 451) instead of miscounting.
        v08_with_rows(&pool).await;
        sqlx::raw_sql(RAMP)
            .execute(&pool)
            .await
            .expect("the ramp migration");

        let v08_upsert = sqlx::query(
            "INSERT INTO quota_usage (route, domain_group, day_index, allowance)
             VALUES ('warming', 'google', 4, 100)
             ON CONFLICT (route, domain_group, day_index) DO UPDATE SET updated_at = now()",
        )
        .execute(&pool)
        .await;
        assert!(v08_upsert.is_err(), "the v0.8 upsert must fail");

        let v08_pause = sqlx::query(
            "INSERT INTO route_state (route, paused) VALUES ('other', true)
             ON CONFLICT (route) DO UPDATE SET paused = true",
        )
        .execute(&pool)
        .await;
        assert!(v08_pause.is_err(), "and so must the v0.8 pause");
    }
}

// ---------------------------------------------------------------------------
// D-116 — commit_and_complete is one transaction
// ---------------------------------------------------------------------------

mod spool_atomicity {
    use chrono::{Duration, Utc};
    use simmer::config::FrequencyMode;
    use simmer::frequency::Keyer;
    use simmer::quota::{PgQuotaStore, QuotaStore, ReserveRequest, Reserved};
    use simmer::spool::{ClaimRequest, NewSpooled, SpoolStore};
    use sqlx::PgPool;
    use uuid::Uuid;

    /// The events insert is the last statement before the row's deletion.
    /// Break it, and neither the quota commit before it nor the deletion after
    /// it may survive.
    #[sqlx::test]
    async fn a_failure_inside_commit_and_complete_leaves_nothing_behind(pool: PgPool) {
        let store = PgQuotaStore::new(pool.clone());
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
                expires_at: now + Duration::hours(1),
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
                lease: Duration::seconds(1),
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
                expires_at: now + Duration::minutes(10),
                over_cap: false,
            })
            .await
            .unwrap()
        else {
            panic!("headroom")
        };

        sqlx::query("ALTER TABLE recipient_event RENAME TO recipient_event_gone")
            .execute(&pool)
            .await
            .unwrap();
        let failed = store
            .commit_and_complete(&r, std::slice::from_ref(&key), id, claim.lease_token)
            .await;
        sqlx::query("ALTER TABLE recipient_event_gone RENAME TO recipient_event")
            .execute(&pool)
            .await
            .unwrap();
        assert!(failed.is_err(), "the injected failure must surface");

        let u = store.usage("main", "warming", "catchall", 0).await.unwrap();
        assert_eq!((u.committed, u.reserved), (0, 1), "the commit rolled back");
        let again = store
            .claim_due(&ClaimRequest {
                owner: "t".into(),
                now: now + Duration::seconds(2),
                batch: 1,
                lease: Duration::seconds(60),
            })
            .await
            .unwrap();
        assert_eq!(again.len(), 1, "the row is still there to retry");
        assert_eq!(again[0].id, id);
    }
}
