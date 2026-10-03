//! §7.7 (D-116) — the opt-in spool.
//!
//! A ramp with `delivery: spool` answers its client `250 queued` once the
//! message is durably stored, and a dispatcher delivers it later through the
//! same walk, rewrite and relay a synchronous ramp uses. With no ramp opting
//! in, nothing here is constructed: no table is read, no body store opened, no
//! task started.
//!
//! Three things keep this from becoming the MTA `CLAUDE.md` rule #1 forbids
//! for everyone else:
//!
//! - **It is opt-in per ramp.** A synchronous ramp never reaches this module.
//! - **It never composes mail.** A message that cannot be delivered becomes a
//!   dead letter (D-120): a row, a metric and an optional webhook. No DSN.
//! - **It is not the capture**, and never reads it (rule #8, D-085). The
//!   capture is written before the relay precisely so that it cannot know the
//!   outcome; the spool exists to know it.

pub mod store;

pub use store::{
    BookedSlot, ClaimRequest, Claimed, DeadEntry, DeadLetterRequest, DeadReason, LaneStats,
    NewSpooled, Reschedule, RetryDead, SpoolRampState, SpoolStore, SpoolTotals,
};
