//! §5 ingress: the listeners, and the §5.2 state machine they drive.
//!
//! Since D-070 there is one listener per configured port, each with its own TLS
//! and AUTH [`Policy`], sharing one `max_concurrent_sessions` bound and one
//! `allowed_cidrs` check — the limits are about the process, not the port.

pub mod acl;
pub mod auth;
pub mod buffer;
pub mod command;
pub mod reply;
pub mod session;
pub mod tls;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use ipnet::IpNet;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tracing::Instrument;

use crate::config::{IngressAuth, IngressTls};
use crate::downstream::stream::Stream;
use crate::relay::Engine;
use crate::{metrics, smtp::acl::Acl, smtp::auth::Verifier, smtp::auth::VerifyLimit};

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

/// What a session needs to know about the port it arrived on (D-070).
#[derive(Clone)]
pub struct Policy {
    /// The listener's configured address, as written — what the capture
    /// records and what §9.4's dry run is asked about (D-099).
    pub address: String,
    pub tls: IngressTls,
    pub auth: IngressAuth,
    /// §5.8 (D-099) — the listener's port affinity, and whether a permitted
    /// `X-Simmer-Ramp` header may override it.
    pub ramp: Option<String>,
    pub header_overrides_affinity: bool,
    /// Present exactly when `tls` is not `off`: §4.2 refuses a TLS listener
    /// without a certificate, so a session never has to wonder.
    pub acceptor: Option<TlsAcceptor>,
}

impl Policy {
    /// Whether `STARTTLS` is on offer here at all. Never on an implicit-TLS
    /// port: RFC 8314 §3.3 forbids advertising it there.
    pub fn offers_starttls(&self) -> bool {
        matches!(
            self.tls,
            IngressTls::Starttls | IngressTls::StarttlsRequired
        )
    }
}

/// State every session shares, whichever port it arrived on.
struct Shared {
    engine: Engine,
    verifier: Arc<Verifier>,
    acl: Arc<Acl>,
    allowed: Vec<IpNet>,
    /// §5.1 `max_concurrent_sessions`. A permit is held for the whole session,
    /// and one pool serves every listener.
    sessions: Arc<Semaphore>,
    /// D-079 — the bound on concurrent argon2 verifications. See
    /// [`auth::VerifyLimit`] for why it exists.
    verifies: VerifyLimit,
}

/// D-079's bound: twice the cores this process may use, and never fewer than 4.
///
/// argon2 is CPU-bound, so more in flight than the CPUs can work on buys memory
/// and nothing else; twice leaves room for one to be scheduled while another
/// finishes. `available_parallelism` honours the cgroup quota, so a 2-CPU
/// container bounds itself at 4 verifies (~76 MiB) and a 16-CPU host at 32.
fn max_concurrent_verifies() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cores * 2).max(4)
}

/// The SMTP listeners (§5.1).
pub struct Listener {
    bound: Vec<(TcpListener, Arc<Policy>)>,
    shared: Arc<Shared>,
    certificate: Option<tls::Loaded>,
}

impl Listener {
    /// Bind every listener and prepare. Separate from [`Listener::serve`] so
    /// `main` can report a bind failure before announcing itself as started — a
    /// port clash on 587 is a startup failure, not a service that is deaf on one
    /// port.
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

        // §4.2 has already loaded this once, so a failure here means the file
        // changed between validation and now. Refusing to start is still right.
        let certificate = match &cfg.server.tls {
            Some(t) => Some(tls::load(t).map_err(|problems| {
                let detail: Vec<String> = problems
                    .iter()
                    .map(|p| format!("{}: {}", p.path, p.message))
                    .collect();
                anyhow::anyhow!("loading the TLS certificate: {}", detail.join("; "))
            })?),
            None => None,
        };

        let mut bound = Vec::with_capacity(cfg.server.listeners.len());
        for l in &cfg.server.listeners {
            let listener = TcpListener::bind(&l.address)
                .await
                .map_err(|e| anyhow::anyhow!("binding SMTP listener {}: {e}", l.address))?;
            let tls = l.tls_mode();
            let policy = Policy {
                address: l.address.clone(),
                tls,
                auth: l.auth_mode(),
                ramp: l.ramp.clone(),
                header_overrides_affinity: l.header_overrides_affinity,
                acceptor: tls
                    .can_encrypt()
                    .then(|| certificate.as_ref().map(|c| c.acceptor.clone()))
                    .flatten(),
            };
            if tls.can_encrypt() && policy.acceptor.is_none() {
                // Unreachable through `config::load`, which refuses a TLS
                // listener with no certificate. A listener that advertised
                // STARTTLS and could not perform it would be worse than none.
                anyhow::bail!(
                    "listener {} has tls: {} but no certificate",
                    l.address,
                    tls.as_str()
                );
            }
            bound.push((listener, Arc::new(policy)));
        }

        let verifies = max_concurrent_verifies();
        tracing::info!(
            max_concurrent_verifies = verifies,
            "argon2 verification bound (D-079)"
        );
        let shared = Shared {
            verifier: Arc::new(Verifier::new(&cfg.server.auth)),
            acl: Arc::new(Acl::new(&cfg.server.auth)),
            sessions: Arc::new(Semaphore::new(cfg.server.max_concurrent_sessions)),
            verifies: VerifyLimit::new(verifies),
            allowed,
            engine,
        };

        Ok(Self {
            bound,
            shared: Arc::new(shared),
            certificate,
        })
    }

    /// The first listener's address — the only one, in every configuration
    /// before D-070.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.bound
            .first()
            .ok_or_else(|| std::io::Error::other("no listeners"))?
            .0
            .local_addr()
    }

    /// Every listener, in configuration order, with its effective policy.
    pub fn local_addrs(&self) -> std::io::Result<Vec<(SocketAddr, IngressTls, IngressAuth)>> {
        self.bound
            .iter()
            .map(|(l, p)| Ok((l.local_addr()?, p.tls, p.auth)))
            .collect()
    }

    /// The loaded certificate, so startup can log its expiry and name coverage.
    pub fn certificate(&self) -> Option<&tls::Loaded> {
        self.certificate.as_ref()
    }

    /// A handle for waiting on in-flight sessions during §10.4 shutdown.
    pub fn sessions(&self) -> Arc<Semaphore> {
        Arc::clone(&self.shared.sessions)
    }

    /// Accept on every listener until `stop_accepting` fires.
    ///
    /// §10.4 is two-phase and both phases are needed: `stop_accepting` breaks the
    /// accept loops, and `hard_stop` — fired by the caller once the grace period
    /// has elapsed — is what makes a session still running at that point emit
    /// `421` rather than being cut off mid-reply.
    pub async fn serve(self, stop_accepting: Shutdown, hard_stop: Shutdown) {
        let loops: Vec<_> = self
            .bound
            .into_iter()
            .map(|(listener, policy)| {
                tokio::spawn(accept_loop(
                    listener,
                    policy,
                    Arc::clone(&self.shared),
                    stop_accepting.clone(),
                    hard_stop.clone(),
                ))
            })
            .collect();
        for l in loops {
            let _ = l.await;
        }

        tracing::info!("SMTP listeners stopped accepting");
    }
}

async fn accept_loop(
    listener: TcpListener,
    policy: Arc<Policy>,
    shared: Arc<Shared>,
    stop_accepting: Shutdown,
    hard_stop: Shutdown,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = stop_accepting.cancelled() => break,
            a = listener.accept() => a,
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

        let policy = Arc::clone(&policy);
        let shared = Arc::clone(&shared);
        let hard_stop = hard_stop.clone();
        // §9.6 (D-101) — one span per connection, refused ones included: a
        // CIDR or session-bound refusal is a connection that happened. The
        // transaction spans are its children.
        let span = tracing::info_span!(
            "smtp.session",
            otel.name = "smtp.session",
            otel.kind = "server",
            client.address = %peer.ip(),
            client.port = peer.port(),
            listener = %policy.address,
            tls = policy.tls.as_str(),
            username = tracing::field::Empty,
            end = tracing::field::Empty,
        );
        tokio::spawn(
            async move {
                handle(stream, peer, policy, shared, hard_stop).await;
            }
            .instrument(span),
        );
    }
}

async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    policy: Arc<Policy>,
    shared: Arc<Shared>,
    hard_stop: Shutdown,
) {
    // On an implicit-TLS port the first bytes belong to a TLS handshake, so a
    // plaintext refusal would arrive as garbage in the client's ClientHello
    // response. §5.1 permits a bare TCP close, and that is the honest answer
    // there.
    let implicit = policy.tls == IngressTls::Implicit;

    // §5.1 — the CIDR check comes before anything else, including the permit, so
    // a disallowed peer cannot consume a session slot.
    if !is_allowed(peer.ip(), &shared.allowed) {
        tracing::warn!(peer = %peer, "connection from outside allowed_cidrs");
        metrics::connection_refused("cidr");
        if !implicit {
            let _ = write_and_close(&mut stream, &reply::access_denied()).await;
        }
        return;
    }

    // §5.1 — "reply 421 4.3.2 too many connections".
    let Ok(_permit) = Arc::clone(&shared.sessions).try_acquire_owned() else {
        tracing::warn!(peer = %peer, "refused: max_concurrent_sessions reached");
        metrics::connection_refused("max_sessions");
        if !implicit {
            let _ = write_and_close(&mut stream, &reply::too_many_connections()).await;
        }
        return;
    };

    let _ = stream.set_nodelay(true);
    let cfg = &shared.engine.config;

    // RFC 8314 — TLS from the first byte. Inside the permit, so a handshake
    // counts against max_concurrent_sessions like any other session, and bounded
    // by the per-command budget, so a peer that opens a socket and sends nothing
    // holds a slot for no longer than an idle plaintext one would.
    let stream = if implicit {
        let acceptor = policy.acceptor.clone().expect("Listener::bind checked");
        match tokio::time::timeout(cfg.server.timeouts.command, acceptor.accept(stream)).await {
            Ok(Ok(tls)) => Stream::Tls(Box::new(tls.into())),
            Ok(Err(e)) => {
                tracing::info!(peer = %peer, error = %e, "implicit TLS handshake failed");
                metrics::inbound_tls_failure("implicit", "handshake");
                return;
            }
            Err(_) => {
                tracing::info!(peer = %peer, "implicit TLS handshake timed out");
                metrics::inbound_tls_failure("implicit", "handshake");
                return;
            }
        }
    } else {
        Stream::Plain(stream)
    };

    let mut session = session::Session::new(
        stream,
        peer,
        shared.engine.clone(),
        Arc::clone(&shared.verifier),
        Arc::clone(&shared.acl),
        shared.verifies.clone(),
        policy,
    );

    let end = tokio::select! {
        // §8.4 / §4.1 `timeouts.session` is enforced inside `run`, at every wait
        // on the client and never mid-relay (D-081). It was a timer here, and
        // when it fired during a relay it dropped the relay future: the client
        // was told `421` for a message the downstream had stored, and the
        // reservation was never resolved (F2).
        end = session.run() => end,

        // §10.4 — "Sessions exceeding the grace period receive 421 and are
        // closed." Cutting the socket instead would leave a client unable to
        // tell a refusal from a network fault, and it would retry either way;
        // the 421 at least says which.
        _ = hard_stop.cancelled() => {
            session.refuse(&reply::shutting_down()).await;
            session::SessionEnd::ShuttingDown
        }
    };

    session.close().await;
    tracing::Span::current().record("end", tracing::field::debug(end));
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
        // §2.3: TLS and AUTH do not make Simmer a public MX. An `auth: optional`
        // listener lets an unauthenticated client send as anyone this admits.
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
