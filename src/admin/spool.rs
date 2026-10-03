//! §9.2 and §9.3 for the spool (D-125). Every endpoint answers `404` with
//! `spool_disabled` when no ramp spools.
//!
//! The read side keeps §9's no-`@` rule: no envelope address, and a dead
//! letter's last downstream text — which very often quotes the recipient — has
//! anything address-shaped redacted. The write side is audited like every
//! other mutation (D-053), and none of it can change the class of a reply: a
//! paused or draining ramp ends at `451`, never a `5xx` (§14.1).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use super::auth::Actor;
use super::error::ApiError;
use super::mutate::{audit, MaybeJson, MutationResponse, NoBody};
use super::AdminState;
use crate::config::{Delivery, Ramp};
use crate::spool::{RetryDead, Spool};

fn spool(state: &AdminState) -> Result<&Arc<Spool>, ApiError> {
    state.engine.spool.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "spool_disabled",
            "no ramp has delivery: spool",
        )
    })
}

fn spooling_ramp<'a>(state: &'a AdminState, name: &str) -> Result<&'a Ramp, ApiError> {
    let ramp = state
        .engine
        .config
        .ramps
        .get(name)
        .ok_or_else(|| ApiError::not_found("ramp", name))?;
    if ramp.delivery != Delivery::Spool {
        return Err(ApiError::bad_request(format!(
            "ramp '{name}' is synchronous; it has no spool"
        )));
    }
    Ok(ramp)
}

/// Anything shaped like `local@domain`, replaced: a downstream's `550 5.1.1
/// <bob@example.com>... unknown` must not turn a read endpoint into a list of
/// recipients (§7.3's reason for hashing them).
pub fn redact_addresses(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^\s<>()\[\]:;,]+@[^\s<>()\[\]:;,]+").unwrap());
    re.replace_all(text, "<redacted>").into_owned()
}

/// §9.2 `GET /spool` — totals, every lane with a live message, and each
/// spooling ramp's pause and drain state.
pub async fn overview(
    State(state): State<AdminState>,
    _actor: Actor,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spool = spool(&state)?;
    let now = Utc::now();
    let totals = spool.store.totals().await?;
    let lanes = spool.store.lanes().await?;
    let states = spool.store.spool_states().await?;
    let ramps: serde_json::Map<String, serde_json::Value> = state
        .engine
        .config
        .ramps
        .iter()
        .filter(|r| r.delivery == Delivery::Spool)
        .map(|r| {
            let s = states.get(&r.name).copied().unwrap_or_default();
            let depth: i64 = lanes
                .iter()
                .filter(|l| l.ramp == r.name)
                .map(|l| l.depth)
                .sum();
            (
                r.name.clone(),
                json!({
                    "paused": s.paused,
                    "draining": s.draining,
                    "depth": depth,
                    // The cutover step (§1.1): draining and empty.
                    "drained": s.draining && depth == 0,
                }),
            )
        })
        .collect();
    Ok(Json(json!({
        "messages": totals.messages,
        "bytes": totals.bytes,
        "max_messages": spool.cfg.max_messages,
        "max_bytes": spool.cfg.max_bytes,
        "ramps": ramps,
        "lanes": lanes.iter().map(|l| json!({
            "ramp": l.ramp,
            "domain_group": l.domain_group,
            "depth": l.depth,
            "bytes": l.bytes,
            "oldest_seconds": l.oldest_received_at.map(|t| (now - t).num_seconds().max(0)),
            "next_attempt_at": l.next_attempt_at.map(|t| t.to_rfc3339()),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeadQuery {
    limit: Option<u32>,
}

/// §9.2 `GET /spool/dead` — newest first, no addresses (D-120).
pub async fn dead(
    State(state): State<AdminState>,
    Query(q): Query<DeadQuery>,
    _actor: Actor,
) -> Result<Json<serde_json::Value>, ApiError> {
    let spool = spool(&state)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let entries = spool.store.dead_entries(limit).await?;
    Ok(Json(json!({
        "dead": entries.iter().map(|e| json!({
            "id": e.id.to_string(),
            "ramp": e.ramp,
            "domain_group": e.domain_group,
            "route": e.route,
            "reason": e.reason.map(|r| r.as_str()),
            "code": e.last_code,
            "text": e.last_error.as_deref().map(redact_addresses),
            "attempts": e.attempts,
            "received_at": e.received_at.to_rfc3339(),
            "dead_at": e.dead_at.map(|t| t.to_rfc3339()),
            "retryable": e.body_retained,
        })).collect::<Vec<_>>(),
    })))
}

fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(id).map_err(|_| ApiError::bad_request(format!("'{id}' is not a spool id")))
}

/// §9.3 `POST /spool/dead/{id}/retry` — only while the body is kept (D-121).
pub async fn retry(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    actor: Actor,
    MaybeJson(_): MaybeJson<NoBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    let spool = spool(&state)?;
    let id = parse_id(&id)?;
    let now = Utc::now();
    let hold = chrono::Duration::from_std(spool.cfg.max_hold).unwrap_or_default();
    match spool.store.retry_dead(id, now, now + hold).await? {
        RetryDead::Requeued => {}
        RetryDead::NotFound => return Err(ApiError::not_found("dead letter", &id.to_string())),
        RetryDead::NoBody => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "no_body",
                "this dead letter's body has been deleted (spool.dead_letter.keep_body); \
                 there is nothing to retry",
            ))
        }
    }
    let detail = json!({ "id": id.to_string(), "requeued": true });
    audit("spool_retry", &actor, "-", "-", detail.clone(), &[]);
    Ok(Json(MutationResponse {
        action: "spool_retry",
        actor: actor.name,
        ramp: "-".into(),
        detail,
        warnings: Vec::new(),
    }))
}

/// §9.3 `DELETE /spool/{id}` — a message in any state, and its body.
pub async fn delete(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    actor: Actor,
) -> Result<Json<MutationResponse>, ApiError> {
    let spool = spool(&state)?;
    let id = parse_id(&id)?;
    let Some(body_ref) = spool.store.delete_message(id).await? else {
        return Err(ApiError::not_found("spooled message", &id.to_string()));
    };
    if let Some(r) = &body_ref {
        if let Err(e) = spool.body.delete(r).await {
            // The row is gone; the orphan sweeper will take the body.
            tracing::warn!(spool_id = %id, error = %e, "deleting a body after DELETE /spool");
        }
    }
    let detail = json!({ "id": id.to_string(), "deleted": true });
    audit("spool_delete", &actor, "-", "-", detail.clone(), &[]);
    Ok(Json(MutationResponse {
        action: "spool_delete",
        actor: actor.name,
        ramp: "-".into(),
        detail,
        warnings: Vec::new(),
    }))
}

pub async fn pause(
    State(state): State<AdminState>,
    Path(ramp): Path<String>,
    actor: Actor,
    MaybeJson(_): MaybeJson<NoBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    set_paused(state, ramp, actor, true).await
}

pub async fn resume(
    State(state): State<AdminState>,
    Path(ramp): Path<String>,
    actor: Actor,
    MaybeJson(_): MaybeJson<NoBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    set_paused(state, ramp, actor, false).await
}

async fn set_paused(
    state: AdminState,
    ramp: String,
    actor: Actor,
    paused: bool,
) -> Result<Json<MutationResponse>, ApiError> {
    let spool = spool(&state)?;
    let ramp = spooling_ramp(&state, &ramp)?;
    spool.store.set_spool_paused(&ramp.name, paused).await?;
    let action = if paused {
        "spool_pause"
    } else {
        "spool_resume"
    };
    let warnings = if paused {
        vec![format!(
            "nothing in ramp '{}' will be delivered until it is resumed; accepted messages \
             keep counting down their hold and expire into the dead-letter list",
            ramp.name
        )]
    } else {
        Vec::new()
    };
    let detail = json!({ "paused": paused });
    audit(action, &actor, &ramp.name, "-", detail.clone(), &warnings);
    Ok(Json(MutationResponse {
        action,
        actor: actor.name,
        ramp: ramp.name.clone(),
        detail,
        warnings,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrainBody {
    #[serde(default = "yes")]
    pub draining: bool,
}

impl Default for DrainBody {
    fn default() -> Self {
        Self { draining: true }
    }
}

fn yes() -> bool {
    true
}

/// §9.3 `POST /ramps/{ramp}/spool/drain` — the cutover step (§1.1): accept
/// nothing new into the ramp (`451 4.7.1`) and keep delivering what is there.
/// The response says how much remains; `GET /spool` says when it is `drained`.
/// `{"draining": false}` reverses it.
pub async fn drain(
    State(state): State<AdminState>,
    Path(ramp): Path<String>,
    actor: Actor,
    MaybeJson(body): MaybeJson<DrainBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    let spool = spool(&state)?;
    let ramp = spooling_ramp(&state, &ramp)?;
    spool
        .store
        .set_spool_draining(&ramp.name, body.draining)
        .await?;
    let remaining: i64 = spool
        .store
        .lanes()
        .await?
        .iter()
        .filter(|l| l.ramp == ramp.name)
        .map(|l| l.depth)
        .sum();
    let warnings = if body.draining {
        vec![format!(
            "ramp '{}' now answers every new message 451 4.7.1 until the drain is reversed",
            ramp.name
        )]
    } else {
        Vec::new()
    };
    let detail = json!({
        "draining": body.draining,
        "remaining": remaining,
        "drained": body.draining && remaining == 0,
    });
    audit(
        "spool_drain",
        &actor,
        &ramp.name,
        "-",
        detail.clone(),
        &warnings,
    );
    Ok(Json(MutationResponse {
        action: "spool_drain",
        actor: actor.name,
        ramp: ramp.name.clone(),
        detail,
        warnings,
    }))
}

/// D-056 for the spool: read on the scrape. Every configured lane of every
/// spooling ramp is published, `0` when empty, so a lane that drains does not
/// keep its last depth.
pub async fn refresh_gauges(state: &AdminState) -> Result<(), crate::quota::QuotaError> {
    let Some(spool) = state.engine.spool.as_ref() else {
        return Ok(());
    };
    let now = Utc::now();
    let totals = spool.store.totals().await?;
    let lanes = spool.store.lanes().await?;
    crate::metrics::spool_bytes(totals.bytes);
    let oldest = lanes
        .iter()
        .filter_map(|l| l.oldest_received_at)
        .min()
        .map_or(0, |t| (now - t).num_seconds().max(0));
    crate::metrics::spool_oldest_seconds(oldest);
    for ramp in state
        .engine
        .config
        .ramps
        .iter()
        .filter(|r| r.delivery == Delivery::Spool)
    {
        for group in &ramp.domain_groups {
            let depth = lanes
                .iter()
                .find(|l| l.ramp == ramp.name && l.domain_group == group.name)
                .map_or(0, |l| l.depth);
            crate::metrics::spool_depth(&ramp.name, &group.name, depth);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_redacted_from_downstream_text() {
        assert_eq!(
            redact_addresses("550 5.1.1 <bob@example.com>... User unknown"),
            "550 5.1.1 <<redacted>>... User unknown"
        );
        assert_eq!(redact_addresses("no address here"), "no address here");
        assert!(!redact_addresses("to: a.b+c@d.example, x@y").contains('@'));
    }
}
