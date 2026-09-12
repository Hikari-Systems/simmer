//! The compose-backed test tiers' shared harness — acceptance today, and the end-to-end
//! matrix, stress, soak and fault-injection tiers as they land (`docs/TESTING.md`).
//!
//! `tests/support/` is the in-process harness: a fake downstream and a Simmer
//! built in the test's own process. This is the other half — Simmer in the
//! container it ships in, driven from outside through `docker compose`, and
//! judged only by what can be observed from outside: replies, mail servers,
//! Postgres and `/metrics`.
//!
//! - [`stack`] — one compose stack, and every command against it.
//! - [`traps`] — reading Mailpit.
//! - [`mail`] — parsing what arrived.
//! - [`findings`] — `test/known-findings.json`, XFAIL and XPASS for these tiers.
//! - [`reconcile`] — no loss and no duplicates, between what clients were told and
//!   what downstreams received.
//! - [`leak`] — whether a resource series is growing.

#![allow(dead_code)] // each tier uses a different subset

pub mod findings;
pub mod leak;
pub mod mail;
pub mod reconcile;
pub mod stack;
pub mod traps;
