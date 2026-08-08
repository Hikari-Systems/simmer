//! §9 control plane.
//!
//! Phase 1 provides `GET /health` only; §9.1 metrics, §9.2 read endpoints, §9.3
//! write endpoints and §9.4 dry-run land in phase 7.
//!
//! axum rather than the house actix-web, per `SPEC.md` §12.1 — see
//! `DECISIONS.md` D-003.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use sqlx::PgPool;

use crate::config::Config;

#[derive(Clone)]
pub struct AdminState {
    pub config: Arc<Config>,
    pub pool: PgPool,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        // §9.2.
        .route("/health", get(health))
        // The house probe path. `hs_utils::healthcheck::check_subcommand` — what
        // `HEALTHCHECK CMD ["/app/server", "healthcheck"]` invokes — issues
        // `GET /healthcheck`, whereas §9.2 names `/health`. Serving both is
        // cheaper than diverging from either.
        .route("/healthcheck", get(healthcheck))
        .with_state(state)
}

#[derive(serde::Deserialize)]
pub struct DepsQuery {
    deps: Option<String>,
}

/// §9.2: "GET /health — liveness; includes database reachability."
///
/// Returns 503 when the database is unreachable. With `fail_closed: true` (§7.5)
/// that state means every message is being answered `451`, so the instance is not
/// usefully serving and a load balancer should be told so.
async fn health(State(state): State<AdminState>) -> (StatusCode, Json<serde_json::Value>) {
    report(&state, true).await
}

/// The house liveness probe.
///
/// Liveness only by default, and dependency-aware with `?deps=true`. The
/// distinction matters because this is the path Docker's `HEALTHCHECK` drives,
/// and Docker reacts to an unhealthy container by restarting it. A database
/// outage is not something a restart fixes, so making it fail here would turn a
/// dependency blip into a restart loop on top of an outage. Operators and load
/// balancers get the full picture from `/health`.
async fn healthcheck(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<DepsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let check_deps = matches!(q.deps.as_deref(), Some("true") | Some("1"));
    report(&state, check_deps).await
}

async fn report(state: &AdminState, check_db: bool) -> (StatusCode, Json<serde_json::Value>) {
    if !check_db {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }

    let db_ok = crate::db::is_reachable(&state.pool).await;
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(json!({
            "status": if db_ok { "ok" } else { "degraded" },
            "database": if db_ok { "up" } else { "down" },
            "fail_closed": state.config.database.fail_closed,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}
