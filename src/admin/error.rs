//! One error shape for the whole §9 control plane.
//!
//! Every failure leaves as `{"error": "...", "message": "..."}` with a machine
//! -readable code and a sentence for a human, because the audience for this API
//! is an operator at a terminal and a script written by the same person.
//!
//! The one thing worth thinking about: a storage failure is `503`, not `500`.
//! §7.5 makes an unreachable database a *temporary* condition that the message
//! path answers `451`, and the control plane has no business describing the same
//! condition as a permanent server error — a health check or a retry loop reads
//! the difference.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::quota::QuotaError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// A stable slug. Safe to match on.
    pub code: &'static str,
    pub message: String,
    /// `WWW-Authenticate`, which RFC 9110 requires on a `401`.
    challenge: bool,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            challenge: false,
        }
    }

    pub fn unauthorised() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorised",
            // Deliberately identical for a missing, malformed and wrong token.
            // The distinction is in the log, where it is useful, rather than in
            // the response, where it is a hint.
            message: "a valid bearer token is required (§9.3)".to_string(),
            challenge: true,
        }
    }

    pub fn not_found(what: &str, name: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no {what} named '{name}' is configured"),
        )
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
}

/// §7.5's posture, in HTTP. The quota store being unreachable is exactly as
/// temporary here as it is on the message path.
impl From<QuotaError> for ApiError {
    fn from(e: QuotaError) -> Self {
        tracing::error!(error = %e, "admin request failed against the quota store");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            // The store's own message, which names the failure. No credentials
            // reach it: `Database`'s Debug redacts the URL and sqlx errors carry
            // the statement, not the connection string.
            format!("quota storage is unavailable: {e}"),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response();

        if self.challenge {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"simmer\""),
            );
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unauthorised_response_carries_a_challenge_and_says_nothing_useful() {
        let response = ApiError::unauthorised().into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
    }

    #[test]
    fn a_storage_failure_is_temporary_not_a_server_error() {
        // §7.5: the same condition the message path answers 451.
        let e: ApiError = QuotaError::Storage("connection refused".into()).into();
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.code, "storage_unavailable");
    }
}
