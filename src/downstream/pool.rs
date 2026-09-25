//! §8.3 — the per-route downstream connection pool.
//!
//! "Per-route pool with `max_connections`, `idle_ttl`, and
//! `max_messages_per_connection`. A connection is validated with `NOOP` before
//! reuse if idle beyond a short threshold, and discarded on any protocol error
//! rather than returned to the pool. `RSET` between messages on a reused
//! connection. The pool bounds concurrency against each downstream."
//!
//! That last sentence is the one that decides the shape. A cache of sockets would
//! satisfy the first three clauses and none of the fourth: with a cache, a burst
//! of two hundred concurrent sessions opens two hundred connections and simply
//! fails to reuse them. So `max_connections` is a **semaphore**, held for the
//! whole time a connection is checked out, and the count of live connections to a
//! route can never exceed it. A session that cannot get a permit within the
//! route's connect budget is answered `451` ([`RelayError::PoolExhausted`]) —
//! §14.1's rule holds here as everywhere: a saturated pool is Simmer's problem
//! and must never be recorded against a recipient.
//!
//! The invariant, since it is not obvious from the code: a connection exists only
//! while a permit is held (checked out) or while it sits idle, and one is opened
//! only when the idle set is empty. Returning a connection swaps one for the
//! other and never both, so `active + idle <= max_connections` always holds.
//!
//! What is deliberately *not* here: any retry loop, any queue, any persistence.
//! `relay` retries exactly once and only against a stale-connection error — see
//! the comment there, which is where §10.2 constrains this file.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::client::{Budget, Connection};
use super::outcome::RelayError;
use super::stream::TlsConfigs;
use crate::config::{Config, Route};

/// §8.3's "short threshold" for revalidating an idle connection with `NOOP`.
///
/// A constant rather than configuration, on D-063's precedent: §4.1 defines no
/// key for it. Five seconds is chosen against what it costs to be wrong in each
/// direction — too low is a wasted round trip on a connection that was fine, too
/// high is a message spending its whole conversation discovering the socket is
/// dead — and the second is much the more expensive, because it is paid on the
/// latency path of a client that is holding a connection open.
const VALIDATE_AFTER: Duration = Duration::from_secs(5);

/// Every route's pool, built once at startup.
pub struct Pool {
    /// Seeded from the configuration so that `/routes` can report a pool for a
    /// route nothing has yet sent through — an operator asking "how many
    /// connections is this allowed" deserves an answer before the first message,
    /// not `null`.
    ///
    /// Still fills lazily on a miss. The map and `Engine.config` come from the
    /// same `Config`, so a miss is unreachable in the service; making it
    /// *impossible* rather than merely unreachable costs one `RwLock` and removes
    /// a branch that could only ever have been handled by lying to a client.
    /// Keyed `(ramp, route)`: pools are per route *within its ramp* (D-099), so
    /// two ramps pointing at one downstream each hold their own bound.
    routes: RwLock<HashMap<(String, String), Arc<RoutePool>>>,
}

impl Pool {
    pub fn build(cfg: &Config) -> Pool {
        let routes = cfg
            .all_routes()
            .map(|r| {
                (
                    (r.ramp.clone(), r.name.clone()),
                    Arc::new(RoutePool::new(r)),
                )
            })
            .collect();
        Pool {
            routes: RwLock::new(routes),
        }
    }

    pub fn for_route(&self, route: &Route) -> Arc<RoutePool> {
        if let Some(pool) = self
            .routes
            .read()
            .expect("not poisoned")
            .get(&(route.ramp.clone(), route.name.clone()))
            .cloned()
        {
            return pool;
        }

        Arc::clone(
            self.routes
                .write()
                .expect("not poisoned")
                .entry((route.ramp.clone(), route.name.clone()))
                .or_insert_with(|| Arc::new(RoutePool::new(route))),
        )
    }

    /// §9.2's "pool statistics", for one route.
    pub fn stats(&self, ramp: &str, route: &str) -> Option<PoolStats> {
        self.routes
            .read()
            .expect("not poisoned")
            .get(&(ramp.to_string(), route.to_string()))
            .map(|p| p.stats())
    }

    /// §10.4 — "drain pools".
    ///
    /// Runs after in-flight sessions have finished, so everything left is idle by
    /// construction. Each connection gets a `QUIT` so the downstream sees an
    /// orderly close rather than counting a reset against us, and the semaphore is
    /// closed so that anything still holding a route pool cannot open a new one.
    pub async fn drain(&self) {
        let pools: Vec<Arc<RoutePool>> = self
            .routes
            .read()
            .expect("not poisoned")
            .values()
            .cloned()
            .collect();

        for pool in pools {
            let closed = pool.drain().await;
            if closed > 0 {
                tracing::info!(route = %pool.route, connections = closed, "drained pooled connections");
            }
        }
    }
}

/// One route's pool.
pub struct RoutePool {
    route: String,
    max_connections: usize,
    idle_ttl: Duration,
    max_messages: u64,
    budget: Budget,

    permits: Arc<Semaphore>,
    idle: Mutex<Vec<Idle>>,

    /// Checked out right now. Not derivable from the semaphore — `available_permits`
    /// counts permits, and a checkout holds one across a `reopen`.
    active: AtomicUsize,
    opened: AtomicU64,
    reused: AtomicU64,
    /// Closed for having reached `max_messages_per_connection` or `idle_ttl`.
    retired: AtomicU64,
    /// Closed because it broke: a failed `NOOP`, a failed `RSET`, or a
    /// conversation that ended in a state we cannot describe.
    discarded: AtomicU64,
}

struct Idle {
    conn: Connection,
    since: Instant,
    /// Messages already delivered over this connection.
    messages: u64,
}

impl RoutePool {
    fn new(route: &Route) -> RoutePool {
        let cfg = &route.downstream.pool;
        RoutePool {
            route: route.name.clone(),
            max_connections: cfg.max_connections,
            idle_ttl: cfg.idle_ttl,
            max_messages: cfg.max_messages_per_connection,
            budget: Budget::for_route(route),
            permits: Arc::new(Semaphore::new(cfg.max_connections)),
            idle: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            opened: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            retired: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        }
    }

    pub(super) fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Take a connection, reusing an idle one where there is a sound one to reuse.
    pub(super) async fn checkout(
        self: &Arc<Self>,
        route: &Route,
        tls: &TlsConfigs,
        hostname: &str,
    ) -> Result<Checkout, RelayError> {
        // §8.4 gives no budget for "waiting for a peer of yours to finish", and
        // the connect budget is the closest thing with the right meaning: it is
        // what this route's operator declared they are willing to spend getting a
        // connection. Bounded either way, because the alternative is holding a
        // client connection past the client's own timeout, which turns our
        // saturation into their ambiguity.
        let permit = match tokio::time::timeout(
            self.budget.connect,
            Arc::clone(&self.permits).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            // Timed out waiting, or the pool was closed by a §10.4 drain. Both
            // mean the same thing to the message in hand.
            Err(_) | Ok(Err(_)) => return Err(RelayError::PoolExhausted),
        };

        while let Some(idle) = self.take_idle() {
            let age = idle.since.elapsed();
            if age >= self.idle_ttl {
                self.retired.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let mut conn = idle.conn;
            if age >= VALIDATE_AFTER {
                if let Err(e) = conn.noop(&self.budget).await {
                    tracing::debug!(
                        route = %self.route,
                        error = ?e,
                        "idle connection failed validation; discarding"
                    );
                    self.discarded.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }

            self.reused.fetch_add(1, Ordering::Relaxed);
            return Ok(self.checked_out(permit, conn, idle.messages, true));
        }

        let conn = Connection::open(route, tls, hostname, &self.budget).await?;
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(self.checked_out(permit, conn, 0, false))
    }

    fn checked_out(
        self: &Arc<Self>,
        permit: OwnedSemaphorePermit,
        conn: Connection,
        messages: u64,
        reused: bool,
    ) -> Checkout {
        self.active.fetch_add(1, Ordering::Relaxed);
        Checkout {
            pool: Arc::clone(self),
            _permit: permit,
            conn: Some(conn),
            messages,
            reused,
        }
    }

    fn take_idle(&self) -> Option<Idle> {
        // Last in, first out: the most recently returned connection is the one
        // least likely to have been closed at the far end while we were not
        // looking, and reusing it keeps the rest of the set ageing towards
        // `idle_ttl` instead of cycling all of them just below it.
        self.idle.lock().expect("not poisoned").pop()
    }

    fn stats(&self) -> PoolStats {
        PoolStats {
            max_connections: self.max_connections,
            idle: self.idle.lock().expect("not poisoned").len(),
            active: self.active.load(Ordering::Relaxed),
            opened: self.opened.load(Ordering::Relaxed),
            reused: self.reused.load(Ordering::Relaxed),
            retired: self.retired.load(Ordering::Relaxed),
            discarded: self.discarded.load(Ordering::Relaxed),
        }
    }

    /// Close every idle connection. Returns how many there were.
    async fn drain(&self) -> usize {
        self.permits.close();
        let idle: Vec<Idle> = std::mem::take(&mut *self.idle.lock().expect("not poisoned"));
        let count = idle.len();
        for mut entry in idle {
            let _ = entry.conn.quit(&self.budget).await;
        }
        self.retired.fetch_add(count as u64, Ordering::Relaxed);
        count
    }
}

/// A connection borrowed from the pool for the length of one message.
pub(super) struct Checkout {
    pool: Arc<RoutePool>,
    /// Held, never read: dropping it is what returns the slot (§8.3's bound).
    _permit: OwnedSemaphorePermit,
    /// `None` only between [`Checkout::reopen`]'s discard and its replacement,
    /// and after [`Checkout::release`] has taken it.
    conn: Option<Connection>,
    messages: u64,
    reused: bool,
}

impl Checkout {
    pub(super) fn conn(&mut self) -> &mut Connection {
        self.conn
            .as_mut()
            .expect("a checkout holds a connection until it is released")
    }

    pub(super) fn reused(&self) -> bool {
        self.reused
    }

    /// Throw this connection away and open a fresh one **under the same permit**,
    /// so the retry cannot push the route past `max_connections`.
    pub(super) async fn reopen(
        &mut self,
        route: &Route,
        tls: &TlsConfigs,
        hostname: &str,
    ) -> Result<(), RelayError> {
        // Dropped rather than QUIT: it has already failed to answer once, and
        // spending another command budget asking it to say goodbye delays a
        // message that is still waiting to be sent.
        self.conn = None;
        self.pool.discarded.fetch_add(1, Ordering::Relaxed);

        self.conn = Some(Connection::open(route, tls, hostname, &self.pool.budget).await?);
        self.pool.opened.fetch_add(1, Ordering::Relaxed);
        self.messages = 0;
        self.reused = false;
        Ok(())
    }

    /// Give the connection back, or close it. `reusable` is `client::reusable`'s
    /// verdict on how the conversation ended.
    pub(super) async fn release(mut self, reusable: bool) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let messages = self.messages + 1;

        if !reusable {
            self.pool.discarded.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // §8.3's `max_messages_per_connection`. Retiring on the way back rather
        // than on the way out means the ceiling is a count of messages actually
        // delivered over the connection, which is what a provider enforcing one
        // is counting too.
        if messages >= self.pool.max_messages {
            let _ = conn.quit(&self.pool.budget).await;
            self.pool.retired.fetch_add(1, Ordering::Relaxed);
            return;
        }

        if let Err(e) = conn.rset(&self.pool.budget).await {
            tracing::debug!(
                route = %self.pool.route,
                error = ?e,
                "RSET failed on return to the pool; discarding"
            );
            self.pool.discarded.fetch_add(1, Ordering::Relaxed);
            return;
        }

        self.pool.idle.lock().expect("not poisoned").push(Idle {
            conn,
            since: Instant::now(),
            messages,
        });
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // Every exit runs through here — `release`, an error return, a panic —
        // so this is the only place `active` is decremented.
        self.pool.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// §9.2's "pool statistics" for one route.
///
/// The three lifetime counters are what distinguish a pool that is working from
/// one that merely looks idle: `reused` far below `opened` means connections are
/// not surviving between messages, and a climbing `discarded` means the
/// downstream is closing them underneath us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PoolStats {
    pub max_connections: usize,
    pub idle: usize,
    pub active: usize,
    pub opened: u64,
    pub reused: u64,
    pub retired: u64,
    pub discarded: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pool: &str) -> Config {
        let yaml = format!(
            r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: simmer.test
  max_message_bytes: 1000
  max_recipients: 1
  max_concurrent_sessions: 1
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ allow_insecure_auth: true }}
database: {{ url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }}
admin: {{ listen: "127.0.0.1:0", auth_token: "t" }}
logging: {{ level: warn, format: text }}
default_ramp: main
ramps:
 main:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "oldbrand.com", match_on: envelope, chain: [only] }}
  default_chain: [only]
  routes:
  - name: only
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {pool}
    identity: {{ envelope_from: "b@newbrand.com" }}
"#
        );
        crate::config::from_str(&yaml, "pool-test").expect("fixture is valid")
    }

    fn route(pool: &str) -> Route {
        config(pool)
            .default_ramp()
            .routes
            .first()
            .cloned()
            .expect("one route")
    }

    #[test]
    fn a_pool_is_built_for_every_configured_route() {
        let cfg = config("{ max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }");

        let pool = Pool::build(&cfg);
        let stats = pool
            .stats("main", "only")
            .expect("a pool for the configured route");
        assert_eq!(stats.max_connections, 4);
        assert_eq!(stats.idle, 0);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.opened, 0);
    }

    #[test]
    fn a_route_the_map_never_saw_gets_a_pool_rather_than_no_answer() {
        let cfg = config("{ max_connections: 2, idle_ttl: 60s, max_messages_per_connection: 10 }");

        let pool = Pool {
            routes: RwLock::new(HashMap::new()),
        };
        assert!(pool.stats("main", "only").is_none(), "nothing seeded yet");

        let r = cfg.default_ramp().routes.first().expect("one route");
        let first = pool.for_route(r);
        let second = pool.for_route(r);
        assert!(
            Arc::ptr_eq(&first, &second),
            "the second call must reuse the pool the first created, or two \
             callers get two independent bounds on one downstream"
        );
        assert_eq!(
            pool.stats("main", "only")
                .expect("now seeded")
                .max_connections,
            2
        );
    }

    #[tokio::test]
    async fn checkout_is_bounded_by_max_connections() {
        // The bound has to hold without a downstream to connect to, so this
        // drives the semaphore directly: two permits, three waiters.
        let r = route("{ max_connections: 2, idle_ttl: 60s, max_messages_per_connection: 10 }");
        let pool = Arc::new(RoutePool::new(&r));

        let a = Arc::clone(&pool.permits).acquire_owned().await.expect("a");
        let b = Arc::clone(&pool.permits).acquire_owned().await.expect("b");
        assert_eq!(pool.permits.available_permits(), 0);

        let waited = tokio::time::timeout(
            Duration::from_millis(50),
            Arc::clone(&pool.permits).acquire_owned(),
        )
        .await;
        assert!(waited.is_err(), "the third must wait for one of the two");

        drop(a);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                Arc::clone(&pool.permits).acquire_owned()
            )
            .await
            .is_ok(),
            "and be admitted the moment one is returned"
        );
        drop(b);
    }

    #[tokio::test]
    async fn an_exhausted_pool_is_pool_exhausted_and_not_a_connect_failure() {
        // §14.1's shape: the distinction is only worth carrying if it survives to
        // the outcome table, where it becomes a 451 of its own class.
        let mut r = route("{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }");
        // Nothing is listening, but the permit runs out first, which is the point:
        // an exhausted pool must not be reported as the downstream being down.
        r.downstream.timeouts = Some(crate::config::DownstreamTimeouts {
            connect: Some(Duration::from_millis(50)),
            command: None,
            data: None,
        });
        let pool = Arc::new(RoutePool::new(&r));
        let held = Arc::clone(&pool.permits)
            .acquire_owned()
            .await
            .expect("held");

        let (tls, _) = TlsConfigs::load().expect("tls");
        let err = pool
            .checkout(&r, &tls, "simmer.test")
            .await
            .err()
            .expect("must fail");
        assert!(
            matches!(err, RelayError::PoolExhausted),
            "expected PoolExhausted, got {err:?}"
        );
        drop(held);
    }

    #[tokio::test]
    async fn draining_an_empty_pool_closes_it_to_new_connections() {
        let r = route("{ max_connections: 2, idle_ttl: 60s, max_messages_per_connection: 10 }");
        let pool = Arc::new(RoutePool::new(&r));
        assert_eq!(pool.drain().await, 0);

        let (tls, _) = TlsConfigs::load().expect("tls");
        assert!(
            matches!(
                pool.checkout(&r, &tls, "simmer.test").await,
                Err(RelayError::PoolExhausted)
            ),
            "a drained pool must not open a new connection during shutdown"
        );
    }
}
