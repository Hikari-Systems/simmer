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
