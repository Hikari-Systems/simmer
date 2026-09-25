//! §11 — the storage trait the quota model sits behind.
//!
//! "The storage layer sits behind a trait so the concrete backend can be
//! substituted, but no alternative backend is implemented in v1." `PgQuotaStore`
//! was that one implementation until D-084 added `MssqlQuotaStore` for the
//! `-mssql` image; each build compiles exactly one. The trait also lets the
//! reservation protocol be reasoned about — and tested — without a database in
//! the way.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A reservation that has been taken and not yet resolved.
///
/// Holding one is a promise to call [`QuotaStore::commit`] or
/// [`QuotaStore::release`] exactly once. §7.4 has no third outcome; the sweeper
/// exists only for the case where the process dies before either happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: Uuid,
    /// D-099: every key is `(ramp, route, …)`. Two ramps may each have a route
    /// of the same name, and nothing they persist is shared.
    pub ramp: String,
    pub route: String,
    pub domain_group: String,
    pub day_index: i64,
    pub count: i64,
}

/// What one `(ramp, route, domain_group, day_index)` row says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// `None` is *no ceiling* — an overflow route (D-024).
    pub allowance: Option<i64>,
    pub allowance_override: Option<i64>,
    pub committed: i64,
    pub reserved: i64,
}

impl Usage {
    /// §9.3's override wins where it is set.
    pub fn effective_allowance(&self) -> Option<i64> {
        self.allowance_override.or(self.allowance)
    }

    /// How much more this row can take. `None` is unlimited.
    pub fn headroom(&self) -> Option<i64> {
        self.effective_allowance()
            .map(|a| (a - self.committed - self.reserved).max(0))
    }

    pub fn has_headroom_for(&self, count: i64) -> bool {
        self.headroom().is_none_or(|h| h >= count)
    }
}

/// A `(route, domain_group, day_index)` address within one ramp, for the §9.2
/// read API. The ramp is [`QuotaStore::usage_many`]'s own argument: every read
/// view is of one ramp, and keeping it out of the key keeps the answer keyed by
/// `(route, domain_group)` as before.
///
/// The day index is part of the key rather than a parameter because it is
/// per-route: a warming route's day begins on its own `warmup.started`
/// anniversary and an overflow route's on UTC midnight (D-024), so one request
/// asking about every route is asking about several different days.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UsageKey {
    pub route: String,
    pub domain_group: String,
    pub day_index: i64,
}

/// What §9.3's `POST /quota/reset` did, so the response and the audit line can
/// say what was destroyed rather than only that something was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reset {
    pub committed_before: i64,
    pub reserved_before: i64,
    /// `reserved` is **recomputed from the live reservation rows**, not zeroed.
    /// A reset during a send would otherwise hand away headroom that an
    /// in-flight message already owns, and the double-spend would only surface
    /// as an overshoot of the day's ceiling — which is the one thing this
    /// service exists to prevent.
    pub reserved_after: i64,
}

/// §9.3 route-scoped admin state. Absent rows read as the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteState {
    pub paused: bool,
    pub graduated: bool,
}

/// Everything a reservation attempt needs to know.
#[derive(Debug, Clone)]
pub struct ReserveRequest {
    /// D-099 — the ramp the message is being routed in.
    pub ramp: String,
    pub route: String,
    pub domain_group: String,
    pub day_index: i64,
    /// The ceiling to write **if the row does not yet exist**. Ignored when it
    /// does, because §7.4's row is authoritative once created (D-026).
    pub allowance: Option<i64>,
    /// §3.2: "decremented per message, by the recipient count, not per
    /// recipient" — one reservation of this magnitude (O-7).
    pub count: i64,
    pub correlation_id: String,
    pub expires_at: DateTime<Utc>,
    /// §3.2 step 2a (D-090): take the reservation **without** the headroom
    /// check. Everything else is unchanged — the row lock is taken, `reserved`
    /// is incremented, and commit or release resolves it — so the send is
    /// counted and contenders are still serialised; `committed` may simply end
    /// above `allowance`. Set only for a thread-affinity reply on its pinned
    /// route, and only once an ordinary reservation has been refused.
    pub over_cap: bool,
}

/// The outcome of §7.4 phase 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    Taken(Reservation),
    /// No headroom. The chain walk moves to the next route (§3.2 step 3c).
    NoHeadroom {
        usage: Usage,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    /// Any database failure. §7.5 turns this into `451 4.3.0 quota service
    /// unavailable` under `fail_closed`, which is the default — "a quota
    /// enforcer that stops enforcing under failure provides no guarantee at
    /// all".
    #[error("quota storage: {0}")]
    Storage(String),
    /// D-099: `adopt_legacy_rows` found a pre-ramps key that already exists
    /// under the target ramp. Moving it would merge two histories, so startup
    /// stops and says which.
    #[error("adopting pre-ramps state: {0}")]
    LegacyConflict(String),
}

#[cfg(feature = "postgres")]
impl From<sqlx::Error> for QuotaError {
    fn from(e: sqlx::Error) -> Self {
        QuotaError::Storage(e.to_string())
    }
}

/// What a reservation was worth when it was swept away unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expired {
    pub ramp: String,
    pub route: String,
    pub count: i64,
}

/// What [`QuotaStore::adopt_legacy_rows`] moved into the default ramp (D-099).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adoption {
    pub quota_usage: u64,
    pub quota_reservation: u64,
    pub route_state: u64,
    pub recipient_event: u64,
    /// Every distinct route name among the moved rows. The caller warns about
    /// any that the ramp does not configure: `route_state` rows outlive deleted
    /// routes, so that is worth a line, not a refusal.
    pub routes: Vec<String>,
}

impl Adoption {
    pub fn total(&self) -> u64 {
        self.quota_usage + self.quota_reservation + self.route_state + self.recipient_event
    }
}

#[async_trait]
pub trait QuotaStore: Send + Sync + 'static {
    /// §7.4 phase 1 — reserve, in one transaction, under a row lock.
    async fn reserve(&self, req: &ReserveRequest) -> Result<Reserved, QuotaError>;

    /// §7.4 phase 3 — downstream said `2xx`: move the count from `reserved` to
    /// `committed`, **and record recipient-frequency events**.
    ///
    /// The second half is §7.4's own wording, and it is one method rather than
    /// two because it is one transaction. A delivered message whose event was not
    /// recorded would under-count somebody's window silently; a recorded event
    /// for a message that did not commit would over-count it. Neither is
    /// reachable if they cannot be separated.
    ///
    /// `recipient_keys` is empty for a route that declares no
    /// `recipient_frequency` — §7.3's reason for hashing is that the container
    /// does not accumulate a record of every address mailed, and rows nothing
    /// will ever read are the opposite of that.
    async fn commit(
        &self,
        reservation: &Reservation,
        recipient_keys: &[crate::frequency::Key],
    ) -> Result<(), QuotaError>;

    /// §7.4 phase 3 — anything else: give the headroom back.
    async fn release(&self, reservation: &Reservation) -> Result<(), QuotaError>;

    /// Read a row without locking it, for the §5.4 early eligibility check and
    /// for §9.2.
    async fn usage(
        &self,
        ramp: &str,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Usage, QuotaError>;

    /// §9.2 — several rows in one query, for the read API.
    ///
    /// Keyed by `(route, domain_group)` on the way out: the day index is an
    /// input, and a route only ever has one *current* day.
    async fn usage_many(
        &self,
        ramp: &str,
        keys: &[UsageKey],
    ) -> Result<std::collections::HashMap<(String, String), Usage>, QuotaError>;

    /// §9.3 `POST /routes/{name}/pause` and `/resume`.
    async fn set_paused(&self, ramp: &str, route: &str, paused: bool) -> Result<(), QuotaError>;

    /// §9.3 `POST /routes/{name}/graduate` — pin to the final schedule value.
    async fn set_graduated(
        &self,
        ramp: &str,
        route: &str,
        graduated: bool,
    ) -> Result<(), QuotaError>;

    /// §9.3 `POST /routes/{name}/allowance`. `allowance: None` clears the
    /// override; `scheduled` is what to write into `allowance` if the row does
    /// not exist yet, so that clearing an override on a fresh row leaves the
    /// schedule's own number behind rather than a null (D-025).
    async fn set_allowance_override(
        &self,
        ramp: &str,
        route: &str,
        domain_group: &str,
        day_index: i64,
        allowance: Option<i64>,
        scheduled: Option<i64>,
    ) -> Result<(), QuotaError>;

    /// §9.3 `POST /quota/reset`. `None` means there was no row to reset.
    async fn reset_counters(
        &self,
        ramp: &str,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Option<Reset>, QuotaError>;

    /// §9.3 state for every route of one ramp, in one query. Read per message rather than
    /// cached: at warm-up volumes the query costs nothing, and a cache would
    /// mean `POST /routes/{name}/pause` did not take effect immediately, which
    /// is the one thing an operator reaching for it needs.
    async fn route_states(
        &self,
        ramp: &str,
    ) -> Result<std::collections::HashMap<String, RouteState>, QuotaError>;

    /// §7.4 — release reservations past their expiry. Returns what was released
    /// so the caller can log and count it.
    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError>;

    /// §7.3 — how many events this route has recorded for this recipient inside
    /// the window. Unlocked, and outside the reservation transaction (D-049).
    async fn recipient_event_count(
        &self,
        ramp: &str,
        route: &str,
        key: &crate::frequency::Key,
        since: DateTime<Utc>,
    ) -> Result<i64, QuotaError>;

    /// §7.3's persisted salt, minting one if this is a fresh instance.
    ///
    /// On the store rather than in `main` because §7.5 says an unreachable
    /// database is not a startup failure: the salt has to be obtainable later,
    /// on the message path, and a failure here has to become the same `451` any
    /// other storage failure does.
    async fn recipient_hash_salt(&self) -> Result<Vec<u8>, QuotaError>;

    /// §7.3's sweeper — evict events older than `cutoff`. Returns how many.
    async fn sweep_recipient_events(&self, cutoff: DateTime<Utc>) -> Result<u64, QuotaError>;

    /// D-099 — move every pre-ramps row (`ramp = ''`, which the migration
    /// wrote) into `ramp`, in one transaction under a lock that serialises
    /// replicas starting together. Idempotent: a second call finds nothing.
    /// Refuses with [`QuotaError::LegacyConflict`] if a legacy
    /// `quota_usage` or `route_state` key already exists under `ramp`.
    async fn adopt_legacy_rows(&self, ramp: &str) -> Result<Adoption, QuotaError>;

    /// Is the backing store reachable? §7.5.
    async fn is_available(&self) -> bool;
}
