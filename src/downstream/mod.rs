//! §8 the outbound leg: TLS, the SMTP conversation, and the §10.1 reply mapping.
//!
//! Phase 2 opened one connection per message and closed it (`DECISIONS.md`
//! D-019). Phase 10 adds §8.3's pool, which wraps the conversation rather than
//! changing it: [`client::relay`] checks a connection out instead of dialling,
//! and the pool is what bounds concurrency against each downstream.

pub mod client;
pub mod outcome;
pub mod pool;
pub mod stream;

pub use client::{relay, Message};
pub use outcome::{Delivered, Outcome, RelayError, Stage};
pub use pool::{Pool, PoolStats};
pub use stream::TlsConfigs;
