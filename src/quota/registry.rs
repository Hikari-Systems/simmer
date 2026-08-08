//! §10.4 — the reservations this process is holding right now.
//!
//! "On `SIGTERM`: … release any reservations still outstanding."
//!
//! A reservation is normally resolved by the relay path, which always commits or
//! releases. This registry covers the one case that path cannot: a session
//! cancelled *during* the downstream conversation, when the §10.4 grace period
//! expires. `Drop` cannot help — releasing is an `await` — so the handles are
//! registered here and released explicitly at shutdown.
//!
//! Scoped to this process's own reservations rather than truncating the table,
//! per `DECISIONS.md` D-007: §2.2 says one instance owns its quota state, but
//! the shapes that keep that from being *baked in* cost nothing now and are
//! expensive to retrofit.
//!
//! §7.4's sweeper is still the backstop, for the case where the process does not
//! get to run its shutdown path at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use super::store::Reservation;

#[derive(Clone, Default)]
pub struct ReservationRegistry {
    inner: Arc<Mutex<HashMap<Uuid, Reservation>>>,
}

impl ReservationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, reservation: &Reservation) {
        self.lock().insert(reservation.id, reservation.clone());
    }

    pub fn remove(&self, id: Uuid) {
        self.lock().remove(&id);
    }

    /// Everything still outstanding, and empty the registry.
    ///
    /// Draining rather than reading means a concurrent resolution racing this
    /// call cannot cause a double release: whichever wins, the loser finds
    /// nothing to do, and the store's own `take_reservation` is the final
    /// arbiter anyway.
    pub fn drain(&self) -> Vec<Reservation> {
        self.lock().drain().map(|(_, r)| r).collect()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock here means an earlier panic, not corrupt state — the map
    /// is only ever inserted into and removed from.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Reservation>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(n: i64) -> Reservation {
        Reservation {
            id: Uuid::new_v4(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: 0,
            count: n,
        }
    }

    #[test]
    fn tracks_and_forgets_reservations() {
        let reg = ReservationRegistry::new();
        assert!(reg.is_empty());

        let a = reservation(1);
        let b = reservation(2);
        reg.insert(&a);
        reg.insert(&b);
        assert_eq!(reg.len(), 2);

        reg.remove(a.id);
        assert_eq!(reg.len(), 1);

        let left = reg.drain();
        assert_eq!(left, vec![b]);
        assert!(reg.is_empty(), "draining empties the registry");
    }

    #[test]
    fn draining_twice_yields_nothing_the_second_time() {
        // The property that stops a shutdown racing a normal resolution from
        // releasing the same reservation twice.
        let reg = ReservationRegistry::new();
        reg.insert(&reservation(1));
        assert_eq!(reg.drain().len(), 1);
        assert_eq!(reg.drain().len(), 0);
    }

    #[test]
    fn removing_an_unknown_id_is_harmless() {
        let reg = ReservationRegistry::new();
        reg.remove(Uuid::new_v4());
        assert!(reg.is_empty());
    }

    #[test]
    fn is_shared_across_clones() {
        let a = ReservationRegistry::new();
        let b = a.clone();
        b.insert(&reservation(1));
        assert_eq!(a.len(), 1, "clones must share one map, not copy it");
    }
}
