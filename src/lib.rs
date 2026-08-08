//! `simmer` — an SMTP relay facade that applies a domain reputation warm-up ramp.
//!
//! See `docs/SPEC.md`. The constraint that shapes everything is the cutover
//! invariant (§1.1): Simmer is temporary infrastructure, so its output must
//! always be exactly expressible as application-side configuration, and it must
//! never write permanent state into the systems around it.
//!
//! The crate is split into a library and a thin `server` binary so the
//! logic-heavy parts — config validation, sender matching, and later the rewrite
//! engine and reply mapping — are reachable from integration tests without
//! spawning a process.

pub mod admin;
pub mod config;
pub mod db;
pub mod downstream;
pub mod logging;
pub mod metrics;
pub mod models;
pub mod quota;
pub mod relay;
pub mod routing;
pub mod smtp;
