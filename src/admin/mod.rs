//! §9 control plane: metrics (§9.1), reads (§9.2), writes (§9.3), dry run
//! (§9.4).
//!
//! axum rather than the house actix-web, per `SPEC.md` §12.1 — see
//! `DECISIONS.md` D-003.
//!
//! ## What is open and what is not
//!
//! `/health`, `/healthcheck` and `/metrics` are unauthenticated; everything else
//! needs §9.3's bearer token, including §9.2's reads. That is wider than §9.3
//! asks for and the reasoning is in [`auth`] and D-055.
//!
//! ## What this module must never emit
//!
//! No recipient, no recipient key, no per-recipient count. §7.3 hashes so that
//! the container does not accumulate a record of who was mailed, and an endpoint
//! that returned any of it would hand that record straight back out — which is
//! also why `simmer_recipient_events_evicted_total` carries no labels. The one
//! place a plaintext address enters the control plane at all is §9.4's dry-run
//! request body, which is supplied by the operator, evaluated, and never stored.
//!
//! ## And what a mutation must never become
//!
//! §14.1. Nothing here can change the *class* of reply a client receives: a
//! paused route and an allowance of zero both steer to §10.3's `451`, which is
//! temporary, which is the point. What they can do is make every message on a
//! chain get that answer, so [`mutate`] computes which chains a mutation
//! exhausts and says so — in the response, and at `WARN`.

pub mod auth;
pub mod dryrun;
pub mod error;
pub mod mutate;
pub mod view;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;

use crate::quota::store::UsageKey;
use crate::quota::{self, QuotaStore};
use crate::relay::Engine;

use auth::Actor;
use error::ApiError;

#[derive(Clone)]
pub struct AdminState {
    /// The same engine the SMTP listener holds — the same configuration, the
    /// same quota store, the same compiled rewrites. §9.4 is only worth having
    /// if what it reports is what would actually happen, and sharing the engine
    /// is what makes that true by construction rather than by care.
    pub engine: Engine,
    /// `None` when no recorder is installed, which is every test that does not
    /// ask for one: `metrics` allows exactly one global recorder per process, so
    /// installing it per test would fail on the second.
    pub metrics: Option<PrometheusHandle>,
}

impl AdminState {
    fn store(&self) -> &std::sync::Arc<dyn QuotaStore> {
        &self.engine.quota
    }

    fn config(&self) -> &crate::config::Config {
        &self.engine.config
    }
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        // -- open ------------------------------------------------------
        .route("/health", get(health))
        // The house probe path. [`crate::healthcheck::check_subcommand`] — what
        // `HEALTHCHECK CMD ["/app/server", "healthcheck"]` invokes — issues
        // `GET /healthcheck`, whereas §9.2 names `/health`. Serving both is
        // cheaper than diverging from either, and the two are not redundant:
        // this one is liveness-only by default, so a database outage does not
        // make Docker restart-loop a container that a restart cannot fix.
        .route("/healthcheck", get(healthcheck))
        .route("/metrics", get(metrics_endpoint))
        // -- §9.2 read -------------------------------------------------
        .route("/routes", get(routes))
        .route("/routes/{name}", get(route_by_name))
        .route("/quota", get(quota_detail))
        // -- §9.3 write ------------------------------------------------
        .route("/routes/{name}/pause", post(mutate::pause))
        .route("/routes/{name}/resume", post(mutate::resume))
        .route("/routes/{name}/graduate", post(mutate::graduate))
        .route("/routes/{name}/allowance", post(mutate::allowance))
        .route("/quota/reset", post(mutate::reset))
        // -- §9.4 ------------------------------------------------------
        .route("/dryrun", post(dryrun::dryrun))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// §9.2 health
// ---------------------------------------------------------------------------

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
    Query(q): Query<DepsQuery>,
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

    let db_ok = state.store().is_available().await;
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
            "fail_closed": state.config().database.fail_closed,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

// ---------------------------------------------------------------------------
// §9.1 metrics
// ---------------------------------------------------------------------------

/// §9.1 — Prometheus exposition.
///
/// Refreshes the §7 quota gauges from storage before rendering. Without that
/// they would only ever be set by a message that relayed, so a route that has
/// sent nothing today would export *yesterday's* numbers under a label set that
/// claims to describe today — the same trap D-026 sets for the read API, in the
/// place an operator is least likely to check. Reading it here also means
/// `/metrics` and `/routes` are computed from one projection and cannot
/// disagree.
///
/// A storage failure logs and renders anyway: a metrics endpoint that fails
/// during an outage takes away the instrument at the moment it is wanted.
async fn metrics_endpoint(State(state): State<AdminState>) -> Response {
    // §8.3's gauges are read here for the same reason and by the same rule
    // (D-056), and one extra: writing them from the relay would put a metric
    // update on the latency path of every message to say something a scrape can
    // read straight off the pool. A route nothing has sent through publishes
    // `0`, which is the answer — not silence.
    refresh_pool_gauges(&state);

    if let Err(e) = refresh_quota_gauges(&state).await {
        tracing::warn!(error = %e, "could not refresh quota gauges for /metrics");
    }

    match &state.metrics {
        Some(handle) => {
            handle.run_upkeep();
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; version=0.0.4",
                )],
                handle.render(),
            )
                .into_response()
        }
        // Reachable only in a test that built the state without a recorder.
        None => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_recorder",
            "no metrics recorder is installed in this process",
        )
        .into_response(),
    }
}

/// §9.1 `simmer_pool_connections{route,state}`, read off the pools themselves.
///
/// Unlike the quota gauges this needs no storage, so it cannot fail and is not
/// inside the fallible path above.
fn refresh_pool_gauges(state: &AdminState) {
    for route in &state.config().routes {
        let Some(stats) = state.engine.pools.stats(&route.name) else {
            continue;
        };
        crate::metrics::pool_connections(&route.name, "idle", stats.idle as f64);
        crate::metrics::pool_connections(&route.name, "active", stats.active as f64);
    }
}

async fn refresh_quota_gauges(state: &AdminState) -> Result<(), quota::QuotaError> {
    let cfg = state.config();
    let now = Utc::now();
    let states = state.store().route_states().await?;
    let usage = state.store().usage_many(&keys_for(cfg, now)).await?;

    for route in &cfg.routes {
        let projected = view::project_route(
            cfg,
            route,
            states.get(&route.name).copied().unwrap_or_default(),
            &usage,
            &state.engine.preflight,
            &state.engine.pools,
            now,
        );
        crate::metrics::warmup_day(&route.name, projected.day_index);
        // §9.3's pause has no other trace on a dashboard. `route_skipped_total`
        // only moves when a message is steered, so a route paused three weeks ago
        // on a chain nothing reaches is invisible — which is exactly the state
        // somebody eventually goes looking for. Not in §9.1's list; see D-056.
        crate::metrics::route_paused(&route.name, projected.paused);

        for group in &projected.groups {
            let allowance = match projected.status {
                // §7.2 — the route has not begun. Its ceiling today is nothing,
                // and the two available wrong answers are opposites: reporting
                // `None` here would export +Inf, which reads on a dashboard as
                // "unlimited" for a route that cannot send at all.
                // `simmer_warmup_day` is negative alongside this, which is what
                // says *why*.
                view::RouteStatus::NotStarted => 0.0,
                // §3.1's overflow route is "never quota-limited", and +Inf says
                // that in a form a dashboard can plot next to the warming routes
                // (D-024).
                _ => group
                    .allowance_override
                    .or(group.allowance)
                    .map_or(f64::INFINITY, |a| a as f64),
            };
            crate::metrics::quota_allowance(&route.name, &group.domain_group, allowance);
            crate::metrics::quota_committed(
                &route.name,
                &group.domain_group,
                group.committed as f64,
            );
            crate::metrics::quota_reserved(&route.name, &group.domain_group, group.reserved as f64);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// §9.2 read
// ---------------------------------------------------------------------------

/// Every `(route, domain_group, day_index)` the read API is about.
///
/// The day index is per route: a warming route's day begins on its own
/// `warmup.started` anniversary, an overflow route's on UTC midnight (D-024).
fn keys_for(cfg: &crate::config::Config, now: chrono::DateTime<Utc>) -> Vec<UsageKey> {
    let mut keys = Vec::with_capacity(cfg.routes.len() * cfg.domain_groups.len());
    for route in &cfg.routes {
        let day_index = quota::day::for_route(route, now);
        for group in &cfg.domain_groups {
            keys.push(UsageKey {
                route: route.name.clone(),
                domain_group: group.name.clone(),
                day_index,
            });
        }
    }
    keys
}

/// §9.2 `GET /routes`.
async fn routes(
    State(state): State<AdminState>,
    _actor: Actor,
) -> Result<Json<view::RoutesView>, ApiError> {
    let cfg = state.config();
    let now = Utc::now();
    let states = state.store().route_states().await?;
    let usage = state.store().usage_many(&keys_for(cfg, now)).await?;

    Ok(Json(view::project_routes(
        cfg,
        &states,
        &usage,
        &state.engine.preflight,
        &state.engine.pools,
        now,
    )))
}

/// §9.2 `GET /routes/{name}`.
async fn route_by_name(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    _actor: Actor,
) -> Result<Json<view::RouteView>, ApiError> {
    let cfg = state.config();
    let route = cfg
        .route(&name)
        .ok_or_else(|| ApiError::not_found("route", &name))?;

    let now = Utc::now();
    let states = state.store().route_states().await?;
    let day_index = quota::day::for_route(route, now);
    let keys: Vec<UsageKey> = cfg
        .domain_groups
        .iter()
        .map(|g| UsageKey {
            route: name.clone(),
            domain_group: g.name.clone(),
            day_index,
        })
        .collect();
    let usage = state.store().usage_many(&keys).await?;

    Ok(Json(view::project_route(
        cfg,
        route,
        states.get(&name).copied().unwrap_or_default(),
        &usage,
        &state.engine.preflight,
        &state.engine.pools,
        now,
    )))
}

#[derive(serde::Deserialize)]
pub struct QuotaQuery {
    route: Option<String>,
    group: Option<String>,
}

/// §9.2 `GET /quota?route=&group=` — "current window detail".
///
/// Both filters are optional and independent, so `?group=google` answers "what
/// is every route doing for Google today", which is the question a deliverability
/// problem actually starts as.
async fn quota_detail(
    State(state): State<AdminState>,
    Query(q): Query<QuotaQuery>,
    _actor: Actor,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = state.config();

    if let Some(route) = &q.route {
        if cfg.route(route).is_none() {
            return Err(ApiError::not_found("route", route));
        }
    }
    if let Some(group) = &q.group {
        if !cfg.domain_groups.iter().any(|g| &g.name == group) {
            return Err(ApiError::not_found("domain group", group));
        }
    }

    let now = Utc::now();
    let states = state.store().route_states().await?;
    let usage = state.store().usage_many(&keys_for(cfg, now)).await?;
    let projected = view::project_routes(
        cfg,
        &states,
        &usage,
        &state.engine.preflight,
        &state.engine.pools,
        now,
    );

    let windows: Vec<serde_json::Value> = projected
        .routes
        .into_iter()
        .filter(|r| q.route.as_ref().is_none_or(|name| &r.name == name))
        .flat_map(|r| {
            let route_name = r.name.clone();
            let status = r.status;
            let day_index = r.day_index;
            let day_started_at = r.day_started_at;
            let day_ends_at = r.day_ends_at;
            r.groups
                .into_iter()
                .filter(|g| q.group.as_ref().is_none_or(|name| &g.domain_group == name))
                .map(move |g| {
                    let mut value = serde_json::to_value(&g).unwrap_or(serde_json::Value::Null);
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("route".into(), json!(route_name));
                        obj.insert("status".into(), json!(status));
                        obj.insert("day_index".into(), json!(day_index));
                        obj.insert("day_started_at".into(), json!(day_started_at));
                        // §9.3: an allowance override "expires at the next day
                        // boundary". This is that instant, so the answer to "when
                        // does my override lapse" is in the same document as the
                        // override.
                        obj.insert("day_ends_at".into(), json!(day_ends_at));
                    }
                    value
                })
                .collect::<Vec<_>>()
        })
        .collect();

    Ok(Json(json!({
        "generated_at": now,
        "windows": windows,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config(yaml: &str) -> Config {
        crate::config::from_str(yaml, "test").expect("fixture is valid")
    }

    const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 1
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "0123456789abcdef" }
domain_groups:
  - { name: google, domains: ["gmail.com"] }
  - { name: catchall, domains: ["*"] }
senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
default_chain: [overflow]
routes:
  - name: warming
    downstream:
      host: w.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@newbrand.com" }
    warmup:
      started: "2026-08-01T09:00:00Z"
      schedule: { default: [50, 100, 200] }
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    #[test]
    fn the_read_key_set_is_every_route_times_every_group() {
        let cfg = config(CFG);
        let now: chrono::DateTime<Utc> = "2026-08-03T15:00:00Z".parse().unwrap();
        let keys = keys_for(&cfg, now);
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn each_route_is_keyed_on_its_own_day_index() {
        // The trap this catches: asking for every route at one day index. The
        // warming route started at 09:00 on the 1st and the overflow route
        // buckets on UTC midnight (D-024), so on this instant they are on
        // different day numbers and a single index would read one of them from
        // the wrong row.
        let cfg = config(CFG);
        let now: chrono::DateTime<Utc> = "2026-08-03T15:00:00Z".parse().unwrap();
        let keys = keys_for(&cfg, now);

        let warming = keys.iter().find(|k| k.route == "warming").unwrap();
        let overflow = keys.iter().find(|k| k.route == "overflow").unwrap();
        assert_eq!(warming.day_index, 2);
        assert_eq!(overflow.day_index, 20668);
        assert_ne!(warming.day_index, overflow.day_index);
    }
}
