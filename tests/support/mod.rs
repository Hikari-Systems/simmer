//! Test harness: a scripted fake downstream, a Simmer under test, and a small
//! SMTP client to drive it.
//!
//! §12.3 asks for "a scripted fake downstream that can be made to return
//! arbitrary codes, stall, drop mid-`DATA`, and refuse TLS", against which the
//! §10.1 reply mapping is asserted exhaustively. That is what [`FakeDownstream`]
//! is.

#![allow(dead_code)] // each integration test file uses a different subset

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

pub struct FakeDownstream {
    pub addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeDownstream {
    pub async fn start(script: Script) -> FakeDownstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
        let addr = listener.local_addr().expect("addr");
        let received = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&received);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = script.clone();
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let _ = serve_one(stream, script, sink).await;
                });
            }
        });

        FakeDownstream { addr, received }
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
    script: Script,
    sink: Arc<Mutex<Vec<Received>>>,
) -> std::io::Result<()> {
    let mut io = BufReader::new(stream);
    let mut seen = Received::default();

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
            act!(script.mail_from, "250 2.1.0 sender ok\r\n");
        } else if upper.starts_with("RCPT TO") {
            if let Some(r) = between(&line, '<', '>') {
                seen.recipients.push(r);
            }
            act!(script.rcpt_to, "250 2.1.5 recipient ok\r\n");
        } else if upper.starts_with("DATA") {
            match &script.data {
                Act::Ok => write(&mut io, "354 send it\r\n").await?,
                other => {
                    act!(other, "");
                    continue;
                }
            }

            if script.drop_mid_data {
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
            if matches!(script.final_dot, Act::Ok) {
                sink.lock().expect("not poisoned").push(seen.clone());
            }
            act!(script.final_dot, "250 2.0.0 queued as ABC123\r\n");
            seen = Received::default();
        } else if upper.starts_with("QUIT") {
            write(&mut io, "221 2.0.0 bye\r\n").await?;
            return Ok(());
        } else if upper.starts_with("RSET") || upper.starts_with("NOOP") {
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
    pub addr: SocketAddr,
    stop: smtp::Shutdown,
    hard: smtp::Shutdown,
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
        let config = simmer::config::from_str(yaml, "test-config").unwrap_or_else(|e| {
            panic!("test config is invalid:\n{e}");
        });
        let (tls, _) = simmer::downstream::TlsConfigs::load().expect("tls");

        let rewriters = simmer::rewrite::Rewriters::compile(&config)
            .unwrap_or_else(|e| panic!("test config's templates do not compile: {e:?}"));

        let engine = Engine {
            config: Arc::new(config),
            tls: Arc::new(tls),
            quota,
            registry: ReservationRegistry::new(),
            rewriters: Arc::new(rewriters),
            frequency: Arc::new(Frequency::new()),
        };

        let listener = smtp::Listener::bind(engine).await.expect("bind simmer");
        let addr = listener.local_addr().expect("addr");

        let stop = smtp::Shutdown::new();
        let hard = smtp::Shutdown::new();
        tokio::spawn(listener.serve(stop.clone(), hard.clone()));

        Simmer { addr, stop, hard }
    }

    pub async fn connect(&self) -> Client {
        Client::connect(self.addr).await
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
pub fn config_for(addr: SocketAddr, overrides: &str) -> String {
    format!(
        r#"
server:
  listen: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 5
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth:
    required: false
    allow_insecure_auth: true
    mechanisms: [PLAIN, LOGIN]
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
admin:
  listen: "127.0.0.1:0"
  auth_token: "t"
logging: {{ level: warn, format: text }}
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
    io: BufReader<TcpStream>,
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).await.expect("connect to simmer");
        stream.set_nodelay(true).ok();
        Client {
            io: BufReader::new(stream),
        }
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
#[derive(Default)]
pub struct GrantAllQuota {
    committed: Arc<Mutex<Vec<Reservation>>>,
    released: Arc<Mutex<Vec<Reservation>>>,
    paused: Arc<Mutex<Vec<String>>>,
    /// §7.3 — every key handed to `commit`, in order. The relay's half of the
    /// §7.4 phase 3 obligation: events are recorded on a downstream `2xx` and on
    /// nothing else.
    recorded: Arc<Mutex<Vec<(String, Key)>>>,
    /// What `recipient_event_count` should answer, keyed by route. Lets a test
    /// put a route over its threshold without a database and without waiting for
    /// a window to fill.
    counts: Arc<Mutex<HashMap<String, i64>>>,
}

impl GrantAllQuota {
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
        Ok(Reserved::Taken(Reservation {
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

    async fn usage(&self, _: &str, _: &str, _: i64) -> Result<Usage, QuotaError> {
        Ok(Usage::default())
    }

    async fn route_states(
        &self,
    ) -> Result<std::collections::HashMap<String, RouteState>, QuotaError> {
        Ok(self
            .paused
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|r| {
                (
                    r.clone(),
                    RouteState {
                        paused: true,
                        graduated: false,
                    },
                )
            })
            .collect())
    }

    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError> {
        Ok(Vec::new())
    }

    async fn recipient_event_count(
        &self,
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

    async fn sweep_recipient_events(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, QuotaError> {
        Ok(0)
    }

    async fn is_available(&self) -> bool {
        true
    }
}
