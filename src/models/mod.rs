//! Storage queries, organised per the hikari-systems data-service pattern:
//! `models/<entity>.rs` holding free functions over `&PgPool`, runtime `sqlx`
//! rather than `query_as!`, and no business logic.
//!
//! The reservation protocol that composes these into transactions lives in
//! [`crate::quota::postgres`], behind §11's storage trait.

pub mod instance_config;
pub mod legacy;
pub mod quota;
pub mod recipient_event;
pub mod route_rate;
pub mod route_state;
