//! §8 the outbound leg: TLS, the SMTP conversation, and the §10.1 reply mapping.
//!
//! Phase 2 opens one connection per message and closes it (`DECISIONS.md`
//! D-019). §8.3's pool is a phase 10 refinement and wraps [`client::relay`]
//! without changing it.

pub mod client;
pub mod outcome;
pub mod stream;

pub use client::{relay, Message};
pub use outcome::{Delivered, Outcome, RelayError, Stage};
pub use stream::TlsConfigs;
