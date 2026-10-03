//! §11 for the spool (D-116): the storage contract both backends implement.
//!
//! [`SpoolStore`] extends [`QuotaStore`] rather than standing beside it because
//! of one method: [`SpoolStore::commit_and_complete`] is §7.4's commit and the
//! row's deletion **in one transaction**, which only the store holding the quota
//! rows can give.
//!
//! Every write a lease holder makes is fenced by the `lease_token` it was given
//! at claim time. A holder whose lease expired under it — a long GC pause, a
//! partitioned instance — finds its token gone and its write refused, so it
//! cannot overwrite the decision of the instance that claimed the row next.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::quota::{QuotaError, QuotaStore, Reservation};

/// A message being accepted: the row [`SpoolStore::enqueue`] inserts once its
/// body is durable (D-117's ordering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSpooled {
    pub id: Uuid,
    pub ramp: String,
    pub domain_group: String,
    /// How the group was chosen (§3.2 step 2, D-100): `literal`, `mx:<host>`, …
    pub group_basis: String,
    pub received_at: DateTime<Utc>,
    /// `min(received_at + max_hold, the day boundary)` (Q4).
    pub expires_at: DateTime<Utc>,
    pub next_attempt_at: DateTime<Utc>,
    /// The envelope and session facts, as JSON the store never looks inside.
    pub envelope: String,
    pub body_ref: String,
    pub body_bytes: i64,
    pub body_sha256: Vec<u8>,
    /// One `{{uuid}}` per message, so every attempt rewrites identically.
    pub uuid_seed: Uuid,
}

/// D-118 — a rate slot booked for a deferred attempt, kept on the row so the
/// attempt uses it rather than booking again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookedSlot {
    pub route: String,
    pub domain_group: String,
    pub tat: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    /// Who holds the lease, for diagnosis only; the token is what fences.
    pub owner: String,
    /// The caller's instant (D-108).
    pub now: DateTime<Utc>,
    pub batch: u32,
    pub lease: chrono::Duration,
}

/// A row this instance now holds a lease on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: Uuid,
    pub ramp: String,
    pub domain_group: String,
    pub group_basis: String,
    pub received_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// When it became due; claims come back in this order.
    pub next_attempt_at: DateTime<Utc>,
    pub envelope: String,
    /// `None` only if the body was deleted under a live row, which no path
    /// does; the dispatcher dead-letters such a row rather than guessing.
    pub body_ref: Option<String>,
    pub body_bytes: i64,
    pub body_sha256: Vec<u8>,
    pub uuid_seed: Uuid,
    /// Downstream attempts so far.
    pub attempts: i64,
    /// Q3 — the route of the first downstream attempt.
    pub pinned_route: Option<String>,
    pub booked: Option<BookedSlot>,
    pub lease_token: Uuid,
    pub lease_until: DateTime<Utc>,
}

/// Put a held row back to wait, fenced by its token.
#[derive(Debug, Clone)]
pub struct Reschedule {
    pub id: Uuid,
    pub token: Uuid,
    pub next_attempt_at: DateTime<Utc>,
    /// Whether a downstream attempt was made. A requeue for an exhausted pool
    /// or a shutdown is not an attempt.
    pub attempted: bool,
    /// Written when set; an existing pin is never cleared by a reschedule.
    pub pinned_route: Option<String>,
    /// Replaces whatever booking the row had.
    pub booked: Option<BookedSlot>,
    pub last_code: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    /// A downstream `5xx`.
    Rejected,
    /// The hold ran out (Q4).
    Expired,
    /// The body was missing or did not match its digest: nothing to deliver.
    Corrupt,
}

impl DeadReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Corrupt => "corrupt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            "corrupt" => Some(Self::Corrupt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeadLetterRequest {
    pub id: Uuid,
    pub token: Uuid,
    pub reason: DeadReason,
    pub at: DateTime<Utc>,
    pub attempted: bool,
    pub pinned_route: Option<String>,
    pub last_code: Option<i64>,
    pub last_error: Option<String>,
    /// D-121: keep `body_ref` so the entry stays retryable. Otherwise it is
    /// cleared in the same statement and the caller deletes the body.
    pub keep_body: bool,
}

/// A dead letter as §9's read API shows it: no addresses (D-120).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadEntry {
    pub id: Uuid,
    pub ramp: String,
    pub domain_group: String,
    pub route: Option<String>,
    pub reason: Option<DeadReason>,
    pub last_code: Option<i64>,
    pub last_error: Option<String>,
    pub attempts: i64,
    pub received_at: DateTime<Utc>,
    pub dead_at: Option<DateTime<Utc>>,
    pub body_retained: bool,
}

/// What [`SpoolStore::retry_dead`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDead {
    Requeued,
    /// The body has gone (D-121), so there is nothing to send.
    NoBody,
    NotFound,
}

/// The admission bounds' inputs (D-119): live messages, and every byte the
/// body store holds for a row — a retained dead letter's included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpoolTotals {
    pub messages: i64,
    pub bytes: i64,
}

/// One `(ramp, domain_group)` lane's live messages, for §9.2 and the metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStats {
    pub ramp: String,
    pub domain_group: String,
    pub depth: i64,
    pub bytes: i64,
    pub oldest_received_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// §9.3 for the spool. Absent rows read as the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpoolRampState {
    /// Nothing in this ramp is claimed.
    pub paused: bool,
    /// Nothing new is accepted into this ramp.
    pub draining: bool,
}

#[async_trait]
pub trait SpoolStore: QuotaStore {
    /// Insert an accepted message as `queued`. The body is already durable.
    async fn enqueue(&self, msg: &NewSpooled) -> Result<(), QuotaError>;

    /// Lease up to `batch` due rows in one short transaction, skipping rows
    /// another claimant holds the lock on and every row of a paused ramp. Due
    /// means `next_attempt_at <= now` and either `queued`, or `leased` with a
    /// lease that has run out. Each claim gets a fresh token.
    async fn claim_due(&self, req: &ClaimRequest) -> Result<Vec<Claimed>, QuotaError>;

    /// Extend a lease. False if the token no longer holds it.
    async fn renew_lease(
        &self,
        id: Uuid,
        token: Uuid,
        until: DateTime<Utc>,
    ) -> Result<bool, QuotaError>;

    /// Back to `queued`, fenced. False if the token no longer holds it.
    async fn reschedule(&self, r: &Reschedule) -> Result<bool, QuotaError>;

    /// To `dead`, fenced. False if the token no longer holds it.
    async fn dead_letter(&self, d: &DeadLetterRequest) -> Result<bool, QuotaError>;

    /// §7.4's commit — `reserved` to `committed` and §7.3's events — and the
    /// row's deletion, in **one transaction**.
    ///
    /// The quota is committed whether or not the token still holds the row:
    /// the downstream said `2xx`, and a delivered message is counted, exactly
    /// as [`QuotaStore::commit`] counts one whose reservation was swept. The
    /// row is deleted either way too — delivery is the one verdict no later
    /// holder can improve on, and deleting it stops a holder that has not yet
    /// sent from sending a duplicate. Returns whether the token still held it,
    /// so the caller can count a lost lease.
    async fn commit_and_complete(
        &self,
        reservation: &Reservation,
        recipient_keys: &[crate::frequency::Key],
        id: Uuid,
        token: Uuid,
    ) -> Result<bool, QuotaError>;

    /// Clear `body_ref` on dead letters that died before `cutoff` and still
    /// name a body, returning the refs for the caller to delete (D-121).
    async fn take_dead_bodies(&self, cutoff: DateTime<Utc>) -> Result<Vec<String>, QuotaError>;

    /// Delete dead letters that died before `cutoff`, returning any body refs
    /// they still named.
    async fn purge_dead(&self, cutoff: DateTime<Utc>) -> Result<Vec<String>, QuotaError>;

    /// Which of `refs` a row still names, for the orphan sweeper (D-117).
    async fn known_body_refs(
        &self,
        refs: &[String],
    ) -> Result<std::collections::HashSet<String>, QuotaError>;

    async fn totals(&self) -> Result<SpoolTotals, QuotaError>;

    /// Live messages in one lane (D-119's forecast).
    async fn lane_depth(&self, ramp: &str, domain_group: &str) -> Result<i64, QuotaError>;

    /// Every lane with a live message.
    async fn lanes(&self) -> Result<Vec<LaneStats>, QuotaError>;

    /// Dead letters, newest first.
    async fn dead_entries(&self, limit: u32) -> Result<Vec<DeadEntry>, QuotaError>;

    /// Requeue a dead letter whose body is retained, due at `now` and expiring
    /// at `expires_at`. Its pin is kept (Q3); its booking is not.
    async fn retry_dead(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<RetryDead, QuotaError>;

    /// §9.3 `DELETE /spool/{id}`: remove a row in any state, returning the
    /// body ref it named (`Some(None)` for a row with no body).
    async fn delete_message(&self, id: Uuid) -> Result<Option<Option<String>>, QuotaError>;

    async fn set_spool_paused(&self, ramp: &str, paused: bool) -> Result<(), QuotaError>;
    async fn set_spool_draining(&self, ramp: &str, draining: bool) -> Result<(), QuotaError>;
    async fn spool_states(
        &self,
    ) -> Result<std::collections::HashMap<String, SpoolRampState>, QuotaError>;
}
