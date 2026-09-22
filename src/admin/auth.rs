//! §9.3's bearer token, and O-11's answer to whose token it was.
//!
//! §9.3: "All require the bearer token. All mutations are logged at `INFO` with
//! the acting token's identifier." The second sentence is what O-11 was about —
//! §4.1's `admin.auth_token` is one scalar and a scalar has no identifier. D-053
//! settles it by naming tokens: `auth_token` keeps working and is the token
//! named `default`, and `admin.tokens` lets an operator issue one per human or
//! per system so the audit line says something.
//!
//! **What requires a token here is wider than §9.3 asks for.** `/health`,
//! `/healthcheck` and `/metrics` are open — an orchestrator's probe and a
//! Prometheus scrape cannot carry a credential without configuration that
//! usually is not there, and a blind dashboard is its own outage. Everything
//! else, including §9.2's reads and §9.4's dry run, needs the token: `/routes`
//! discloses every downstream provider hostname and the whole routing shape, and
//! `/dryrun` runs the real engine over attacker-chosen input. §12.2 assumes the
//! admin listener is on a trusted interface but nothing enforces that, and the
//! cost of being wrong is asymmetric. See D-055.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use subtle::ConstantTimeEq;

use super::error::ApiError;
use crate::config::Admin;
use crate::metrics;

/// Who made a request, for §9.3's audit line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The configured name of the token presented — never the token.
    pub name: String,
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Match a presented token against every configured credential.
///
/// Constant-time, and it does not stop at the first match. `==` on a byte slice
/// returns as soon as it finds a difference, which turns a shared secret into
/// something an attacker can extract one byte at a time; §5.3 already takes this
/// seriously for passwords and there is no reason an admin token that can pause
/// a route deserves less. Length is still observable — `ct_eq` on slices of
/// different lengths cannot be otherwise — and that is accepted: it leaks how
/// long the secret is, not what it is.
pub fn identify(admin: &Admin, presented: &str) -> Option<Actor> {
    let mut matched: Option<&str> = None;

    for (name, token) in admin.credentials() {
        let hit: bool = token.as_bytes().ct_eq(presented.as_bytes()).into();
        if hit && matched.is_none() {
            matched = Some(name);
        }
    }

    matched.map(|name| Actor {
        name: name.to_string(),
    })
}

/// `Authorization: Bearer <token>`, per §9.3.
fn bearer(parts: &Parts) -> Result<&str, &'static str> {
    let header = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or("missing")?;
    let value = header.to_str().map_err(|_| "malformed")?;
    let (scheme, token) = value.split_once(' ').ok_or("malformed")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err("malformed");
    }
    let token = token.trim();
    if token.is_empty() {
        return Err("malformed");
    }
    Ok(token)
}

impl FromRequestParts<super::AdminState> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &super::AdminState,
    ) -> Result<Self, Self::Rejection> {
        let admin = &state.engine.config.admin;

        let reason = match bearer(parts) {
            Ok(token) => match identify(admin, token) {
                Some(actor) => return Ok(actor),
                None => "invalid",
            },
            Err(reason) => reason,
        };

        // The path but never the token, and never a hint about which part of it
        // was wrong. A run of these is credential guessing and is worth alerting
        // on, which is what the counter is for.
        tracing::warn!(
            path = %parts.uri.path(),
            reason,
            "admin request rejected"
        );
        metrics::admin_auth_failure(reason);

        Err(ApiError::unauthorised())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdminToken;

    fn admin(auth_token: Option<&str>, tokens: &[(&str, &str)]) -> Admin {
        Admin {
            listen: "127.0.0.1:8080".to_string(),
            auth_token: auth_token.map(str::to_string),
            metrics: None,
            tokens: tokens
                .iter()
                .map(|(name, token)| AdminToken {
                    name: name.to_string(),
                    token: token.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_scalar_token_is_the_actor_named_default() {
        // D-053: `auth_token` keeps working exactly as §4.1 specifies, and the
        // audit line gains a name without the operator doing anything.
        let a = admin(Some("s3cret-token-value"), &[]);
        assert_eq!(
            identify(&a, "s3cret-token-value").map(|a| a.name),
            Some("default".to_string())
        );
    }

    #[test]
    fn a_named_token_identifies_itself() {
        let a = admin(None, &[("oncall", "aaaa"), ("deploybot", "bbbb")]);
        assert_eq!(
            identify(&a, "bbbb").map(|a| a.name),
            Some("deploybot".into())
        );
        assert_eq!(identify(&a, "aaaa").map(|a| a.name), Some("oncall".into()));
    }

    #[test]
    fn both_spellings_coexist() {
        let a = admin(Some("shared"), &[("oncall", "personal")]);
        assert_eq!(
            identify(&a, "shared").map(|a| a.name),
            Some("default".into())
        );
        assert_eq!(
            identify(&a, "personal").map(|a| a.name),
            Some("oncall".into())
        );
    }

    #[test]
    fn an_unknown_token_identifies_nobody() {
        let a = admin(Some("right"), &[("oncall", "also-right")]);
        assert_eq!(identify(&a, "wrong"), None);
        assert_eq!(identify(&a, ""), None);
        // A prefix of a valid token must not pass, which is the thing a
        // length-insensitive comparison would get wrong.
        assert_eq!(identify(&a, "righ"), None);
        assert_eq!(identify(&a, "rightright"), None);
    }

    #[test]
    fn no_credential_configured_admits_nobody() {
        // §4.2 refuses to start in this state (see `check_admin_tokens`), so this
        // is belt and braces — but "no token configured" must never mean "no
        // token required", which is the direction that mistake usually goes.
        let a = admin(None, &[]);
        assert_eq!(identify(&a, ""), None);
        assert_eq!(identify(&a, "anything"), None);
    }

    // -- header parsing ----------------------------------------------------

    fn parts_with(header: Option<&str>) -> Parts {
        let mut builder = axum::http::Request::builder().uri("/routes");
        if let Some(value) = header {
            builder = builder.header(axum::http::header::AUTHORIZATION, value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive_and_the_token_is_not() {
        assert_eq!(bearer(&parts_with(Some("Bearer abc"))), Ok("abc"));
        assert_eq!(bearer(&parts_with(Some("bearer abc"))), Ok("abc"));
        assert_eq!(bearer(&parts_with(Some("BEARER abc"))), Ok("abc"));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(bearer(&parts_with(Some("Bearer   abc  "))), Ok("abc"));
    }

    #[test]
    fn anything_that_is_not_a_bearer_token_is_malformed() {
        assert_eq!(bearer(&parts_with(None)), Err("missing"));
        assert_eq!(bearer(&parts_with(Some("abc"))), Err("malformed"));
        assert_eq!(bearer(&parts_with(Some("Basic abc"))), Err("malformed"));
        assert_eq!(bearer(&parts_with(Some("Bearer "))), Err("malformed"));
        assert_eq!(bearer(&parts_with(Some("Bearer"))), Err("malformed"));
    }
}
