//! §5 ingress: the listener, and the §5.2 state machine it drives.

pub mod auth;
pub mod buffer;
pub mod command;
pub mod reply;
pub mod session;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use ipnet::IpNet;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::relay::Engine;
use crate::{metrics, smtp::auth::Verifier};

/// A minimal cancellation primitive.
///
/// `tokio-util` would provide `CancellationToken`, but it is a dependency taken
/// for one type, and §10.4's needs are met by a watch channel.
mod tokio_util_shim {
    #[derive(Clone)]
    pub struct CancellationToken(tokio::sync::watch::Sender<bool>);

    impl CancellationToken {
        pub fn new() -> Self {
            Self(tokio::sync::watch::channel(false).0)
        }

        pub fn cancel(&self) {
            let _ = self.0.send(true);
        }

        pub fn is_cancelled(&self) -> bool {
            *self.0.borrow()
        }

        /// Resolves when cancelled. Safe to call repeatedly.
        pub async fn cancelled(&self) {
            let mut rx = self.0.subscribe();
            if *rx.borrow() {
                return;
            }
            let _ = rx.changed().await;
        }
    }

    impl Default for CancellationToken {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub use tokio_util_shim::CancellationToken as Shutdown;

/// The SMTP listener (§5.1).
pub struct Listener {
    listener: TcpListener,
    engine: Engine,
    verifier: Arc<Verifier>,
    allowed: Arc<Vec<IpNet>>,
    /// §5.1 `max_concurrent_sessions`. A permit is held for the whole session.
    sessions: Arc<Semaphore>,
}

impl Listener {
    /// Bind and prepare. Separate from [`Listener::serve`] so `main` can report a
    /// bind failure before announcing itself as started.
    pub async fn bind(engine: Engine) -> anyhow::Result<Self> {
        let cfg = &engine.config;

        let allowed: Vec<IpNet> = cfg
            .server
            .allowed_cidrs
            .iter()
            // §4.2 has already validated every entry parses; an unparseable one
            // cannot reach here.
            .filter_map(|c| c.parse().ok())
            .collect();

        let listener = TcpListener::bind(&cfg.server.listen)
            .await
            .map_err(|e| anyhow::anyhow!("binding SMTP listener {}: {e}", cfg.server.listen))?;

        let verifier = Arc::new(Verifier::new(&cfg.server.auth));
        let sessions = Arc::new(Semaphore::new(cfg.server.max_concurrent_sessions));

        Ok(Self {
            listener,
            engine,
            verifier,
            allowed: Arc::new(allowed),
            sessions,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// A handle for waiting on in-flight sessions during §10.4 shutdown.
    pub fn sessions(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sessions)
    }

    /// Accept until `stop_accepting` fires.
    ///
    /// §10.4 is two-phase and both phases are needed: `stop_accepting` breaks the
    /// accept loop, and `hard_stop` — fired by the caller once the grace period
    /// has elapsed — is what makes a session still running at that point emit
    /// `421` rather than being cut off mid-reply.
    pub async fn serve(self, stop_accepting: Shutdown, hard_stop: Shutdown) {
        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop_accepting.cancelled() => break,
                a = self.listener.accept() => a,
            };

            let (stream, peer) = match accepted {
                Ok(v) => v,
                Err(e) => {
                    // A per-connection accept error (EMFILE, a peer that
                    // vanished) must not take the listener down.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::task::yield_now().await;
                    continue;
                }
            };

            let engine = self.engine.clone();
            let verifier = Arc::clone(&self.verifier);
            let allowed = Arc::clone(&self.allowed);
            let sessions = Arc::clone(&self.sessions);
            let hard_stop = hard_stop.clone();

            tokio::spawn(async move {
                handle(stream, peer, engine, verifier, allowed, sessions, hard_stop).await;
            });
        }

        tracing::info!("SMTP listener stopped accepting");
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    engine: Engine,
    verifier: Arc<Verifier>,
    allowed: Arc<Vec<IpNet>>,
    sessions: Arc<Semaphore>,
    hard_stop: Shutdown,
) {
    // §5.1 — the CIDR check comes before anything else, including the permit, so
    // a disallowed peer cannot consume a session slot.
    if !is_allowed(peer.ip(), &allowed) {
        tracing::warn!(peer = %peer, "connection from outside allowed_cidrs");
        metrics::connection_refused("cidr");
        let _ = write_and_close(&mut stream, &reply::access_denied()).await;
        return;
    }

    // §5.1 — "reply 421 4.3.2 too many connections".
    let Ok(_permit) = Arc::clone(&sessions).try_acquire_owned() else {
        tracing::warn!(peer = %peer, "refused: max_concurrent_sessions reached");
        metrics::connection_refused("max_sessions");
        let _ = write_and_close(&mut stream, &reply::too_many_connections()).await;
        return;
    };

    let _ = stream.set_nodelay(true);

    let session_timeout = engine.config.server.timeouts.session;
    let mut session = session::Session::new(stream, peer, engine, verifier);

    let end = tokio::select! {
        end = session.run() => end,

        // §8.4 / §4.1 `timeouts.session` — a hard ceiling on the whole
        // conversation, independent of the per-command budget. Without it a
        // client that sends NOOP every 29 seconds holds a slot forever.
        _ = tokio::time::sleep(session_timeout) => {
            session.refuse(&reply::session_timeout()).await;
            session::SessionEnd::SessionTimeout
        }

        // §10.4 — "Sessions exceeding the grace period receive 421 and are
        // closed." Cutting the socket instead would leave a client unable to
        // tell a refusal from a network fault, and it would retry either way;
        // the 421 at least says which.
        _ = hard_stop.cancelled() => {
            session.refuse(&reply::shutting_down()).await;
            session::SessionEnd::ShuttingDown
        }
    };

    tracing::debug!(peer = %peer, ?end, "session ended");
}

fn is_allowed(ip: IpAddr, allowed: &[IpNet]) -> bool {
    allowed.iter().any(|net| net.contains(&ip))
}

async fn write_and_close(stream: &mut TcpStream, reply: &reply::Reply) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(reply.to_wire().as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn allows_addresses_inside_the_configured_blocks() {
        let allowed = nets(&["10.0.0.0/8", "172.16.0.0/12"]);
        assert!(is_allowed("10.1.2.3".parse().unwrap(), &allowed));
        assert!(is_allowed("172.16.0.1".parse().unwrap(), &allowed));
        assert!(is_allowed("172.31.255.255".parse().unwrap(), &allowed));
    }

    #[test]
    fn refuses_addresses_outside_them() {
        // §2.3: the listener is plaintext and accepts plaintext AUTH, so this is
        // the only thing standing between it and an untrusted network.
        let allowed = nets(&["10.0.0.0/8", "172.16.0.0/12"]);
        assert!(!is_allowed("192.168.1.1".parse().unwrap(), &allowed));
        assert!(!is_allowed("172.32.0.1".parse().unwrap(), &allowed));
        assert!(!is_allowed("8.8.8.8".parse().unwrap(), &allowed));
        assert!(!is_allowed("127.0.0.1".parse().unwrap(), &allowed));
    }

    #[test]
    fn an_empty_allow_list_refuses_everything() {
        // §4.2 (D-015) rejects an empty list at startup precisely because this
        // is what it would mean.
        assert!(!is_allowed("10.0.0.1".parse().unwrap(), &[]));
    }

    #[test]
    fn ipv6_blocks_work_and_do_not_match_ipv4() {
        let allowed = nets(&["fd00::/8"]);
        assert!(is_allowed("fd00::1".parse().unwrap(), &allowed));
        assert!(!is_allowed("10.0.0.1".parse().unwrap(), &allowed));
    }

    #[tokio::test]
    async fn a_shutdown_token_resolves_once_cancelled() {
        let t = Shutdown::new();
        assert!(!t.is_cancelled());
        let t2 = t.clone();
        tokio::spawn(async move { t2.cancel() });
        t.cancelled().await;
        assert!(t.is_cancelled());
        // Already-cancelled tokens must resolve immediately, not hang.
        t.cancelled().await;
    }
}
