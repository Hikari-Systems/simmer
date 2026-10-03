//! Test harness: a scripted fake downstream, a Simmer under test, and a small
//! SMTP client to drive it.
//!
//! §12.3 asks for "a scripted fake downstream that can be made to return
//! arbitrary codes, stall, drop mid-`DATA`, and refuse TLS", against which the
//! §10.1 reply mapping is asserted exhaustively. That is what [`FakeDownstream`]
//! is.

#![allow(dead_code)] // each integration test file uses a different subset

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use simmer::downstream::stream::Stream;
use simmer::frequency::{Frequency, Key};
use simmer::quota::store::{
    Expired, QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, RouteState, Usage,
};
use simmer::quota::ReservationRegistry;
use simmer::relay::Engine;
use simmer::smtp;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// The scripted downstream
// ---------------------------------------------------------------------------

/// What the fake downstream should do at one stage of the conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Act {
    /// The normal positive reply for this stage.
    #[default]
    Ok,
    /// An arbitrary code and text — the §12.3 "arbitrary codes" requirement.
    Reply(u16, &'static str),
    /// Accept the command and never answer, so the client's stage timeout fires.
    Stall,
    /// Close the connection without answering.
    Drop,
}

/// How the fake downstream handles `STARTTLS` (§8.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tls {
    /// Do not advertise it at all.
    #[default]
    NotOffered,
    /// Advertise it, then refuse the command.
    Refuse,
    /// Advertise it, accept the command, then speak plaintext rubbish instead of
    /// completing a handshake.
    AcceptThenFail,
}

#[derive(Clone, Debug)]
pub struct Script {
    pub greeting: Act,
    pub ehlo: Act,
    pub auth: Act,
    pub mail_from: Act,
    pub rcpt_to: Act,
    pub data: Act,
    pub final_dot: Act,
    pub tls: Tls,
    /// Extra `EHLO` capability lines, beyond `PIPELINING`.
    pub caps: Vec<String>,
    /// Close the connection partway through reading the `DATA` payload.
    pub drop_mid_data: bool,
    /// Answer `RSET` and then close, which is what §8.3's pool has to survive: a
    /// downstream that reaps an idle connection between one message and the next.
    ///
    /// It closes *after* the reply, so the connection goes back into the pool
    /// looking healthy and is dead by the time the next message picks it up —
    /// precisely the case a `NOOP` on checkout cannot catch inside its threshold.
    pub close_after_rset: bool,
    /// Record the message, *then* wait this long before answering the final dot.
    ///
    /// The shape §10.2 is about: the downstream has the message and the reply is
    /// late. It also holds the connection busy, which is what the pool-bound tests
    /// need to make concurrency observable from this side.
    pub final_dot_delay: Option<Duration>,
    /// Per-transaction overrides, indexed by the transaction's position **on its
    /// own connection** (0 is the first `MAIL FROM` a connection sees). A field
    /// left `None` falls back to the connection-wide setting above.
    ///
    /// Per connection, not global, on purpose: it is what lets a test fail the
    /// second message on a *reused* connection while the fresh connection a
    /// retry would open behaves normally — the shape D-068's retry is about.
    pub transactions: Vec<Turn>,
}

/// One transaction's overrides; see [`Script::transactions`].
#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub mail_from: Option<Act>,
    pub rcpt_to: Option<Act>,
    pub data: Option<Act>,
    pub final_dot: Option<Act>,
    pub drop_mid_data: Option<bool>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            greeting: Act::Ok,
            ehlo: Act::Ok,
            auth: Act::Ok,
            mail_from: Act::Ok,
            rcpt_to: Act::Ok,
            data: Act::Ok,
            final_dot: Act::Ok,
            tls: Tls::NotOffered,
            caps: vec![
                "8BITMIME".into(),
                "SIZE 26214400".into(),
                "AUTH PLAIN".into(),
            ],
            drop_mid_data: false,
            close_after_rset: false,
            final_dot_delay: None,
            transactions: Vec::new(),
        }
    }
}

impl Script {
    pub fn with(mut f: impl FnMut(&mut Script)) -> Script {
        let mut s = Script::default();
        f(&mut s);
        s
    }
}

/// What the fake downstream saw.
#[derive(Clone, Debug, Default)]
pub struct Received {
    pub mail_from: Option<String>,
    pub recipients: Vec<String>,
    /// The message as it arrived, after un-stuffing.
    pub body: Vec<u8>,
    pub mail_from_params: String,
    pub ehlo_seen: bool,
    pub auth_seen: bool,
}

/// One command line as the downstream received it.
#[derive(Clone, Debug)]
pub struct Seen {
    /// When the line was read.
    pub at: std::time::Instant,
    /// Which accepted connection it arrived on, numbered from 0 in accept order.
    pub connection: usize,
    pub line: String,
}

pub struct FakeDownstream {
    pub addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
    /// TCP connections accepted, ever. §8.3's pool is only observable from the
    /// downstream's side as the difference between this and the message count.
    connections: Arc<Mutex<usize>>,
    /// Every command line, across every connection, in order, with when and
    /// on which connection it arrived.
    commands: Arc<Mutex<Vec<Seen>>>,
    /// Connections open right now, and the most ever open at once. §8.3's bound
    /// is a claim about *concurrent* connections, which the lifetime count above
    /// cannot see.
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl FakeDownstream {
    pub async fn start(script: Script) -> FakeDownstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
        let addr = listener.local_addr().expect("addr");
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0usize));
        let commands = Arc::new(Mutex::new(Vec::new()));

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let sink = Arc::clone(&received);
        let counter = Arc::clone(&connections);
        let log = Arc::clone(&commands);
        let (open, most) = (Arc::clone(&active), Arc::clone(&peak));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let id = {
                    let mut n = counter.lock().expect("not poisoned");
                    *n += 1;
                    *n - 1
                };
                let now = open.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                let script = script.clone();
                let sink = Arc::clone(&sink);
                let log = Arc::clone(&log);
                let open = Arc::clone(&open);
                tokio::spawn(async move {
                    let _ = serve_one(stream, id, script, sink, log).await;
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        FakeDownstream {
            addr,
            received,
            connections,
            commands,
            active,
            peak,
        }
    }

    /// The most connections this downstream has ever had open at once.
    pub fn peak_connections(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Connections open right now.
    pub fn open_connections(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// How many TCP connections have been accepted.
    pub fn connections(&self) -> usize {
        *self.connections.lock().expect("not poisoned")
    }

    /// Every command line seen, across every connection.
    pub fn commands(&self) -> Vec<String> {
        self.timed_commands().into_iter().map(|c| c.line).collect()
    }

    /// Every command line seen, with its arrival time and connection.
    pub fn timed_commands(&self) -> Vec<Seen> {
        self.commands.lock().expect("not poisoned").clone()
    }

    /// The command lines seen on one connection (0 is the first accepted).
    pub fn commands_on(&self, connection: usize) -> Vec<String> {
        self.timed_commands()
            .into_iter()
            .filter(|c| c.connection == connection)
            .map(|c| c.line)
            .collect()
    }

    pub fn command_count(&self, verb: &str) -> usize {
        self.commands()
            .iter()
            .filter(|c| c.to_ascii_uppercase().starts_with(verb))
            .count()
    }

    /// A downstream that is not listening at all, for the connect-failure row of
    /// §10.1. Binding and dropping guarantees the port is free but unowned.
    pub async fn unreachable() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        addr
    }

    pub fn messages(&self) -> Vec<Received> {
        self.received.lock().expect("not poisoned").clone()
    }

    pub fn last(&self) -> Option<Received> {
        self.messages().last().cloned()
    }
}

async fn serve_one(
    stream: TcpStream,
    connection: usize,
    script: Script,
    sink: Arc<Mutex<Vec<Received>>>,
    log: Arc<Mutex<Vec<Seen>>>,
) -> std::io::Result<()> {
    let mut io = BufReader::new(stream);
    let mut seen = Received::default();
    // Which transaction on this connection is in progress: bumped at each
    // `MAIL FROM`, so the first is 0. See `Script::transactions`.
    let mut transaction: Option<usize> = None;
    let turn = |t: Option<usize>| -> Turn {
        t.and_then(|i| script.transactions.get(i).cloned())
            .unwrap_or_default()
    };

    macro_rules! act {
        ($a:expr, $default:expr) => {
            match &$a {
                Act::Ok => write(&mut io, $default).await?,
                Act::Reply(code, text) => write(&mut io, &format!("{code} {text}\r\n")).await?,
                Act::Stall => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return Ok(());
                }
                Act::Drop => return Ok(()),
            }
        };
    }

    act!(script.greeting, "220 fake.downstream ESMTP\r\n");

    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let upper = line.to_ascii_uppercase();
        log.lock().expect("not poisoned").push(Seen {
            at: std::time::Instant::now(),
            connection,
            line: line.clone(),
        });

        if upper.starts_with("EHLO") {
            seen.ehlo_seen = true;
            match &script.ehlo {
                Act::Ok => {
                    let mut caps: Vec<String> = vec!["PIPELINING".into()];
                    caps.extend(script.caps.iter().cloned());
                    if script.tls != Tls::NotOffered {
                        caps.push("STARTTLS".into());
                    }
                    let mut out = String::from("250-fake.downstream\r\n");
                    for (i, c) in caps.iter().enumerate() {
                        let sep = if i + 1 == caps.len() { ' ' } else { '-' };
                        out.push_str(&format!("250{sep}{c}\r\n"));
                    }
                    write(&mut io, &out).await?;
                }
                other => act!(other, ""),
            }
        } else if upper.starts_with("HELO") {
            write(&mut io, "250 fake.downstream\r\n").await?;
        } else if upper.starts_with("STARTTLS") {
            match script.tls {
                Tls::Refuse | Tls::NotOffered => {
                    write(&mut io, "502 5.5.1 STARTTLS not available\r\n").await?;
                }
                Tls::AcceptThenFail => {
                    write(&mut io, "220 2.0.0 ready to start TLS\r\n").await?;
                    // Plaintext rubbish where a ClientHello response belongs.
                    write(&mut io, "this is not a TLS record at all\r\n").await?;
                    return Ok(());
                }
            }
        } else if upper.starts_with("AUTH") {
            seen.auth_seen = true;
            act!(script.auth, "235 2.7.0 authenticated\r\n");
        } else if upper.starts_with("MAIL FROM") {
            let inner = between(&line, '<', '>');
            seen.mail_from = inner.filter(|s| !s.is_empty());
            seen.mail_from_params = line
                .split_once('>')
                .map(|(_, t)| t.trim().to_string())
                .unwrap_or_default();
            transaction = Some(transaction.map_or(0, |t| t + 1));
            let a = turn(transaction)
                .mail_from
                .unwrap_or(script.mail_from.clone());
            act!(a, "250 2.1.0 sender ok\r\n");
        } else if upper.starts_with("RCPT TO") {
            if let Some(r) = between(&line, '<', '>') {
                seen.recipients.push(r);
            }
            let a = turn(transaction).rcpt_to.unwrap_or(script.rcpt_to.clone());
            act!(a, "250 2.1.5 recipient ok\r\n");
        } else if upper.starts_with("DATA") {
            let t = turn(transaction);
            let final_dot = t.final_dot.unwrap_or(script.final_dot.clone());
            let drop_mid_data = t.drop_mid_data.unwrap_or(script.drop_mid_data);
            match &t.data.unwrap_or(script.data.clone()) {
                Act::Ok => write(&mut io, "354 send it\r\n").await?,
                other => {
                    act!(other, "");
                    continue;
                }
            }

            if drop_mid_data {
                // Read a little, then vanish — §12.3's "drop mid-DATA".
                let mut scratch = [0u8; 16];
                let _ = io.read(&mut scratch).await;
                return Ok(());
            }

            // Read to the terminating dot, un-stuffing as we go.
            loop {
                let mut l = Vec::new();
                if io.read_until(b'\n', &mut l).await? == 0 {
                    return Ok(());
                }
                let content = strip_eol(&l);
                if content == b"." {
                    break;
                }
                let content = content.strip_prefix(b".").unwrap_or(content);
                seen.body.extend_from_slice(content);
                seen.body.extend_from_slice(b"\r\n");
            }

            // Record *before* replying. Simmer answers its own client as soon as
            // this reply lands, so a test that asserts on `messages()` right
            // after a 250 would otherwise race the push and flake.
            if matches!(final_dot, Act::Ok) {
                sink.lock().expect("not poisoned").push(seen.clone());
            }
            if let Some(delay) = script.final_dot_delay {
                tokio::time::sleep(delay).await;
            }
            act!(final_dot, "250 2.0.0 queued as ABC123\r\n");
            seen = Received::default();
        } else if upper.starts_with("QUIT") {
            write(&mut io, "221 2.0.0 bye\r\n").await?;
            return Ok(());
        } else if upper.starts_with("RSET") {
            write(&mut io, "250 2.0.0 ok\r\n").await?;
            if script.close_after_rset {
                return Ok(());
            }
        } else if upper.starts_with("NOOP") {
            write(&mut io, "250 2.0.0 ok\r\n").await?;
        } else {
            write(&mut io, "500 5.5.2 unrecognised\r\n").await?;
        }
    }
}

async fn write(io: &mut BufReader<TcpStream>, s: &str) -> std::io::Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    io.get_mut().write_all(s.as_bytes()).await?;
    io.get_mut().flush().await
}

fn between(s: &str, open: char, close: char) -> Option<String> {
    let start = s.find(open)? + 1;
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[..end]
}

// ---------------------------------------------------------------------------
// Simmer under test
// ---------------------------------------------------------------------------

pub struct Simmer {
    /// The first listener — the only one in every fixture but D-070's.
    pub addr: SocketAddr,
    /// Every listener, in configuration order.
    pub addrs: Vec<SocketAddr>,
    /// §8.3's pools, so a test can read the statistics §9.2 reports and drive
    /// §10.4's drain without a process to signal.
    pub pools: Arc<simmer::downstream::Pool>,
    /// §10.4's in-flight reservations — the engine's own registry, so a test can
    /// assert that every reservation a relay took was resolved.
    pub registry: ReservationRegistry,
    stop: smtp::Shutdown,
    hard: smtp::Shutdown,
    /// The listener task; it resolves to the sessions still running (D-106).
    serve: Option<tokio::task::JoinHandle<smtp::Sessions>>,
    engine: Engine,
    /// D-085 — held so the writer's channel stays open for the life of the
    /// fixture, and closes when it is dropped.
    _capture: Option<simmer::capture::Capture>,
}

impl Drop for Simmer {
    fn drop(&mut self) {
        self.stop.cancel();
        self.hard.cancel();
    }
}

impl Simmer {
    /// Start a Simmer listening on an ephemeral port with the given config.
    ///
    /// Uses [`GrantAllQuota`], so these tests exercise ingress and the outbound
    /// leg without needing a database. The quota model's own behaviour is tested
    /// against real Postgres in `tests/quota.rs`.
    pub async fn start(yaml: &str) -> Simmer {
        Simmer::start_with_quota(yaml, Arc::new(GrantAllQuota::new())).await
    }

    pub async fn start_with_quota(yaml: &str, quota: Arc<dyn QuotaStore>) -> Simmer {
        Simmer::start_with(yaml, quota, Arc::new(simmer::preflight::Registry::new())).await
    }

    /// With a §6.7 preflight registry already populated, so `tests/preflight.rs`
    /// can drive a `strict` route's failure all the way to the client reply
    /// rather than stopping at the chain walk.
    pub async fn start_with(
        yaml: &str,
        quota: Arc<dyn QuotaStore>,
        preflight: Arc<simmer::preflight::Registry>,
    ) -> Simmer {
        let config = simmer::config::from_str(yaml, "test-config").unwrap_or_else(|e| {
            panic!("test config is invalid:\n{e}");
        });
        let (tls, _) = simmer::downstream::TlsConfigs::load().expect("tls");

        let rewriters = simmer::rewrite::Rewriters::compile(&config)
            .unwrap_or_else(|e| panic!("test config's templates do not compile: {e:?}"));

        let pools = Arc::new(simmer::downstream::Pool::build(&config));
        let registry = ReservationRegistry::new();

        // D-085 — started from the config exactly as `main.rs` does, so a test
        // that configures `capture:` gets the real writer rather than a stub. The
        // handle is kept on `Simmer` below: dropping it closes the writer's
        // channel, which is how the writer knows to stop.
        let capture = config.capture.as_ref().map(|c| {
            simmer::capture::Capture::start(c)
                .unwrap_or_else(|e| panic!("starting the test capture: {e}"))
                .0
        });

        let engine = Engine {
            config: Arc::new(config),
            tls: Arc::new(tls),
            pools: Arc::clone(&pools),
            quota,
            registry: registry.clone(),
            rewriters: Arc::new(rewriters),
            frequency: Arc::new(Frequency::new()),
            preflight,
            groups: Arc::new(simmer::routing::domain_group::Grouper::literal()),
            capture: capture.clone(),
        };

        let listener = smtp::Listener::bind(engine.clone())
            .await
            .expect("bind simmer");
        let addr = listener.local_addr().expect("addr");
        let addrs = listener
            .local_addrs()
            .expect("addrs")
            .into_iter()
            .map(|(a, _, _)| a)
            .collect();

        let stop = smtp::Shutdown::new();
        let hard = smtp::Shutdown::new();
        let serve = tokio::spawn(listener.serve(stop.clone(), hard.clone()));

        Simmer {
            addr,
            addrs,
            pools,
            registry,
            stop,
            hard,
            serve: Some(serve),
            engine,
            _capture: capture,
        }
    }

    pub async fn connect(&self) -> Client {
        Client::connect(self.addr).await
    }

    /// §10.4 past its grace period, in `main.rs`'s order: stop accepting, fire
    /// the hard stop, wait (bounded) for the sessions still running, then
    /// release whatever is outstanding. Returns how many sessions were aborted.
    pub async fn hard_stop(&mut self) -> usize {
        self.stop.cancel();
        self.hard.cancel();
        let sessions = self
            .serve
            .take()
            .expect("hard_stop called once")
            .await
            .expect("the listener task");
        let aborted = sessions
            .finish(smtp::relay_drain_bound(&self.engine.config))
            .await;
        simmer::relay::release_outstanding(&self.engine).await;
        aborted
    }
}

/// A received message with the `Received:` header Simmer prepended taken back
/// off, so a test can compare against what the client sent.
///
/// §6.1 step 8 makes exactly one new header, at the top, and D-002 excludes it
/// from §12.3's byte-equivalence comparison for the same reason this helper
/// exists: it is present only because Simmer is in the path. Everything after it
/// is fair game for a byte-for-byte assertion.
pub fn without_received(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    match text.split_once("\r\n") {
        Some((first, rest)) if first.starts_with("Received: ") => rest.to_string(),
        _ => text.to_string(),
    }
}

/// Build a config for a downstream at `addr`. `overrides` are appended verbatim,
/// so a test can change any leaf without restating the whole document.
///
/// The one ramp (D-099) is written compactly — `main` at one space, its keys at
/// two, their sequences at the same two — so every route's contents sit where
/// they sat before ramps existed. An override that continues the route's
/// `identity` keeps working unchanged, and one that sets a ramp-level key
/// (`strict_senders`, `thread_affinity`) is written at two spaces.
pub fn config_for(addr: SocketAddr, overrides: &str) -> String {
    format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 5
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth:
    allow_insecure_auth: true
    mechanisms: [PLAIN, LOGIN]
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
admin:
  listen: "127.0.0.1:0"
  auth_token: "t"
logging: {{ level: warn, format: text }}
default_ramp: main
ramps:
 main:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - match: "oldbrand.com"
    match_on: envelope
    chain: [only]
  default_chain: [only]
  routes:
  - name: only
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: {port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity:
      envelope_from: "b@example.com"
{overrides}"#,
        port = addr.port(),
        overrides = overrides,
    )
}

// ---------------------------------------------------------------------------
// A client for driving Simmer
// ---------------------------------------------------------------------------

pub struct Client {
    /// The library's own client-or-server stream, so `STARTTLS` upgrades in
    /// place exactly as the outbound leg does.
    io: BufReader<Stream>,
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).await.expect("connect to simmer");
        stream.set_nodelay(true).ok();
        Client {
            io: BufReader::new(Stream::Plain(stream)),
        }
    }

    /// RFC 8314 implicit TLS: the handshake before the banner, verifying the
    /// server's certificate against `pki`'s CA for `simmer.test`.
    pub async fn connect_implicit_tls(addr: SocketAddr, pki: &TestPki) -> Client {
        let stream = TcpStream::connect(addr).await.expect("connect to simmer");
        stream.set_nodelay(true).ok();
        let tls = pki
            .connector()
            .connect(pki.server_name(), stream)
            .await
            .expect("implicit TLS handshake");
        Client {
            io: BufReader::new(Stream::Tls(Box::new(tls.into()))),
        }
    }

    /// A handshake over a fresh connection that is expected to *fail* — for
    /// asserting that a bad certificate or a plaintext port is refused.
    pub async fn implicit_tls_error(addr: SocketAddr, pki: &TestPki) -> std::io::Error {
        let stream = TcpStream::connect(addr).await.expect("connect to simmer");
        match pki.connector().connect(pki.server_name(), stream).await {
            Ok(_) => panic!("the TLS handshake succeeded"),
            Err(e) => e,
        }
    }

    /// Issue `STARTTLS`, and upgrade if the server says `220`. Returns the reply
    /// either way, so a test can assert a refusal.
    pub async fn starttls(&mut self, pki: &TestPki) -> Reply {
        let r = self.command("STARTTLS").await;
        if r.code != 220 {
            return r;
        }
        assert!(
            self.io.buffer().is_empty(),
            "server sent bytes after 220 to STARTTLS"
        );
        let Stream::Plain(tcp) = std::mem::replace(self.io.get_mut(), Stream::Taken) else {
            panic!("STARTTLS on a stream that is not plaintext");
        };
        let tls = pki
            .connector()
            .connect(pki.server_name(), tcp)
            .await
            .expect("STARTTLS handshake");
        self.io = BufReader::new(Stream::Tls(Box::new(tls.into())));
        r
    }

    pub fn is_encrypted(&self) -> bool {
        self.io.get_ref().is_encrypted()
    }

    /// Read one complete reply, following `250-` continuations.
    pub async fn read_reply(&mut self) -> Reply {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.io.read_line(&mut line))
                .await
                .expect("timed out waiting for a reply from simmer")
                .expect("read");
            assert!(n > 0, "simmer closed the connection without replying");
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            let code = line[..3].parse().unwrap_or(0);
            let more = line.as_bytes().get(3) == Some(&b'-');
            lines.push(line);
            if !more {
                return Reply { code, lines };
            }
        }
    }

    pub async fn send(&mut self, line: &str) {
        self.io
            .get_mut()
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write");
        self.io.get_mut().flush().await.expect("flush");
    }

    /// Write raw bytes, for pipelining tests that need one syscall.
    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.io.get_mut().write_all(bytes).await.expect("write");
        self.io.get_mut().flush().await.expect("flush");
    }

    pub async fn command(&mut self, line: &str) -> Reply {
        self.send(line).await;
        self.read_reply().await
    }

    /// Greet and get past the banner.
    pub async fn hello(&mut self) -> Reply {
        let banner = self.read_reply().await;
        assert_eq!(banner.code, 220, "banner: {banner:?}");
        self.command("EHLO client.test").await
    }

    /// The whole happy path, returning the reply to the final dot.
    pub async fn deliver(&mut self, from: &str, to: &str, body: &str) -> Reply {
        let r = self.command(&format!("MAIL FROM:<{from}>")).await;
        assert_eq!(r.code, 250, "MAIL FROM: {r:?}");
        let r = self.command(&format!("RCPT TO:<{to}>")).await;
        assert_eq!(r.code, 250, "RCPT TO: {r:?}");
        let r = self.command("DATA").await;
        assert_eq!(r.code, 354, "DATA: {r:?}");
        self.send_raw(&stuff(body.as_bytes())).await;
        self.read_reply().await
    }

    /// Whether the peer has closed. Used to assert `421` really disconnects.
    pub async fn is_closed(&mut self) -> bool {
        let mut buf = [0u8; 1];
        matches!(
            tokio::time::timeout(Duration::from_secs(2), self.io.read(&mut buf)).await,
            Ok(Ok(0))
        )
    }
}

/// Dot-stuff a message and append the terminator, as a real client must.
///
/// Written independently of `simmer::smtp::buffer::stuff_into` on purpose. Using
/// the implementation under test to construct the test input would make the
/// byte-for-byte forwarding assertion circular: a stuffing bug mirrored by an
/// identical un-stuffing bug would pass.
fn stuff(message: &[u8]) -> Vec<u8> {
    if message.is_empty() {
        return b".\r\n".to_vec();
    }

    let mut segments: Vec<&[u8]> = message.split(|b| *b == b'\n').collect();
    if message.ends_with(b"\n") {
        // The empty segment after a trailing newline is not a line.
        segments.pop();
    }

    let mut out = Vec::with_capacity(message.len() + 16);
    for line in segments {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.starts_with(b".") {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }
}

// ---------------------------------------------------------------------------
// A quota store that always says yes
// ---------------------------------------------------------------------------

/// §11's storage trait, implemented in memory so the ingress and reply-mapping
/// suites do not need a database to test ingress and reply mapping.
///
/// Not a second *backend* in the §11 sense — it enforces nothing and is only
/// reachable from tests. It does record what the relay path asked it to do, so a
/// test can assert the §7.4 obligation that every reservation is resolved exactly
/// once, and that a failed send commits nothing.
/// `(route, domain_group, day_index)` to its row, as `quota_usage` is keyed.
type UsageRows = HashMap<(String, String, i64), Usage>;
/// D-111 — `(route, domain_group)` to its bucket's `tat`.
type RateRows = HashMap<(String, String), chrono::DateTime<chrono::Utc>>;

#[derive(Default)]
pub struct GrantAllQuota {
    committed: Arc<Mutex<Vec<Reservation>>>,
    released: Arc<Mutex<Vec<Reservation>>>,
    paused: Arc<Mutex<Vec<String>>>,
    /// §9.3 — routes pinned to their final schedule value.
    graduated: Arc<Mutex<Vec<String>>>,
    /// §9.2/§9.3 — rows, keyed as the real table is. Empty by default, which is
    /// what "nothing has been sent today" looks like.
    usage: Arc<Mutex<UsageRows>>,
    /// §7.3 — every key handed to `commit`, in order. The relay's half of the
    /// §7.4 phase 3 obligation: events are recorded on a downstream `2xx` and on
    /// nothing else.
    recorded: Arc<Mutex<Vec<(String, Key)>>>,
    /// What `recipient_event_count` should answer, keyed by route. Lets a test
    /// put a route over its threshold without a database and without waiting for
    /// a window to fill.
    counts: Arc<Mutex<HashMap<String, i64>>>,
    /// D-111 — each rate bucket's `tat`, keyed `(route, domain_group)`, driven
    /// by the same pure `quota::rate` functions the real stores call.
    rate: Arc<Mutex<RateRows>>,
    /// D-111 — every slot given back, in order.
    unbooked: Arc<Mutex<Vec<simmer::quota::RateKey>>>,
    /// Make every `reserve` fail as a storage error, for the walk's error path.
    fail_reserve: Arc<AtomicUsize>,
}

impl GrantAllQuota {
    /// Make every later `reserve` fail as a storage error (§7.5).
    pub fn fail_reserves(&self) {
        self.fail_reserve.store(1, Ordering::SeqCst);
    }

    /// D-111 — the slots given back so far.
    pub fn unbooked(&self) -> Vec<simmer::quota::RateKey> {
        self.unbooked.lock().expect("not poisoned").clone()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Make a route report as paused (§3.2 3a) without a database.
    pub fn pause(&self, route: &str) {
        self.paused
            .lock()
            .expect("not poisoned")
            .push(route.to_string());
    }

    pub fn committed(&self) -> Vec<Reservation> {
        self.committed.lock().expect("not poisoned").clone()
    }

    pub fn released(&self) -> Vec<Reservation> {
        self.released.lock().expect("not poisoned").clone()
    }

    /// §7.3 — the recipient keys recorded so far, with the route they were
    /// recorded for.
    pub fn recorded(&self) -> Vec<(String, Key)> {
        self.recorded.lock().expect("not poisoned").clone()
    }

    /// Put a route at `count` events for every recipient, so a test can drive the
    /// §3.2 3b skip without writing rows or moving a clock.
    pub fn set_recipient_count(&self, route: &str, count: i64) {
        self.counts
            .lock()
            .expect("not poisoned")
            .insert(route.to_string(), count);
    }
}

#[async_trait::async_trait]
impl QuotaStore for GrantAllQuota {
    async fn reserve(&self, req: &ReserveRequest) -> Result<Reserved, QuotaError> {
        if self.fail_reserve.load(Ordering::SeqCst) != 0 {
            return Err(QuotaError::Storage("injected reserve failure".into()));
        }
        Ok(Reserved::Taken(Reservation {
            ramp: req.ramp.clone(),
            id: uuid::Uuid::new_v4(),
            route: req.route.clone(),
            domain_group: req.domain_group.clone(),
            day_index: req.day_index,
            count: req.count,
        }))
    }

    async fn commit(
        &self,
        reservation: &Reservation,
        recipient_keys: &[Key],
    ) -> Result<(), QuotaError> {
        self.committed
            .lock()
            .expect("not poisoned")
            .push(reservation.clone());

        let mut recorded = self.recorded.lock().expect("not poisoned");
        for key in recipient_keys {
            recorded.push((reservation.route.clone(), key.clone()));
        }
        Ok(())
    }

    async fn release(&self, reservation: &Reservation) -> Result<(), QuotaError> {
        self.released
            .lock()
            .expect("not poisoned")
            .push(reservation.clone());
        Ok(())
    }

    // One ramp per fake: every test that uses it configures only the default one,
    // so the ramp argument is accepted and not keyed on.
    async fn usage(
        &self,
        _ramp: &str,
        route: &str,
        group: &str,
        day: i64,
    ) -> Result<Usage, QuotaError> {
        Ok(self
            .usage
            .lock()
            .expect("not poisoned")
            .get(&(route.to_string(), group.to_string(), day))
            .copied()
            .unwrap_or_default())
    }

    async fn usage_many(
        &self,
        _ramp: &str,
        keys: &[simmer::quota::UsageKey],
    ) -> Result<std::collections::HashMap<(String, String), Usage>, QuotaError> {
        let rows = self.usage.lock().expect("not poisoned");
        Ok(keys
            .iter()
            .filter_map(|k| {
                rows.get(&(k.route.clone(), k.domain_group.clone(), k.day_index))
                    .map(|u| ((k.route.clone(), k.domain_group.clone()), *u))
            })
            .collect())
    }

    async fn set_paused(&self, _ramp: &str, route: &str, paused: bool) -> Result<(), QuotaError> {
        let mut rows = self.paused.lock().expect("not poisoned");
        rows.retain(|r| r != route);
        if paused {
            rows.push(route.to_string());
        }
        Ok(())
    }

    async fn set_graduated(
        &self,
        _ramp: &str,
        route: &str,
        graduated: bool,
    ) -> Result<(), QuotaError> {
        let mut rows = self.graduated.lock().expect("not poisoned");
        rows.retain(|r| r != route);
        if graduated {
            rows.push(route.to_string());
        }
        Ok(())
    }

    async fn set_allowance_override(
        &self,
        _ramp: &str,
        route: &str,
        group: &str,
        day: i64,
        allowance: Option<i64>,
        scheduled: Option<i64>,
    ) -> Result<(), QuotaError> {
        let mut rows = self.usage.lock().expect("not poisoned");
        let row = rows
            .entry((route.to_string(), group.to_string(), day))
            .or_insert(Usage {
                allowance: scheduled,
                ..Usage::default()
            });
        row.allowance_override = allowance;
        Ok(())
    }

    async fn reset_counters(
        &self,
        _ramp: &str,
        route: &str,
        group: &str,
        day: i64,
    ) -> Result<Option<simmer::quota::Reset>, QuotaError> {
        let mut rows = self.usage.lock().expect("not poisoned");
        let Some(row) = rows.get_mut(&(route.to_string(), group.to_string(), day)) else {
            return Ok(None);
        };
        let before = *row;
        row.committed = 0;
        // The fake holds no reservation rows, so "recompute from the live
        // reservations" is zero here. The real arithmetic is asserted against
        // Postgres in `tests/admin_api.rs`.
        row.reserved = 0;
        Ok(Some(simmer::quota::Reset {
            committed_before: before.committed,
            reserved_before: before.reserved,
            reserved_after: 0,
        }))
    }

    async fn route_states(
        &self,
        _ramp: &str,
    ) -> Result<std::collections::HashMap<String, RouteState>, QuotaError> {
        let mut out: std::collections::HashMap<String, RouteState> =
            std::collections::HashMap::new();
        for route in self.paused.lock().expect("not poisoned").iter() {
            out.entry(route.clone()).or_default().paused = true;
        }
        for route in self.graduated.lock().expect("not poisoned").iter() {
            out.entry(route.clone()).or_default().graduated = true;
        }
        Ok(out)
    }

    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError> {
        Ok(Vec::new())
    }

    async fn recipient_event_count(
        &self,
        _ramp: &str,
        route: &str,
        _key: &Key,
        _since: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64, QuotaError> {
        Ok(self
            .counts
            .lock()
            .expect("not poisoned")
            .get(route)
            .copied()
            .unwrap_or(0))
    }

    async fn recipient_hash_salt(&self) -> Result<Vec<u8>, QuotaError> {
        // Fixed rather than generated: a test that asserts on a key needs the
        // same key twice.
        Ok(b"a fixed salt for the in-memory store".to_vec())
    }

    async fn adopt_legacy_rows(&self, _ramp: &str) -> Result<simmer::quota::Adoption, QuotaError> {
        Ok(simmer::quota::Adoption::default())
    }

    async fn sweep_recipient_events(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, QuotaError> {
        Ok(0)
    }

    async fn book_rate_slot(
        &self,
        req: &simmer::quota::RateBookRequest,
    ) -> Result<simmer::quota::rate::RateBooked, QuotaError> {
        use simmer::quota::rate::{decide, RateBooked};
        let mut rows = self.rate.lock().expect("not poisoned");
        let k = (req.key.route.clone(), req.key.domain_group.clone());
        let outcome = decide(
            rows.get(&k).copied(),
            req.rate,
            req.now,
            req.max_wait,
            req.force,
        );
        if let RateBooked::Booked { booked_tat, .. } = outcome {
            rows.insert(k, booked_tat);
        }
        Ok(outcome)
    }

    async fn unbook_rate_slot(
        &self,
        key: &simmer::quota::RateKey,
        rate: simmer::quota::rate::Rate,
        booked_tat: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, QuotaError> {
        let mut rows = self.rate.lock().expect("not poisoned");
        let k = (key.route.clone(), key.domain_group.clone());
        self.unbooked
            .lock()
            .expect("not poisoned")
            .push(key.clone());
        match simmer::quota::rate::unbook(rows.get(&k).copied(), rate, booked_tat) {
            Some(tat) => {
                rows.insert(k, tat);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn rate_tats(
        &self,
        _ramp: &str,
    ) -> Result<HashMap<(String, String), chrono::DateTime<chrono::Utc>>, QuotaError> {
        Ok(self.rate.lock().expect("not poisoned").clone())
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Certificates for §5.1's inbound TLS (D-070)
// ---------------------------------------------------------------------------

/// A throwaway CA and a leaf it issued, written to a temporary directory.
///
/// A real two-level chain rather than a self-signed leaf, so the client side of
/// every test *verifies* the server — webpki refuses a CA certificate presented
/// as an end entity, and a test that skipped verification would prove only that
/// bytes were encrypted, not that the right certificate was served.
pub struct TestPki {
    dir: tempfile::TempDir,
    ca: rustls::pki_types::CertificateDer<'static>,
    server_name: String,
}

impl TestPki {
    /// A leaf for `names`, issued by a fresh CA. The first name is the one the
    /// client verifies against.
    pub fn new(names: &[&str]) -> TestPki {
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "simmer test CA");
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");
        let issuer = rcgen::Issuer::new(ca_params, ca_key);

        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf =
            rcgen::CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .expect("leaf params")
                .signed_by(&leaf_key, &issuer)
                .expect("leaf cert");

        let dir = tempfile::tempdir().expect("tempdir");
        // Leaf first, then the CA: the order §5.1's `certificate` documents.
        std::fs::write(
            dir.path().join("cert.pem"),
            format!("{}{}", leaf.pem(), ca_cert.pem()),
        )
        .expect("write cert");
        std::fs::write(dir.path().join("key.pem"), leaf_key.serialize_pem()).expect("write key");

        TestPki {
            dir,
            ca: ca_cert.der().clone(),
            server_name: names[0].to_string(),
        }
    }

    pub fn cert_path(&self) -> String {
        self.dir.path().join("cert.pem").display().to_string()
    }

    pub fn key_path(&self) -> String {
        self.dir.path().join("key.pem").display().to_string()
    }

    /// The `server.tls` block for a config.
    pub fn yaml(&self) -> String {
        format!(
            "  tls:\n    certificate: \"{}\"\n    private_key: \"{}\"\n",
            self.cert_path(),
            self.key_path()
        )
    }

    fn server_name(&self) -> rustls::pki_types::ServerName<'static> {
        rustls::pki_types::ServerName::try_from(self.server_name.clone()).expect("server name")
    }

    /// A verifying client that trusts this CA and nothing else.
    fn connector(&self) -> tokio_rustls::TlsConnector {
        tokio_rustls::TlsConnector::from(Arc::new(self.client_config()))
    }

    /// The configuration behind [`connector`](Self::connector), for a client
    /// that is not a raw stream — D-083's link proxy dialling an HTTPS upstream.
    pub fn client_config(&self) -> rustls::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca.clone()).expect("add CA");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    /// A server presenting this leaf: D-083's tests stand up an HTTPS upstream.
    pub fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = CertificateDer::pem_file_iter(self.cert_path())
            .expect("read cert")
            .collect::<Result<Vec<_>, _>>()
            .expect("parse cert");
        let key = PrivateKeyDer::from_pem_file(self.key_path()).expect("read key");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("server config");
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }
}

// ---------------------------------------------------------------------------
// Known findings
// ---------------------------------------------------------------------------

/// Run a check that a *known, not yet fixed* defect is expected to fail.
///
/// The test-programme counterpart of `test/known-findings.toml`, for `cargo
/// test`. `#[ignore]` would hide the defect and, worse, hide its fix: nobody
/// notices an ignored test start passing. This runs the check every time and
/// inverts the verdict:
///
/// - the check **panics** → the defect is still there → the test passes, and
///   says `XFAIL` on stderr;
/// - the check **passes** → the defect looks fixed → the test fails with
///   `XPASS`, so the fixing commit has to delete the marker and turn the check
///   into an ordinary test.
///
/// `finding` is the id from the test programme's findings table (`F1`, `F2`, …).
///
/// `because` lists the failure messages the defect produces, and a panic counts
/// as `XFAIL` only if its message contains one of them. Without that, a check
/// broken for an unrelated reason — a typo, a harness fault, a changed reply —
/// would be reported as "known defect still present" and hide itself. A panic
/// with any other message fails the test as a wrong-reason failure.
pub async fn xfail<F>(finding: &str, because: &[&str], check: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::spawn(check).await {
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            assert!(
                because.iter().any(|b| message.contains(b)),
                "{finding}: the check failed, but not for the known reason — expected one \
                 of {because:?}, got: {message}"
            );
            eprintln!("XFAIL {finding}: the known defect is still present ({message})");
        }
        Err(e) => panic!("{finding}: the check did not complete: {e}"),
        Ok(()) => panic!(
            "XPASS {finding}: the check now passes, so the defect looks fixed. Remove the \
             xfail marker (and its test/known-findings.toml entry) in the fixing commit"
        ),
    }
}
