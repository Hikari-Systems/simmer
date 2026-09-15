//! A fast, counting SMTP sink for the test programme's load tiers.
//!
//! **Not part of the shipped image.** `Dockerfile`'s `acceptance` stage adds it,
//! next to the loadgen; the `runtime` stage does not.
//!
//! Mailpit is the right downstream for asserting on *content*, and the wrong one
//! for load: it stores every message in memory and becomes both the bottleneck and
//! the leak the soak would then blame on Simmer. This sink keeps nothing but
//! counts and one JSON line per message — enough to answer the questions a load
//! tier asks of a downstream, and nothing that grows with the body size:
//!
//! - **What arrived, exactly once?** Every message is recorded against the
//!   loadgen's test id — from the RCPT local part and the `X-Test-Id` header,
//!   which must agree (a body on the wrong envelope is recorded as a mismatch).
//!   `tests/compose/reconcile.rs` joins these records with the loadgen's. Where
//!   a route stamps `X-Simmer-Correlation`, the record carries it too: the id
//!   Simmer logged the message under, which is how the soak traces a slow one.
//! - **How many connections at once?** Peak concurrent connections per listener
//!   and per peer address, which is how §8.3's `max_connections` is measured from
//!   the side it protects.
//! - **What if the downstream misbehaves?** Scripted faults, by rate (seeded, so a
//!   run is reproducible) or per message by an `X-Sink-Script` header.
//!
//! One listener per route: `--listen 0.0.0.0:2525 --listen 0.0.0.0:2526`. Port 0
//! binds an ephemeral port; every bound address is printed as
//! `sink listening <requested> <bound>` so a test can find it.
//!
//! Speaks SMTP by hand for the loadgen's reason: the point is the reply codes and
//! the timing of them, which a server crate would decide for us.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
struct Opts {
    listens: Vec<String>,
    jsonl: Option<String>,
    http: Option<String>,
    seed: u64,
    fail_rcpt_pct: f64,
    fail_data_pct: f64,
    drop_before_dot_pct: f64,
    drop_after_dot_pct: f64,
    stall_at_dot_pct: f64,
    stall: Duration,
    slow: Duration,
    idle_close: Option<Duration>,
    lose_every: Option<u64>,
}

fn opts() -> Opts {
    let mut o = Opts {
        listens: Vec::new(),
        jsonl: None,
        http: None,
        seed: 1,
        fail_rcpt_pct: 0.0,
        fail_data_pct: 0.0,
        drop_before_dot_pct: 0.0,
        drop_after_dot_pct: 0.0,
        stall_at_dot_pct: 0.0,
        stall: Duration::from_secs(30),
        slow: Duration::ZERO,
        idle_close: None,
        lose_every: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let value = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{} needs a value", argv[i]))
                .clone()
        };
        let pct = |v: String| -> f64 {
            let p: f64 = v.parse().expect("a percentage");
            assert!((0.0..=100.0).contains(&p), "percentages are 0..=100");
            p
        };
        match argv[i].as_str() {
            "--listen" => o.listens.push(value()),
            "--jsonl" => o.jsonl = Some(value()),
            "--http" => o.http = Some(value()),
            "--seed" => o.seed = value().parse().expect("--seed"),
            "--fail-rcpt-pct" => o.fail_rcpt_pct = pct(value()),
            "--fail-data-pct" => o.fail_data_pct = pct(value()),
            "--drop-before-dot-pct" => o.drop_before_dot_pct = pct(value()),
            "--drop-after-dot-pct" => o.drop_after_dot_pct = pct(value()),
            "--stall-at-dot-pct" => o.stall_at_dot_pct = pct(value()),
            "--stall-secs" => o.stall = Duration::from_secs(value().parse().expect("--stall-secs")),
            "--slow-ms" => o.slow = Duration::from_millis(value().parse().expect("--slow-ms")),
            "--idle-close-secs" => {
                o.idle_close = Some(Duration::from_secs(
                    value().parse().expect("--idle-close-secs"),
                ))
            }
            // The self-check mode: answer 250 and record nothing, every Nth
            // message. A reconciler that passes this run is broken.
            "--lose-every" => o.lose_every = Some(value().parse().expect("--lose-every")),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    if o.listens.is_empty() {
        o.listens.push("0.0.0.0:2525".to_string());
    }
    o
}

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Gauge {
    open: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicU64,
}

impl Gauge {
    fn enter(&self) {
        let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.total.fetch_add(1, Ordering::SeqCst);
    }
    fn leave(&self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
    fn json(&self) -> String {
        format!(
            r#"{{"open":{},"peak":{},"connections":{}}}"#,
            self.open.load(Ordering::SeqCst),
            self.peak.load(Ordering::SeqCst),
            self.total.load(Ordering::SeqCst)
        )
    }
}

#[derive(Default)]
struct Outcomes {
    delivered: AtomicU64,
    rejected: AtomicU64,
    dropped_before_dot: AtomicU64,
    dropped_after_dot: AtomicU64,
    stalled_at_dot: AtomicU64,
    lost: AtomicU64,
    mismatches: AtomicU64,
}

struct State {
    opts: Opts,
    listeners: HashMap<String, Arc<Gauge>>,
    peers: Mutex<HashMap<(String, String), Arc<Gauge>>>,
    outcomes: Outcomes,
    delivered_seq: AtomicU64,
    conn_seq: AtomicU64,
    rng: Mutex<u64>,
    records: mpsc::UnboundedSender<String>,
    started: Instant,
}

impl State {
    /// `pct` percent of the time. xorshift64*, seeded from `--seed`: no
    /// dependency, and the same seed gives the same faults in the same order.
    fn chance(&self, pct: f64) -> bool {
        if pct <= 0.0 {
            return false;
        }
        let mut s = self.rng.lock().expect("rng");
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        let x = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 11) as f64 / (1u64 << 53) as f64 * 100.0 < pct
    }

    fn peer(&self, listener: &str, ip: &str) -> Arc<Gauge> {
        Arc::clone(
            self.peers
                .lock()
                .expect("peers")
                .entry((listener.to_string(), ip.to_string()))
                .or_default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        id: &str,
        outcome: &'static str,
        listener: &str,
        conn: u64,
        peer: &str,
        mismatch: bool,
        correlation: Option<&str>,
    ) {
        let counter = match outcome {
            "delivered" => &self.outcomes.delivered,
            "rejected" => &self.outcomes.rejected,
            "dropped_before_dot" => &self.outcomes.dropped_before_dot,
            "dropped_after_dot" => &self.outcomes.dropped_after_dot,
            _ => &self.outcomes.stalled_at_dot,
        };
        counter.fetch_add(1, Ordering::SeqCst);
        if mismatch {
            self.outcomes.mismatches.fetch_add(1, Ordering::SeqCst);
        }
        let _ = self.records.send(format!(
            r#"{{"id":{},"outcome":"{outcome}","listener":{},"conn":{conn},"peer":{},"mismatch":{mismatch},"correlation":{},"at_ms":{}}}"#,
            json_string(id),
            json_string(listener),
            json_string(peer),
            correlation.map_or_else(|| "null".to_string(), json_string),
            self.started.elapsed().as_millis()
        ));
    }

    fn stats(&self) -> String {
        let listeners: Vec<String> = self
            .listeners
            .iter()
            .map(|(name, g)| format!("{}:{}", json_string(name), g.json()))
            .collect();
        let peers: Vec<String> = self
            .peers
            .lock()
            .expect("peers")
            .iter()
            .map(|((l, ip), g)| format!("{}:{}", json_string(&format!("{l} {ip}")), g.json()))
            .collect();
        let o = &self.outcomes;
        format!(
            r#"{{"listeners":{{{}}},"peers":{{{}}},"outcomes":{{"delivered":{},"rejected":{},"dropped_before_dot":{},"dropped_after_dot":{},"stalled_at_dot":{},"lost":{},"mismatches":{}}}}}"#,
            listeners.join(","),
            peers.join(","),
            o.delivered.load(Ordering::SeqCst),
            o.rejected.load(Ordering::SeqCst),
            o.dropped_before_dot.load(Ordering::SeqCst),
            o.dropped_after_dot.load(Ordering::SeqCst),
            o.stalled_at_dot.load(Ordering::SeqCst),
            o.lost.load(Ordering::SeqCst),
            o.mismatches.load(Ordering::SeqCst),
        )
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let opts = opts();

    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(write_records(opts.jsonl.clone(), rx));

    let mut bound = Vec::new();
    for requested in &opts.listens {
        let listener = TcpListener::bind(requested)
            .await
            .unwrap_or_else(|e| panic!("binding {requested}: {e}"));
        let addr = listener.local_addr().expect("addr");
        println!("sink listening {requested} {addr}");
        bound.push((requested.clone(), listener));
    }

    let state = Arc::new(State {
        listeners: bound
            .iter()
            .map(|(name, _)| (name.clone(), Arc::new(Gauge::default())))
            .collect(),
        peers: Mutex::new(HashMap::new()),
        outcomes: Outcomes::default(),
        delivered_seq: AtomicU64::new(0),
        conn_seq: AtomicU64::new(0),
        rng: Mutex::new(opts.seed.max(1)),
        records: tx,
        started: Instant::now(),
        opts,
    });

    if let Some(http) = state.opts.http.clone() {
        let listener = TcpListener::bind(&http)
            .await
            .unwrap_or_else(|e| panic!("binding {http}: {e}"));
        println!("sink http {}", listener.local_addr().expect("addr"));
        tokio::spawn(serve_http(listener, Arc::clone(&state)));
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    for (name, listener) in bound {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    continue;
                };
                let state = Arc::clone(&state);
                let name = name.clone();
                tokio::spawn(async move {
                    let gauge = Arc::clone(&state.listeners[&name]);
                    let per_peer = state.peer(&name, &peer.ip().to_string());
                    gauge.enter();
                    per_peer.enter();
                    let _ = serve(stream, peer, &name, &state).await;
                    per_peer.leave();
                    gauge.leave();
                });
            }
        });
    }

    // `docker stop` sends SIGTERM, a terminal sends SIGINT; records are already
    // on disk either way, since the writer flushes whenever its queue drains.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    drop(state);
    let _ = writer.await;
}

/// One JSON line per record, batched: a line is written as soon as it arrives
/// and the file is flushed whenever the queue drains, so a record is on disk
/// within one scheduling turn of the reply it describes.
async fn write_records(path: Option<String>, mut rx: mpsc::UnboundedReceiver<String>) {
    use std::io::Write;
    let mut out: Box<dyn std::io::Write + Send> = match path {
        Some(p) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(&p).unwrap_or_else(|e| panic!("creating {p}: {e}")),
        )),
        None => Box::new(std::io::sink()),
    };
    while let Some(line) = rx.recv().await {
        let _ = writeln!(out, "{line}");
        while let Ok(more) = rx.try_recv() {
            let _ = writeln!(out, "{more}");
        }
        let _ = out.flush();
    }
    let _ = out.flush();
}

async fn serve_http(listener: TcpListener, state: Arc<State>) {
    loop {
        let Ok((mut s, _)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let body = state.stats();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        });
    }
}

// ---------------------------------------------------------------------------
// one SMTP connection
// ---------------------------------------------------------------------------

/// A fault named by a message's `X-Sink-Script` header. Header faults apply at
/// the final dot, the only stage at which the header has arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Script {
    None,
    Defer,
    DropAfterDot,
    Stall(Duration),
    Slow(Duration),
}

fn parse_script(v: &str) -> Script {
    let v = v.trim();
    let (what, arg) = v.split_once(':').unwrap_or((v, ""));
    let secs = |a: &str| -> Duration {
        let a = a.trim();
        if let Some(ms) = a.strip_suffix("ms") {
            Duration::from_millis(ms.parse().unwrap_or(0))
        } else {
            Duration::from_secs(a.trim_end_matches('s').parse().unwrap_or(0))
        }
    };
    match what {
        "451@dot" => Script::Defer,
        "drop@dot" => Script::DropAfterDot,
        "stall@dot" => Script::Stall(secs(arg)),
        "slow@dot" => Script::Slow(secs(arg)),
        _ => Script::None,
    }
}

async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    listener: &str,
    st: &State,
) -> std::io::Result<()> {
    let conn = st.conn_seq.fetch_add(1, Ordering::SeqCst);
    let peer_ip = peer.ip().to_string();
    let mut io = BufReader::new(stream);
    reply(&mut io, "220 sink.test ESMTP\r\n").await?;

    let mut rcpt: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = match st.opts.idle_close {
            // A downstream that reaps idle connections — the case §8.3's pool
            // and D-068's retry exist for.
            Some(idle) => match tokio::time::timeout(idle, io.read_line(&mut line)).await {
                Ok(r) => r?,
                Err(_) => return Ok(()),
            },
            None => io.read_line(&mut line).await?,
        };
        if n == 0 {
            return Ok(());
        }
        let cmd = line.trim_end().to_string();
        let upper = cmd.to_ascii_uppercase();

        if upper.starts_with("EHLO") {
            reply(
                &mut io,
                "250-sink.test\r\n250-PIPELINING\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-SIZE 52428800\r\n250 AUTH PLAIN LOGIN\r\n",
            )
            .await?;
        } else if upper.starts_with("HELO") {
            reply(&mut io, "250 sink.test\r\n").await?;
        } else if upper.starts_with("AUTH LOGIN") {
            reply(&mut io, "334 VXNlcm5hbWU6\r\n").await?;
            let mut s = String::new();
            io.read_line(&mut s).await?;
            reply(&mut io, "334 UGFzc3dvcmQ6\r\n").await?;
            io.read_line(&mut s).await?;
            reply(&mut io, "235 2.7.0 accepted\r\n").await?;
        } else if upper.starts_with("AUTH") {
            reply(&mut io, "235 2.7.0 accepted\r\n").await?;
        } else if upper.starts_with("MAIL") {
            rcpt = None;
            reply(&mut io, "250 2.1.0 ok\r\n").await?;
        } else if upper.starts_with("RCPT") {
            let addr = between(&cmd, '<', '>').unwrap_or_default();
            if st.chance(st.opts.fail_rcpt_pct) {
                st.record(
                    &id_of(&addr),
                    "rejected",
                    listener,
                    conn,
                    &peer_ip,
                    false,
                    None,
                );
                reply(&mut io, "451 4.3.0 sink: scripted deferral at RCPT\r\n").await?;
            } else {
                rcpt = Some(addr);
                reply(&mut io, "250 2.1.5 ok\r\n").await?;
            }
        } else if upper.starts_with("DATA") {
            let envelope_id = rcpt.as_deref().map(id_of).unwrap_or_default();
            if st.chance(st.opts.fail_data_pct) {
                st.record(
                    &envelope_id,
                    "rejected",
                    listener,
                    conn,
                    &peer_ip,
                    false,
                    None,
                );
                reply(&mut io, "451 4.3.0 sink: scripted deferral at DATA\r\n").await?;
                continue;
            }
            reply(&mut io, "354 go ahead\r\n").await?;

            if st.chance(st.opts.drop_before_dot_pct) {
                // Take a little of the body, then vanish before the dot:
                // nothing is stored.
                let mut scratch = [0u8; 64];
                let _ = io.read(&mut scratch).await;
                st.record(
                    &envelope_id,
                    "dropped_before_dot",
                    listener,
                    conn,
                    &peer_ip,
                    false,
                    None,
                );
                return Ok(());
            }

            let (header_id, correlation, script) = read_body(&mut io).await?;
            let correlation = correlation.as_deref();
            let id = if envelope_id.is_empty() {
                header_id.clone().unwrap_or_default()
            } else {
                envelope_id.clone()
            };
            let mismatch = header_id
                .as_deref()
                .is_some_and(|h| !envelope_id.is_empty() && h != envelope_id);

            let script = if script != Script::None {
                script
            } else if st.chance(st.opts.drop_after_dot_pct) {
                Script::DropAfterDot
            } else if st.chance(st.opts.stall_at_dot_pct) {
                Script::Stall(st.opts.stall)
            } else if !st.opts.slow.is_zero() {
                Script::Slow(st.opts.slow)
            } else {
                Script::None
            };

            match script {
                Script::Defer => {
                    st.record(
                        &id,
                        "rejected",
                        listener,
                        conn,
                        &peer_ip,
                        mismatch,
                        correlation,
                    );
                    reply(&mut io, "451 4.3.0 sink: scripted deferral at the dot\r\n").await?;
                }
                Script::DropAfterDot => {
                    // Stored, then gone without a word: §10.2's window.
                    st.record(
                        &id,
                        "dropped_after_dot",
                        listener,
                        conn,
                        &peer_ip,
                        mismatch,
                        correlation,
                    );
                    return Ok(());
                }
                Script::Stall(d) => {
                    st.record(
                        &id,
                        "stalled_at_dot",
                        listener,
                        conn,
                        &peer_ip,
                        mismatch,
                        correlation,
                    );
                    tokio::time::sleep(d).await;
                    reply(&mut io, "250 2.0.0 stored (late)\r\n").await?;
                }
                Script::Slow(d) => {
                    tokio::time::sleep(d).await;
                    deliver(
                        st,
                        &id,
                        listener,
                        conn,
                        &peer_ip,
                        mismatch,
                        correlation,
                        &mut io,
                    )
                    .await?;
                }
                Script::None => {
                    deliver(
                        st,
                        &id,
                        listener,
                        conn,
                        &peer_ip,
                        mismatch,
                        correlation,
                        &mut io,
                    )
                    .await?
                }
            }
            rcpt = None;
        } else if upper.starts_with("RSET") {
            rcpt = None;
            reply(&mut io, "250 2.0.0 ok\r\n").await?;
        } else if upper.starts_with("NOOP") {
            reply(&mut io, "250 2.0.0 ok\r\n").await?;
        } else if upper.starts_with("QUIT") {
            reply(&mut io, "221 2.0.0 bye\r\n").await?;
            return Ok(());
        } else {
            reply(&mut io, "500 5.5.2 unrecognised\r\n").await?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn deliver(
    st: &State,
    id: &str,
    listener: &str,
    conn: u64,
    peer: &str,
    mismatch: bool,
    correlation: Option<&str>,
    io: &mut BufReader<TcpStream>,
) -> std::io::Result<()> {
    let n = st.delivered_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if st
        .opts
        .lose_every
        .is_some_and(|every| every > 0 && n.is_multiple_of(every))
    {
        st.outcomes.lost.fetch_add(1, Ordering::SeqCst);
    } else {
        st.record(id, "delivered", listener, conn, peer, mismatch, correlation);
    }
    reply(io, "250 2.0.0 stored\r\n").await
}

/// Read to the terminating dot, keeping only the three headers the sink needs:
/// `X-Test-Id`, `X-Simmer-Correlation` (where a route stamps one) and
/// `X-Sink-Script`. The body is discarded as it streams, so a
/// 25 MiB message costs one line buffer, not 25 MiB.
async fn read_body(
    io: &mut BufReader<TcpStream>,
) -> std::io::Result<(Option<String>, Option<String>, Script)> {
    let mut in_headers = true;
    let mut test_id = None;
    let mut correlation = None;
    let mut script = Script::None;
    let mut line = Vec::with_capacity(1024);
    loop {
        line.clear();
        if io.read_until(b'\n', &mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF in DATA",
            ));
        }
        let content = line.strip_suffix(b"\n").unwrap_or(&line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content == b"." {
            return Ok((test_id, correlation, script));
        }
        if in_headers {
            if content.is_empty() {
                in_headers = false;
                continue;
            }
            let text = String::from_utf8_lossy(content);
            if let Some((name, value)) = text.split_once(':') {
                if name.eq_ignore_ascii_case("x-test-id") {
                    test_id = Some(value.trim().to_string());
                } else if name.eq_ignore_ascii_case("x-simmer-correlation") {
                    correlation = Some(value.trim().to_string());
                } else if name.eq_ignore_ascii_case("x-sink-script") {
                    script = parse_script(value);
                }
            }
        }
    }
}

/// The loadgen's recipients are `<id>@<domain>`; anything else is its own id.
fn id_of(addr: &str) -> String {
    addr.rsplit_once('@')
        .map(|(local, _)| local.to_string())
        .unwrap_or_else(|| addr.to_string())
}

async fn reply(io: &mut BufReader<TcpStream>, s: &str) -> std::io::Result<()> {
    io.get_mut().write_all(s.as_bytes()).await?;
    io.get_mut().flush().await
}

fn between(s: &str, open: char, close: char) -> Option<String> {
    let start = s.find(open)? + 1;
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
