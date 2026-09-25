//! §9.3's write API.
//!
//! The database already honours all four mutations — `chain::walk_and_reserve`
//! reads `route_states()` once per message, and `quota_usage.allowance_override`
//! wins over the schedule (D-025). So this file is an HTTP surface and an audit
//! log, not new semantics.
//!
//! ## What a mutation is allowed to do to a client's answer
//!
//! §14.1: Simmer must never emit a reply that makes a client record permanent
//! state. Nothing here can: a paused route and an allowance of zero both make a
//! route ineligible, an ineligible chain is §10.3, and §10.3's default is `451`.
//! The reply stays temporary however hard an operator leans on this API.
//!
//! What a mutation *can* do is make every message on a chain get that answer.
//! That is a legitimate thing to want — an allowance of zero is the only way to
//! stop one domain group without pausing the whole route — and it is also the
//! single most consequential thing this service can be told to do. So every
//! mutation computes which chains it leaves with nothing eligible and returns
//! them in a `warnings` array, and logs them at `WARN`. Refusing outright was
//! considered and rejected: it would remove a capability §9.3 implies, and an
//! operator who means it would reach for `pause` and get a blunter version of
//! the same state with no warning at all.
//!
//! ## The audit line
//!
//! Every mutation logs at `INFO` with the acting token's name (O-11, D-053), the
//! action, what it targeted, and the value it replaced. Never the token.

use axum::extract::{FromRequest, Path, Request, State};
use axum::Json;
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::auth::Actor;
use super::error::ApiError;
use super::AdminState;
use crate::config::Ramp;
use crate::metrics;
use crate::quota::store::RouteState;
use crate::quota::{self, Allowance, QuotaError};

// ---------------------------------------------------------------------------
// a body that may be absent
// ---------------------------------------------------------------------------

/// A JSON body that may be omitted entirely.
///
/// `POST /routes/x/pause` has nothing to say in a body, and an operator reaching
/// for `curl -XPOST` should not have to send `{}` to be understood. An empty
/// body reads as `T::default()`; anything present must parse.
pub struct MaybeJson<T>(pub T);

impl<T, S> FromRequest<S> for MaybeJson<T>
where
    T: DeserializeOwned + Default,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::bad_request(format!("could not read the request body: {e}")))?;

        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(MaybeJson(T::default()));
        }

        serde_json::from_slice(&bytes)
            .map(MaybeJson)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
    }
}

/// An endpoint that takes no body at all.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoBody {}

// ---------------------------------------------------------------------------
// the shared response
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct MutationResponse {
    pub action: &'static str,
    /// O-11 / D-053 — the configured name of the token presented.
    pub actor: String,
    #[serde(flatten)]
    pub detail: serde_json::Value,
    /// §14.1's operational warnings. Empty is the ordinary case.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// §9.3 pause / resume
// ---------------------------------------------------------------------------

pub async fn pause(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    actor: Actor,
    MaybeJson(_): MaybeJson<NoBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    set_paused(state, name, actor, true).await
}

pub async fn resume(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    actor: Actor,
    MaybeJson(_): MaybeJson<NoBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    set_paused(state, name, actor, false).await
}

async fn set_paused(
    state: AdminState,
    name: String,
    actor: Actor,
    paused: bool,
) -> Result<Json<MutationResponse>, ApiError> {
    let action = if paused { "pause" } else { "resume" };
    known_route(state.config().default_ramp(), &name, action)?;

    // Read before writing, so the audit line can say what it replaced. Racy in
    // principle against a second operator; an audit line's "previous" always is.
    let previous = state
        .store()
        .route_states()
        .await?
        .get(&name)
        .copied()
        .unwrap_or_default()
        .paused;

    state
        .store()
        .set_paused(&name, paused)
        .await
        .inspect_err(|_| {
            metrics::admin_mutation(action, "failed");
        })?;

    let warnings = exhaustion_warnings(&state).await?;

    audit(
        action,
        &actor,
        &name,
        json!({ "paused": paused, "previous": previous }),
        &warnings,
    );

    Ok(Json(MutationResponse {
        action,
        actor: actor.name,
        detail: json!({ "route": name, "paused": paused, "previous": previous }),
        warnings,
    }))
}

// ---------------------------------------------------------------------------
// §9.3 graduate
// ---------------------------------------------------------------------------

/// §9.3 names only one direction. The reverse is here because graduating pins a
/// route to its *final* allowance immediately, and an operator who does that to
/// the wrong route needs a way back that is not a manual `UPDATE`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraduateBody {
    #[serde(default = "yes")]
    pub graduated: bool,
}

impl Default for GraduateBody {
    fn default() -> Self {
        Self { graduated: true }
    }
}

fn yes() -> bool {
    true
}

pub async fn graduate(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    actor: Actor,
    MaybeJson(body): MaybeJson<GraduateBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    let route = known_route(state.config().default_ramp(), &name, "graduate")?;

    if route.overflow {
        // §3.1: an overflow route carries no warm-up schedule, so there is no
        // final value to pin it to. Accepting the write would store a flag
        // nothing reads and report success for a no-op.
        metrics::admin_mutation("graduate", "rejected");
        return Err(ApiError::bad_request(format!(
            "route '{name}' is an overflow route: it carries no warm-up schedule (§3.1), \
             so there is no final value to graduate to"
        )));
    }

    let previous = state
        .store()
        .route_states()
        .await?
        .get(&name)
        .copied()
        .unwrap_or_default()
        .graduated;

    state
        .store()
        .set_graduated(&name, body.graduated)
        .await
        .inspect_err(|_| metrics::admin_mutation("graduate", "failed"))?;

    // §7.2, and worth saying out loud: graduation changes the *ceiling*, not the
    // rows already written. `quota_usage.allowance` is authoritative once
    // written (D-026), so today's row keeps today's number and the pinned value
    // takes effect at the next day boundary.
    let warnings = exhaustion_warnings(&state).await?;

    audit(
        "graduate",
        &actor,
        &name,
        json!({ "graduated": body.graduated, "previous": previous }),
        &warnings,
    );

    Ok(Json(MutationResponse {
        action: "graduate",
        actor: actor.name,
        detail: json!({
            "route": name,
            "graduated": body.graduated,
            "previous": previous,
            "effective": "today's quota_usage row keeps the allowance it was created with \
                          (D-026); the pinned value applies from the next day boundary",
        }),
        warnings,
    }))
}

// ---------------------------------------------------------------------------
// §9.3 allowance override
// ---------------------------------------------------------------------------

/// A field that must be *present* and may be `null`.
///
/// serde derive special-cases a field typed `Option<T>` so that a missing field
/// reads as `None` — which would make "I forgot to say" and "clear the
/// override" the same request. For something that can zero a ceiling they must
/// not be.
///
/// A plain newtype restores the distinction. Deliberately **not**
/// `#[serde(transparent)]`: transparent makes this deserialize exactly as
/// `Option<T>` does, including from serde's missing-field deserializer, which is
/// the behaviour being fixed. In JSON a newtype struct is just its inner value,
/// so nothing about the wire format changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Present<T>(pub Option<T>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowanceBody {
    pub domain_group: String,
    /// `null` clears the override and returns the row to its own `allowance`.
    pub allowance: Present<i64>,
}

pub async fn allowance(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    actor: Actor,
    Json(body): Json<AllowanceBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    let route = known_route(state.config().default_ramp(), &name, "allowance")?;

    if state
        .config()
        .default_ramp()
        .domain_group(&body.domain_group)
        .is_none()
    {
        metrics::admin_mutation("allowance", "rejected");
        return Err(ApiError::not_found("domain group", &body.domain_group));
    }

    if let Some(value) = body.allowance.0 {
        if value < 0 {
            metrics::admin_mutation("allowance", "rejected");
            return Err(ApiError::bad_request(
                "allowance must not be negative; use 0 to stop this domain group, or null \
                 to clear the override",
            ));
        }
    }

    let now = Utc::now();
    let day_index = quota::day::for_route(route, now);
    let states = state.store().route_states().await?;
    let route_state = states.get(&name).copied().unwrap_or_default();
    let scheduled = quota::allowance_for(route, &body.domain_group, day_index, route_state);

    let previous = state
        .store()
        .usage(&name, &body.domain_group, day_index)
        .await?
        .allowance_override;

    state
        .store()
        .set_allowance_override(
            &name,
            &body.domain_group,
            day_index,
            body.allowance.0,
            scheduled.as_column(),
        )
        .await
        .inspect_err(|_| metrics::admin_mutation("allowance", "failed"))?;

    let warnings = exhaustion_warnings(&state).await?;

    audit(
        "allowance",
        &actor,
        &name,
        json!({
            "domain_group": body.domain_group,
            "day_index": day_index,
            "allowance": body.allowance.0,
            "previous": previous,
        }),
        &warnings,
    );

    Ok(Json(MutationResponse {
        action: "allowance",
        actor: actor.name,
        detail: json!({
            "route": name,
            "domain_group": body.domain_group,
            "day_index": day_index,
            "allowance": body.allowance.0,
            "previous": previous,
            // §9.3: "expires at the next day boundary". No expiry job does this —
            // the override is a column on the row for one day index, and tomorrow
            // is a different row (D-025).
            "expires_at": quota::day::boundary(quota::day::origin(route), day_index + 1),
        }),
        warnings,
    }))
}

// ---------------------------------------------------------------------------
// §9.3 quota reset
// ---------------------------------------------------------------------------

/// §9.3: "Destructive; requires an explicit confirmation field in the body."
const RESET_CONFIRMATION: &str = "reset";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetBody {
    pub route: String,
    pub domain_group: String,
    /// Must be the literal `"reset"`. A boolean would be satisfied by a `true`
    /// that a script produced without anyone reading this sentence.
    pub confirm: String,
}

pub async fn reset(
    State(state): State<AdminState>,
    actor: Actor,
    Json(body): Json<ResetBody>,
) -> Result<Json<MutationResponse>, ApiError> {
    let route = known_route(state.config().default_ramp(), &body.route, "reset")?;

    if state
        .config()
        .default_ramp()
        .domain_group(&body.domain_group)
        .is_none()
    {
        metrics::admin_mutation("reset", "rejected");
        return Err(ApiError::not_found("domain group", &body.domain_group));
    }

    if body.confirm != RESET_CONFIRMATION {
        metrics::admin_mutation("reset", "rejected");
        return Err(ApiError::bad_request(format!(
            "this discards today's committed count for '{}' / '{}', which is the record of \
             what the ramp has already sent. Send \"confirm\": \"{RESET_CONFIRMATION}\" if \
             that is what you mean",
            body.route, body.domain_group
        )));
    }

    let day_index = quota::day::for_route(route, Utc::now());
    let outcome = state
        .store()
        .reset_counters(&body.route, &body.domain_group, day_index)
        .await
        .inspect_err(|_| metrics::admin_mutation("reset", "failed"))?;

    let Some(outcome) = outcome else {
        // No row: nothing has been sent on this route and group today, so there
        // is nothing to reset. Reported rather than 404'd — the operator's intent
        // ("this counter should be zero") is already satisfied.
        metrics::admin_mutation("reset", "noop");
        return Ok(Json(MutationResponse {
            action: "reset",
            actor: actor.name,
            detail: json!({
                "route": body.route,
                "domain_group": body.domain_group,
                "day_index": day_index,
                "reset": false,
                "reason": "no quota_usage row exists for this route, group and day",
            }),
            warnings: Vec::new(),
        }));
    };

    let warnings = exhaustion_warnings(&state).await?;

    audit(
        "reset",
        &actor,
        &body.route,
        json!({
            "domain_group": body.domain_group,
            "day_index": day_index,
            "committed_before": outcome.committed_before,
            "reserved_before": outcome.reserved_before,
            "reserved_after": outcome.reserved_after,
        }),
        &warnings,
    );

    Ok(Json(MutationResponse {
        action: "reset",
        actor: actor.name,
        detail: json!({
            "route": body.route,
            "domain_group": body.domain_group,
            "day_index": day_index,
            "reset": true,
            "committed_before": outcome.committed_before,
            "reserved_before": outcome.reserved_before,
            // Recomputed from the live reservations rather than zeroed: an
            // in-flight send still owns its headroom, and handing it away twice
            // would overshoot the ceiling this service exists to hold.
            "reserved_after": outcome.reserved_after,
        }),
        warnings,
    }))
}

// ---------------------------------------------------------------------------
// §14.1 — what did this mutation just do to the chains?
// ---------------------------------------------------------------------------

/// Which `(chain, domain group)` pairs now have no eligible route.
///
/// Computed *after* the write, from storage, so it describes the state an
/// operator has actually produced rather than the one the request asked for.
///
/// A pair is exhausted when every route in the chain is paused, has not started,
/// or has no headroom left for that group today. §7.3's recipient-frequency
/// check is deliberately not modelled: it is per recipient, so "this chain is
/// exhausted" is not a property of the configuration for it, and asserting one
/// would be a guess dressed as a warning.
///
/// Empty is both the ordinary case and the cheap case.
pub async fn exhaustion_warnings(state: &AdminState) -> Result<Vec<String>, QuotaError> {
    // D-057 within one ramp (D-099): a ramp's routes can only empty its own
    // chains. The default ramp until phase 3 scopes mutations by path.
    let ramp = state.config().default_ramp();
    let now = Utc::now();
    let states = state.store().route_states().await?;
    let usage = state
        .store()
        .usage_many(&super::keys_for(ramp, now))
        .await?;

    let mut out = Vec::new();

    for chain in ramp.chains() {
        for group in &ramp.domain_groups {
            let any_eligible = chain.routes.iter().any(|name| {
                ramp.route(name).is_some_and(|route| {
                    eligible(
                        route,
                        &group.name,
                        states.get(name).copied().unwrap_or_default(),
                        usage
                            .get(&(name.clone(), group.name.clone()))
                            .copied()
                            .unwrap_or_default(),
                        now,
                    )
                })
            });

            if !any_eligible {
                out.push(format!(
                    "{} has no eligible route for domain group '{}': every message on it will \
                     be answered 451 (§10.3) until this changes",
                    chain.path, group.name
                ));
            }
        }
    }

    Ok(out)
}

/// Could this route carry one more message for this group right now?
///
/// The same three questions `chain::walk_and_reserve` asks, minus §7.3's, which
/// needs a recipient.
fn eligible(
    route: &crate::config::Route,
    group: &str,
    state: RouteState,
    usage: crate::quota::Usage,
    now: chrono::DateTime<Utc>,
) -> bool {
    if state.paused {
        return false;
    }

    let day_index = quota::day::for_route(route, now);
    let allowance = quota::allowance_for(route, group, day_index, state);
    if allowance == Allowance::NotStarted {
        return false;
    }

    // An absent row reads as all-zero, so the ceiling to test against is the one
    // the first message would write.
    crate::quota::Usage {
        allowance: usage.allowance.or(allowance.as_column()),
        ..usage
    }
    .has_headroom_for(1)
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

fn known_route<'a>(
    ramp: &'a Ramp,
    name: &str,
    action: &'static str,
) -> Result<&'a crate::config::Route, ApiError> {
    ramp.route(name).ok_or_else(|| {
        metrics::admin_mutation(action, "rejected");
        ApiError::not_found("route", name)
    })
}

/// §9.3: "All mutations are logged at `INFO` with the acting token's
/// identifier."
fn audit(
    action: &'static str,
    actor: &Actor,
    route: &str,
    detail: serde_json::Value,
    warnings: &[String],
) {
    metrics::admin_mutation(action, "applied");

    tracing::info!(
        admin_action = action,
        actor = %actor,
        route = %route,
        detail = %detail,
        "admin mutation applied"
    );

    // Separate lines, at a level that pages. An operator who has just made every
    // message on a chain 451 should not have to read a nested field to find out.
    for warning in warnings {
        tracing::warn!(
            admin_action = action,
            actor = %actor,
            "{warning}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_body_reads_as_the_default() {
        // `curl -XPOST .../graduate` with no body means graduate, because that is
        // what §9.3 names the endpoint after.
        let body: GraduateBody = serde_json::from_str("{}").unwrap();
        assert!(body.graduated);
        assert!(GraduateBody::default().graduated);
    }

    #[test]
    fn graduation_can_be_reversed_explicitly() {
        let body: GraduateBody = serde_json::from_str(r#"{"graduated": false}"#).unwrap();
        assert!(!body.graduated);
    }

    #[test]
    fn an_allowance_of_null_is_distinct_from_an_absent_field() {
        // `null` clears the override. An absent field is a malformed request,
        // because "I forgot to say" and "set it to nothing" must not be the same
        // request for something that can zero a ceiling.
        let cleared: AllowanceBody =
            serde_json::from_str(r#"{"domain_group": "google", "allowance": null}"#).unwrap();
        assert_eq!(cleared.allowance.0, None);

        assert!(serde_json::from_str::<AllowanceBody>(r#"{"domain_group": "google"}"#).is_err());
    }

    #[test]
    fn an_unknown_field_in_a_body_is_refused() {
        // deny_unknown_fields everywhere, as the §4.1 schema does: a typo in
        // `domain_group` must not silently apply an override to the wrong thing.
        assert!(serde_json::from_str::<AllowanceBody>(
            r#"{"domain_grp": "google", "allowance": 1}"#
        )
        .is_err());
        assert!(serde_json::from_str::<ResetBody>(
            r#"{"route":"a","domain_group":"b","confirm":"reset","force":true}"#
        )
        .is_err());
    }

    #[test]
    fn the_reset_confirmation_is_a_word_rather_than_a_flag() {
        let body: ResetBody = serde_json::from_str(
            r#"{"route":"warming","domain_group":"google","confirm":"reset"}"#,
        )
        .unwrap();
        assert_eq!(body.confirm, RESET_CONFIRMATION);
    }
}
