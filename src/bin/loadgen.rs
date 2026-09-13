//! The test programme's load generator (`docs/ACCEPTANCE.md` §2, and the load
//! tiers of `docs/TESTING.md`).
//!
//! **Not part of the shipped image.** `Dockerfile`'s `acceptance` stage adds it;
//! the `runtime` stage does not, and only the `acceptance` compose profile
//! builds that stage.
//!
//! It runs *inside* the compose network because §2.3 says port 25 must not be
//! published to a host interface, and `docker-compose.yml` deliberately does not
//! publish it. Sending from a container is also the realistic topology: an
//! application talking to a relay over a private network.
//!
//! Deliberately dumb. It sends, and records what it was told. **It asserts
//! nothing** — every assertion lives in the tests, where a failure is legible and
//! where the expected numbers come from the config under test rather than from
//! here.
//!
//! ## Two ways to run it
//!
//! **As the acceptance suite always has:** one connection per message, one at a
//! time, and one JSON array of `{recipient, code, text}` printed at the end. With
//! no new flags the bytes it sends are exactly what they were, which matters: the
//! §1.1 cutover test compares two runs byte for byte.
//!
//! **As a load generator:** `--concurrency`, an open-loop `--rate`, a
//! `--duration`, size distributions, TLS modes, persistent sessions, and a
//! streaming `--jsonl` record of every message. For load, `--stamp` gives every
//! message a test id — in the RCPT local part and an `X-Test-Id` header — which
//! `tests/compose/reconcile.rs` joins with the sink's record of the same id. It is
//! opt-in precisely because an extra header would break the cutover comparison.
//!
//! **Latency in open-loop mode is measured from the scheduled send time**, not
//! from when the send actually began. A server that stalls delays the sends
//! queued behind it, and measuring from the actual start would hide exactly the
//! latency the stall caused (coordinated omission).
//!
//! A refusal is recorded as the reply it was — `530` at AUTH is code 530 with
//! stage `auth` — and code 0 is kept for transport failures, where there was no
//! reply at all. The first loadgen folded both into code 0.
//!
//! ## Why it speaks SMTP by hand
//!
//! The same reason `src/downstream/client.rs` does (D-022): client crates
//! normalise the reply into an error type, and this tool's entire output is the
//! reply codes. It reuses the library's own [`Stream`] for STARTTLS and implicit
//! TLS, verifying against `--ca` (a PEM file, or `os` for the platform store) —
//! an unverified handshake would show the bytes were encrypted, not that Simmer
//! served the certificate it was configured with.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use simmer::downstream::stream::Stream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain,
    Starttls,
    Implicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMech {
    Plain,
    Login,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Utf8,
    Latin1,
    Cp1252,
}

/// What each client does. `Send` is the load generator; the rest are the silent
/// and slow clients of the stress tier's S1 and S8 — one connection per
/// "message", and never a message delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Send mail: everything the loadgen did before this flag existed.
    Send,
    /// Read the banner, then say nothing at all.
    Silent,
    /// Greet and authenticate, then `NOOP` every `--idle-every`.
    NoopIdle,
    /// Reach `DATA`, then send one byte every `--trickle-every`.
    DataTrickle,
    /// Reach `DATA`, then send `--no-lf-bytes` without a line ending.
    NoLf,
}

#[derive(Debug, Clone)]
struct Args {
    host: String,
    port: u16,
    username: String,
    password: String,
    count: Option<u64>,
    duration: Option<Duration>,
    concurrency: usize,
    rate: Option<f64>,
    per_session: usize,
    from: String,
    from_header: String,
    /// Draw every message's sender from a domain nobody has seen before,
    /// `u<n>.soak.test`, in the envelope **and** the `From:` header.
    ///
    /// Both, because `relay.rs` labels `simmer_unmatched_sender_total` with the
    /// `From:` header's domain and only falls back to the envelope's — varying
    /// the envelope alone would emit one series forever and read as F7 failing
    /// to reproduce against an untouched defect.
    fresh_sender: bool,
    recipient_domain: String,
    tag: String,
    subject: String,
    mode: Mode,
    ca: String,
    tls_name: String,
    auth: AuthMech,
    wrong_password_pct: f64,
    pipelining: bool,
    stamp: bool,
    sink_script: Option<String>,
    sink_script_pct: f64,
    sizes: Vec<(usize, f64)>,
    charset: Charset,
    jsonl: Option<String>,
    seed: u64,
    helo: String,
    /// A message read whole from stdin, sent verbatim (after CRLF and
    /// dot-stuffing) in place of the generated one.
    raw: Option<Vec<u8>>,
    behaviour: Behaviour,
    /// How long a misbehaving client waits for the server to end it.
    hold: Duration,
    idle_every: Duration,
    trickle_every: Duration,
    no_lf_bytes: usize,
    /// Hold every connection open and authenticate at this offset from the run's
    /// start, so the verifies coincide rather than being staggered by each
    /// client's own handshake. Zero authenticates as soon as the client is ready.
    auth_delay: Duration,
}

fn args() -> Args {
    let mut a = Args {
        host: "app".to_string(),
        port: 25,
        username: "cfapp".to_string(),
        password: String::new(),
        count: None,
        duration: None,
        concurrency: 1,
        rate: None,
        per_session: 1,
        from: "jane@oldbrand.com".to_string(),
        from_header: "Jane Smith <jane@oldbrand.com>".to_string(),
        fresh_sender: false,
        recipient_domain: "example.net".to_string(),
        tag: "run".to_string(),
        subject: "Your order has shipped".to_string(),
        mode: Mode::Plain,
        ca: "/tls/ca.pem".to_string(),
        tls_name: "simmer.acceptance".to_string(),
        auth: AuthMech::Plain,
        wrong_password_pct: 0.0,
        pipelining: false,
        stamp: false,
        sink_script: None,
        sink_script_pct: 100.0,
        sizes: Vec::new(),
        charset: Charset::Utf8,
        jsonl: None,
        seed: 1,
        helo: "loadgen.acceptance".to_string(),
        raw: None,
        behaviour: Behaviour::Send,
        hold: Duration::from_secs(300),
        idle_every: Duration::from_secs(5),
        trickle_every: Duration::from_secs(1),
        no_lf_bytes: 512 * 1024 * 1024,
        auth_delay: Duration::ZERO,
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        // The flags that take no value.
        match argv[i].as_str() {
            "--starttls" => {
                a.mode = Mode::Starttls;
                i += 1;
                continue;
            }
            "--pipelining" => {
                a.pipelining = true;
                i += 1;
                continue;
            }
            "--stamp" => {
                a.stamp = true;
                i += 1;
                continue;
            }
            "--fresh-sender" => {
                a.fresh_sender = true;
                i += 1;
                continue;
            }
            "--raw-stdin" => {
                let mut raw = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut raw).expect("stdin");
                a.raw = Some(raw);
                i += 1;
                continue;
            }
            _ => {}
        }
        let value = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{} needs a value", argv[i]))
                .clone()
        };
        match argv[i].as_str() {
            "--host" => a.host = value(),
            "--port" => a.port = value().parse().expect("--port"),
            "--username" => a.username = value(),
            "--password" => a.password = value(),
            "--count" => a.count = Some(value().parse().expect("--count")),
            "--duration" => a.duration = Some(parse_duration(&value())),
            "--concurrency" => a.concurrency = value().parse().expect("--concurrency"),
            "--rate" => a.rate = Some(value().parse().expect("--rate")),
            "--per-session" => a.per_session = value().parse().expect("--per-session"),
            "--from" => a.from = value(),
            "--from-header" => a.from_header = value(),
            "--recipient-domain" => a.recipient_domain = value(),
            "--tag" => a.tag = value(),
            "--subject" => a.subject = value(),
            "--mode" => {
                a.mode = match value().as_str() {
                    "plain" => Mode::Plain,
                    "starttls" => Mode::Starttls,
                    "implicit" => Mode::Implicit,
                    other => panic!("--mode {other}: plain, starttls or implicit"),
                }
            }
            "--ca" => a.ca = value(),
            "--tls-name" => a.tls_name = value(),
            "--helo" => a.helo = value(),
            "--behaviour" => {
                a.behaviour = match value().as_str() {
                    "send" => Behaviour::Send,
                    "silent" => Behaviour::Silent,
                    "noop-idle" => Behaviour::NoopIdle,
                    "data-trickle" => Behaviour::DataTrickle,
                    "no-lf" => Behaviour::NoLf,
                    other => panic!(
                        "--behaviour {other}: send, silent, noop-idle, data-trickle or no-lf"
                    ),
                }
            }
            "--hold" => a.hold = parse_duration(&value()),
            "--idle-every" => a.idle_every = parse_duration(&value()),
            "--trickle-every" => a.trickle_every = parse_duration(&value()),
            "--no-lf-bytes" => a.no_lf_bytes = parse_sizes(&value())[0].0,
            "--auth-delay" => a.auth_delay = parse_duration(&value()),
            "--auth" => {
                a.auth = match value().as_str() {
                    "plain" => AuthMech::Plain,
                    "login" => AuthMech::Login,
                    "none" => AuthMech::None,
                    other => panic!("--auth {other}: plain, login or none"),
                }
            }
            "--wrong-password-pct" => a.wrong_password_pct = value().parse().expect("a percentage"),
            "--sink-script" => a.sink_script = Some(value()),
            "--sink-script-pct" => a.sink_script_pct = value().parse().expect("a percentage"),
            "--size" => a.sizes = parse_sizes(&value()),
            "--charset" => {
                a.charset = match value().as_str() {
                    "utf8" | "utf-8" => Charset::Utf8,
                    "latin1" | "iso-8859-1" => Charset::Latin1,
                    "cp1252" | "windows-1252" => Charset::Cp1252,
                    other => panic!("--charset {other}: utf8, latin1 or cp1252"),
                }
            }
            "--jsonl" => a.jsonl = Some(value()),
            "--seed" => a.seed = value().parse().expect("--seed"),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }

    if a.password.is_empty() {
        a.password = std::env::var("SIMMER_PASSWORD").unwrap_or_default();
    }
    if a.count.is_none() && a.duration.is_none() {
        a.count = Some(1);
    }
    assert!(a.concurrency >= 1, "--concurrency must be at least 1");
    assert!(a.per_session >= 1, "--per-session must be at least 1");
    a
}

/// `90s`, `10m`, `1h`, `1500ms`, or bare seconds.
fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    let (n, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| s.split_at(i))
        .unwrap_or((s, "s"));
    let n: f64 = n.parse().unwrap_or_else(|_| panic!("duration {s}"));
    Duration::from_secs_f64(match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        other => panic!("duration unit {other}"),
    })
}

/// `4096`, `2m`, or a weighted distribution `dist:4k:80,100k:15,2m:4,15m:1`.
fn parse_sizes(s: &str) -> Vec<(usize, f64)> {
    let bytes = |v: &str| -> usize {
        let v = v.trim().to_ascii_lowercase();
        let (n, mult) = if let Some(n) = v.strip_suffix('k') {
            (n, 1024)
        } else if let Some(n) = v.strip_suffix('m') {
            (n, 1024 * 1024)
        } else {
            (v.as_str(), 1)
        };
        n.parse::<usize>().unwrap_or_else(|_| panic!("size {v}")) * mult
    };
    match s.strip_prefix("dist:") {
        Some(spec) => spec
            .split(',')
            .map(|part| {
                let (size, weight) = part
                    .rsplit_once(':')
                    .unwrap_or_else(|| panic!("size {part}"));
                (
                    bytes(size),
                    weight.parse().unwrap_or_else(|_| panic!("weight {part}")),
                )
            })
            .collect(),
        None => vec![(bytes(s), 1.0)],
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// What one message was told, as recorded.
#[derive(Debug, Clone)]
struct Rec {
    id: String,
    recipient: String,
    code: u16,
    stage: &'static str,
    text: String,
    latency_ms: f64,
    sent_ms: u128,
}

#[tokio::main]
async fn main() {
    let args = Arc::new(args());
    let tls = (args.mode != Mode::Plain).then(|| Arc::new(connector(&args.ca)));
    let started = Instant::now();

    let (tx, rx) = mpsc::unbounded_channel::<Rec>();
    let collector = tokio::spawn(collect(Arc::clone(&args), rx, started));

    let seq = Arc::new(AtomicU64::new(0));
    let limit = args.count.unwrap_or(u64::MAX);
    let deadline = args.duration.map(|d| started + d);
    let time_left = move || deadline.is_none_or(|d| Instant::now() < d);

    if let Some(rate) = args.rate {
        // Open loop: sessions are *scheduled* at the rate, whatever the server
        // is doing, and in-flight sessions are capped at --concurrency.
        let sessions_per_sec = rate / args.per_session as f64;
        let interval = Duration::from_secs_f64(1.0 / sessions_per_sec);
        let permits = Arc::new(Semaphore::new(args.concurrency));
        let mut next = tokio::time::Instant::now();
        let mut running = tokio::task::JoinSet::new();
        loop {
            if !time_left() {
                break;
            }
            let ids = take(&seq, args.per_session, limit);
            if ids.is_empty() {
                break;
            }
            tokio::time::sleep_until(next).await;
            let scheduled = next.into_std();
            next += interval;
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .expect("semaphore");
            let (args, tls, tx) = (Arc::clone(&args), tls.clone(), tx.clone());
            running.spawn(async move {
                session(&args, tls.as_deref(), &ids, scheduled, started, &tx).await;
                drop(permit);
            });
        }
        while running.join_next().await.is_some() {}
    } else {
        // Closed loop: --concurrency workers, each sending back to back.
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..args.concurrency {
            let (args, tls, tx, seq) =
                (Arc::clone(&args), tls.clone(), tx.clone(), Arc::clone(&seq));
            workers.spawn(async move {
                while time_left() {
                    let ids = take(&seq, args.per_session, limit);
                    if ids.is_empty() {
                        break;
                    }
                    session(&args, tls.as_deref(), &ids, Instant::now(), started, &tx).await;
                }
            });
        }
        while workers.join_next().await.is_some() {}
    }

    drop(tx);
    let summary = collector.await.expect("collector");
    println!("{summary}");
}

/// The next `n` message numbers, stopping at `limit`.
fn take(seq: &AtomicU64, n: usize, limit: u64) -> Vec<u64> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let v = seq.fetch_add(1, Ordering::SeqCst);
        if v >= limit {
            break;
        }
        out.push(v);
    }
    out
}

/// Stream every record: to the JSONL file as it arrives, or — the acceptance
/// suite's format — into one JSON array printed when the run ends. Returns the
/// summary line.
async fn collect(
    args: Arc<Args>,
    mut rx: mpsc::UnboundedReceiver<Rec>,
    started: Instant,
) -> String {
    use std::io::Write;

    let mut file = args.jsonl.as_ref().map(|p| {
        std::io::BufWriter::new(
            std::fs::File::create(p).unwrap_or_else(|e| panic!("creating {p}: {e}")),
        )
    });
    let mut legacy: Vec<String> = Vec::new();
    let mut latencies: Vec<f64> = Vec::new();
    let (mut accepted, mut deferred, mut refused, mut transport) = (0u64, 0u64, 0u64, 0u64);

    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        for r in batch {
            match r.code {
                0 => transport += 1,
                200..=299 => accepted += 1,
                400..=499 => deferred += 1,
                _ => refused += 1,
            }
            if r.code != 0 {
                latencies.push(r.latency_ms);
            }
            match file.as_mut() {
                Some(f) => {
                    let _ = writeln!(
                        f,
                        r#"{{"id":{},"recipient":{},"code":{},"stage":"{}","text":{},"latency_ms":{:.3},"sent_ms":{}}}"#,
                        json_string(&r.id),
                        json_string(&r.recipient),
                        r.code,
                        r.stage,
                        json_string(&r.text),
                        r.latency_ms,
                        r.sent_ms
                    );
                }
                None => legacy.push(format!(
                    r#"{{"recipient":{},"code":{},"text":{}}}"#,
                    json_string(&r.recipient),
                    r.code,
                    json_string(&r.text)
                )),
            }
        }
        if let Some(f) = file.as_mut() {
            let _ = f.flush();
        }
    }

    if file.is_none() {
        println!("[{}]", legacy.join(","));
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let pct = |p: f64| -> f64 {
        if latencies.is_empty() {
            return 0.0;
        }
        let i = ((p / 100.0) * (latencies.len() - 1) as f64).round() as usize;
        latencies[i]
    };
    let elapsed = started.elapsed().as_secs_f64();
    let total = accepted + deferred + refused + transport;
    format!(
        r#"{{"summary":{{"messages":{total},"accepted":{accepted},"deferred":{deferred},"refused":{refused},"transport":{transport},"elapsed_s":{elapsed:.3},"per_sec":{:.2},"p50_ms":{:.1},"p90_ms":{:.1},"p99_ms":{:.1},"max_ms":{:.1}}}}}"#,
        total as f64 / elapsed.max(0.001),
        pct(50.0),
        pct(90.0),
        pct(99.0),
        latencies.last().copied().unwrap_or(0.0)
    )
}

// ---------------------------------------------------------------------------
// one session
// ---------------------------------------------------------------------------

/// A transport failure: which message it cut off, at which stage, and why.
struct Cut {
    at: usize,
    stage: &'static str,
    text: String,
}

async fn session(
    args: &Args,
    tls: Option<&tokio_rustls::TlsConnector>,
    numbers: &[u64],
    scheduled: Instant,
    started: Instant,
    tx: &mpsc::UnboundedSender<Rec>,
) {
    if let Err(cut) = converse(args, tls, numbers, scheduled, started, tx).await {
        for (k, n) in numbers.iter().enumerate().skip(cut.at) {
            let (id, recipient) = identity(args, *n);
            let _ = tx.send(Rec {
                id,
                recipient,
                code: 0,
                stage: if k == cut.at { cut.stage } else { "not_sent" },
                text: cut.text.clone(),
                latency_ms: scheduled.elapsed().as_secs_f64() * 1000.0,
                sent_ms: scheduled.duration_since(started).as_millis(),
            });
        }
    }
}

fn identity(args: &Args, n: u64) -> (String, String) {
    let id = format!("{}-{n}", args.tag);
    let recipient = format!("{id}@{}", args.recipient_domain);
    (id, recipient)
}

/// This message's envelope sender and `From:` header.
///
/// With `--fresh-sender`, one message in twenty comes from `u<n>.soak.test` — a
/// domain the server has never seen and never will again. The ACL still grants it
/// (`*.soak.test`), so the message is accepted; no `senders:` rule matches it, so
/// it falls through to `default_chain` and mints a new
/// `simmer_unmatched_sender_total{domain}` series on the way. That is F7, driven
/// rather than argued about.
///
/// One in twenty, not all of them: at 10 msg/s for an hour, every message would
/// be ~34,000 series rather than ~1,700, which stops being a measurement of the
/// defect and becomes a cardinality bomb of the test's own making — it would be
/// the soak's memory and scrape time under test, not Simmer's.
fn sender(args: &Args, n: u64) -> (String, String) {
    if !args.fresh_sender || !n.is_multiple_of(FRESH_SENDER_EVERY) {
        return (args.from.clone(), args.from_header.clone());
    }
    let envelope = format!("jane@u{n}.soak.test");
    let header = format!("Jane Smith <{envelope}>");
    (envelope, header)
}

/// One message in twenty carries a never-before-seen sender domain: the plan's 5%.
const FRESH_SENDER_EVERY: u64 = 20;

fn cut(at: usize, stage: &'static str) -> impl Fn(String) -> Cut {
    move |text| Cut { at, stage, text }
}

async fn converse(
    args: &Args,
    tls: Option<&tokio_rustls::TlsConnector>,
    numbers: &[u64],
    scheduled: Instant,
    started: Instant,
    tx: &mpsc::UnboundedSender<Rec>,
) -> Result<(), Cut> {
    // Every message in the session gets the same verdict when the session is
    // refused before its first transaction.
    let refuse_all = |code: u16, stage: &'static str, text: String| {
        for n in numbers {
            let (id, recipient) = identity(args, *n);
            let _ = tx.send(Rec {
                id,
                recipient,
                code,
                stage,
                text: text.clone(),
                latency_ms: scheduled.elapsed().as_secs_f64() * 1000.0,
                sent_ms: scheduled.duration_since(started).as_millis(),
            });
        }
    };

    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((args.host.as_str(), args.port)),
    )
    .await
    .map_err(|_| cut(0, "transport")("connect timed out".to_string()))?
    .map_err(|e| cut(0, "transport")(format!("connect: {e}")))?;

    let stream = if args.mode == Mode::Implicit {
        let connector = tls.expect("implicit mode has a connector");
        let tls = connector
            .connect(server_name(args), tcp)
            .await
            .map_err(|e| cut(0, "transport")(format!("TLS handshake: {e}")))?;
        Stream::Tls(Box::new(tls.into()))
    } else {
        Stream::Plain(tcp)
    };
    let mut io = BufReader::new(stream);

    let (code, text) = read_reply(&mut io).await.map_err(cut(0, "transport"))?;
    if code != 220 {
        refuse_all(code, "banner", text);
        return Ok(());
    }

    if args.behaviour == Behaviour::Silent {
        let (code, text) = await_reply(&mut io, args.hold).await;
        refuse_all(code, "idle", text);
        return Ok(());
    }

    ehlo(&mut io, &args.helo)
        .await
        .map_err(cut(0, "transport"))?;

    if args.mode == Mode::Starttls {
        write(&mut io, "STARTTLS\r\n")
            .await
            .map_err(cut(0, "transport"))?;
        let (code, text) = read_reply(&mut io).await.map_err(cut(0, "transport"))?;
        if code != 220 {
            refuse_all(code, "tls", text);
            return Ok(());
        }
        let Stream::Plain(tcp) = std::mem::replace(io.get_mut(), Stream::Taken) else {
            return Err(cut(0, "transport")(
                "STARTTLS on a stream that is not plaintext".into(),
            ));
        };
        let connector = tls.expect("starttls mode has a connector");
        let upgraded = connector
            .connect(server_name(args), tcp)
            .await
            .map_err(|e| cut(0, "transport")(format!("TLS handshake: {e}")))?;
        io = BufReader::new(Stream::Tls(Box::new(upgraded.into())));
        // RFC 3207 §4.2: everything learned before the handshake is void.
        ehlo(&mut io, &args.helo)
            .await
            .map_err(cut(0, "transport"))?;
    }

    // A synchronised burst. Every client is connected and greeted by now, so
    // waiting for one shared instant puts all the argon2 verifies in flight at
    // once — the only shape that decouples what an arrival costs from what a
    // verify costs, since otherwise both are paid on the same CPUs.
    if !args.auth_delay.is_zero() {
        tokio::time::sleep_until((started + args.auth_delay).into()).await;
    }

    if args.auth != AuthMech::None && !args.password.is_empty() {
        let wrong = unit(args.seed, numbers[0], 1) * 100.0 < args.wrong_password_pct;
        let password = if wrong {
            "not-the-password"
        } else {
            args.password.as_str()
        };
        let (code, text) = auth(&mut io, args.auth, &args.username, password)
            .await
            .map_err(cut(0, "transport"))?;
        if code != 235 {
            refuse_all(code, "auth", text);
            let _ = write(&mut io, "QUIT\r\n").await;
            return Ok(());
        }
    }

    match args.behaviour {
        Behaviour::Send | Behaviour::Silent => {}
        Behaviour::NoopIdle => {
            let (code, text) = noop_until_told(&mut io, args).await;
            refuse_all(code, "idle", text);
            return Ok(());
        }
        Behaviour::DataTrickle | Behaviour::NoLf => {
            let (_, recipient) = identity(args, numbers[0]);
            let (code, stage, text) = misbehave_in_data(&mut io, args, &recipient).await;
            refuse_all(code, stage, text);
            return Ok(());
        }
    }

    for (k, n) in numbers.iter().enumerate() {
        let began = if k == 0 { scheduled } else { Instant::now() };
        let (id, recipient) = identity(args, *n);
        let record = |code: u16, stage: &'static str, text: String| {
            let _ = tx.send(Rec {
                id: id.clone(),
                recipient: recipient.clone(),
                code,
                stage,
                text,
                latency_ms: began.elapsed().as_secs_f64() * 1000.0,
                sent_ms: began.duration_since(started).as_millis(),
            });
        };

        if k > 0 {
            write(&mut io, "RSET\r\n")
                .await
                .map_err(cut(k, "transport"))?;
            read_reply(&mut io).await.map_err(cut(k, "transport"))?;
        }

        let body = message(args, &id, &recipient, *n);
        let raw_8bit = args
            .raw
            .as_ref()
            .is_some_and(|r| r.iter().any(|&b| b > 0x7F));
        let params = if args.charset != Charset::Utf8 || raw_8bit {
            " BODY=8BITMIME"
        } else {
            ""
        };
        let mail = format!("MAIL FROM:<{}>{params}\r\n", sender(args, *n).0);
        let rcpt = format!("RCPT TO:<{recipient}>\r\n");

        let replies = if args.pipelining {
            write(&mut io, &format!("{mail}{rcpt}DATA\r\n"))
                .await
                .map_err(cut(k, "transport"))?;
            let mut r = Vec::with_capacity(3);
            for _ in 0..3 {
                r.push(read_reply(&mut io).await.map_err(cut(k, "transport"))?);
            }
            r
        } else {
            let mut r = Vec::with_capacity(3);
            for (line, want) in [
                (mail.as_str(), 250),
                (rcpt.as_str(), 250),
                ("DATA\r\n", 354),
            ] {
                write(&mut io, line).await.map_err(cut(k, "transport"))?;
                let reply = read_reply(&mut io).await.map_err(cut(k, "transport"))?;
                let stop = reply.0 != want;
                r.push(reply);
                if stop {
                    break;
                }
            }
            r
        };

        let stages = ["mail", "rcpt", "data"];
        let wants = [250, 250, 354];
        if let Some(i) = replies.iter().zip(wants).position(|(r, w)| r.0 != w) {
            let (code, text) = replies[i].clone();
            record(code, stages[i], text);
            continue;
        }

        write_bytes(&mut io, &body)
            .await
            .map_err(cut(k, "transport"))?;
        let (code, text) = read_reply(&mut io).await.map_err(cut(k, "transport"))?;
        record(code, "dot", text);
    }

    let _ = write(&mut io, "QUIT\r\n").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// misbehaving clients (--behaviour)
// ---------------------------------------------------------------------------

/// Wait up to `hold` for the server to say something: its reply, or code 0 for a
/// close with no reply, or code 0 for silence past the hold.
async fn await_reply(io: &mut BufReader<Stream>, hold: Duration) -> (u16, String) {
    match tokio::time::timeout(hold, read_reply(io)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => (0, format!("closed without a reply: {e}")),
        Err(_) => (0, format!("still open after {hold:?}")),
    }
}

/// After a failed write, whatever the server said before it hung up — a reply
/// already sitting in the receive buffer is the one that matters.
async fn parting_words(io: &mut BufReader<Stream>, write_error: String) -> (u16, String) {
    match tokio::time::timeout(Duration::from_secs(5), read_reply(io)).await {
        Ok(Ok(reply)) => reply,
        _ => (0, format!("closed without a reply: {write_error}")),
    }
}

/// `NOOP` every `--idle-every` until the server ends the session.
async fn noop_until_told(io: &mut BufReader<Stream>, args: &Args) -> (u16, String) {
    let deadline = Instant::now() + args.hold;
    loop {
        tokio::time::sleep(args.idle_every).await;
        if Instant::now() > deadline {
            return (0, format!("still open after {:?}", args.hold));
        }
        if let Err(e) = write(io, "NOOP\r\n").await {
            return parting_words(io, e).await;
        }
        match read_reply(io).await {
            Ok((250, _)) => {}
            Ok(reply) => return reply,
            Err(e) => return (0, format!("closed without a reply: {e}")),
        }
    }
}

/// Reach `DATA`, then trickle a byte at a time or send one endless line.
async fn misbehave_in_data(
    io: &mut BufReader<Stream>,
    args: &Args,
    recipient: &str,
) -> (u16, &'static str, String) {
    for (line, want, stage) in [
        (format!("MAIL FROM:<{}>\r\n", args.from), 250, "mail"),
        (format!("RCPT TO:<{recipient}>\r\n"), 250, "rcpt"),
        ("DATA\r\n".to_string(), 354, "data"),
    ] {
        if let Err(e) = write(io, &line).await {
            return (0, "transport", e);
        }
        match read_reply(io).await {
            Ok((code, _)) if code == want => {}
            Ok((code, text)) => return (code, stage, text),
            Err(e) => return (0, "transport", e),
        }
    }
    let deadline = Instant::now() + args.hold;

    if args.behaviour == Behaviour::NoLf {
        let chunk = vec![b'x'; 64 * 1024];
        let mut sent = 0usize;
        while sent < args.no_lf_bytes {
            let n = chunk.len().min(args.no_lf_bytes - sent);
            if let Err(e) = write_bytes(io, &chunk[..n]).await {
                let (code, text) = parting_words(io, e).await;
                return (code, "data", format!("{text} (after {sent} bytes)"));
            }
            sent += n;
        }
        let (code, text) =
            await_reply(io, deadline.saturating_duration_since(Instant::now())).await;
        return (code, "data", format!("{text} (after {sent} bytes)"));
    }

    loop {
        if Instant::now() > deadline {
            return (0, "data", format!("still open after {:?}", args.hold));
        }
        if let Err(e) = write_bytes(io, b"x").await {
            let (code, text) = parting_words(io, e).await;
            return (code, "data", text);
        }
        // Wait one interval for a reply before the next byte. A reply line arrives
        // in one segment, so abandoning a read that has seen nothing loses nothing.
        match tokio::time::timeout(args.trickle_every, read_reply(io)).await {
            Ok(Ok((code, text))) => return (code, "data", text),
            Ok(Err(e)) => return (0, "data", format!("closed without a reply: {e}")),
            Err(_) => {}
        }
    }
}

fn server_name(args: &Args) -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from(args.tls_name.clone())
        .unwrap_or_else(|e| panic!("--tls-name {}: {e}", args.tls_name))
}

async fn ehlo(io: &mut BufReader<Stream>, helo: &str) -> Result<(), String> {
    write(io, &format!("EHLO {helo}\r\n")).await?;
    let (code, text) = read_reply(io).await?;
    if code != 250 {
        return Err(format!("EHLO: {code} {text}"));
    }
    Ok(())
}

async fn auth(
    io: &mut BufReader<Stream>,
    mech: AuthMech,
    username: &str,
    password: &str,
) -> Result<(u16, String), String> {
    match mech {
        AuthMech::Login => {
            write(io, "AUTH LOGIN\r\n").await?;
            let (code, text) = read_reply(io).await?;
            if code != 334 {
                return Ok((code, text));
            }
            write(io, &format!("{}\r\n", base64_encode(username.as_bytes()))).await?;
            let (code, text) = read_reply(io).await?;
            if code != 334 {
                return Ok((code, text));
            }
            write(io, &format!("{}\r\n", base64_encode(password.as_bytes()))).await?;
            read_reply(io).await
        }
        _ => {
            // AUTH PLAIN: NUL authzid, NUL-separated (RFC 4616).
            let payload = format!("\0{username}\0{password}");
            write(
                io,
                &format!("AUTH PLAIN {}\r\n", base64_encode(payload.as_bytes())),
            )
            .await?;
            read_reply(io).await
        }
    }
}

/// The message, dot-terminated and ready for the wire.
///
/// With no size, charset, stamp or script options the bytes are exactly the
/// first loadgen's — the §1.1 cutover test compares two runs byte for byte.
fn message(args: &Args, id: &str, recipient: &str, n: u64) -> Vec<u8> {
    if let Some(raw) = &args.raw {
        return wire_form(raw);
    }
    let (charset, extra_body): (&str, &[u8]) = match args.charset {
        Charset::Utf8 => ("utf-8", b""),
        // "Café crème, naïve" in each single-byte charset; CP1252 adds an em dash
        // and a euro sign, which ISO-8859-1 does not have.
        Charset::Latin1 => ("iso-8859-1", b"Caf\xe9 cr\xe8me, na\xefve\r\n"),
        Charset::Cp1252 => ("windows-1252", b"Caf\xe9 cr\xe8me \x97 na\xefve, \x805\r\n"),
    };

    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(
        format!(
            "From: {}\r\n\
             To: {recipient}\r\n\
             Subject: {}\r\n\
             Message-ID: <{}-{}@oldbrand.com>\r\n\
             Return-Path: <bounces@oldbrand.com>\r\n\
             X-Mailer: AcceptanceApp 1.0\r\n\
             DKIM-Signature: v=1; a=rsa-sha256; d=oldbrand.com; s=s1; b=notarealsignature\r\n\
             Authentication-Results: mx.oldbrand.com; spf=pass\r\n\
             ARC-Seal: i=1; cv=none; d=oldbrand.com\r\n\
             MIME-Version: 1.0\r\n",
            sender(args, n).1,
            args.subject,
            args.tag,
            recipient.split('@').next().unwrap_or("x"),
        )
        .as_bytes(),
    );
    if args.stamp {
        out.extend_from_slice(format!("X-Test-Id: {id}\r\n").as_bytes());
    }
    if let Some(script) = &args.sink_script {
        if unit(args.seed, n, 2) * 100.0 < args.sink_script_pct {
            out.extend_from_slice(format!("X-Sink-Script: {script}\r\n").as_bytes());
        }
    }
    out.extend_from_slice(format!("Content-Type: text/plain; charset={charset}\r\n").as_bytes());
    if args.charset != Charset::Utf8 {
        out.extend_from_slice(b"Content-Transfer-Encoding: 8bit\r\n");
    }
    out.extend_from_slice(
        b"\r\nYour order has shipped.\r\nTrack it at https://oldbrand.com/track\r\n",
    );
    out.extend_from_slice(extra_body);

    if let Some(target) = pick_size(args, n) {
        // Plain 76-column lines: never a leading dot, so no stuffing needed.
        let line = [b'x'; 76];
        while out.len() < target {
            out.extend_from_slice(&line);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// A `--raw-stdin` message as it goes on the wire: CRLF line endings,
/// dot-stuffed, terminated. The input is whatever a test wrote, in either line
/// ending.
fn wire_form(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 64);
    let text = raw.strip_suffix(b"\n").unwrap_or(raw);
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

fn pick_size(args: &Args, n: u64) -> Option<usize> {
    if args.sizes.is_empty() {
        return None;
    }
    let total: f64 = args.sizes.iter().map(|(_, w)| w).sum();
    let mut roll = unit(args.seed, n, 3) * total;
    for (size, weight) in &args.sizes {
        if roll < *weight {
            return Some(*size);
        }
        roll -= weight;
    }
    args.sizes.last().map(|(s, _)| *s)
}

/// A deterministic number in `[0, 1)` for message `n` — SplitMix64 over the seed,
/// the message number and a salt per decision, so a run is reproducible and each
/// decision independent of the others.
fn unit(seed: u64, n: u64, salt: u64) -> f64 {
    let mut z = seed
        .wrapping_add(n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(salt.wrapping_mul(0xD1B5_4A32_D192_ED03));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// A client that trusts `path` (a PEM CA) or, for `os`, the platform store —
/// the same lookup the server's `required_verify` routes use. A panic rather
/// than a reported error: without trust there is no run to report on.
fn connector(path: &str) -> tokio_rustls::TlsConnector {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    if path == "os" {
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        assert!(!roots.is_empty(), "--ca os: the platform store is empty");
    } else {
        let ca = rustls::pki_types::CertificateDer::from_pem_file(path)
            .unwrap_or_else(|e| panic!("reading --ca {path}: {e}"));
        roots.add(ca).expect("--ca is not a usable trust anchor");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

// ---------------------------------------------------------------------------
// a very small SMTP client
// ---------------------------------------------------------------------------

async fn write(io: &mut BufReader<Stream>, s: &str) -> Result<(), String> {
    write_bytes(io, s.as_bytes()).await
}

async fn write_bytes(io: &mut BufReader<Stream>, b: &[u8]) -> Result<(), String> {
    io.get_mut()
        .write_all(b)
        .await
        .map_err(|e| format!("write: {e}"))?;
    io.get_mut()
        .flush()
        .await
        .map_err(|e| format!("flush: {e}"))
}

async fn read_line(io: &mut BufReader<Stream>) -> Result<String, String> {
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(30), io.read_line(&mut line)).await {
        Err(_) => Err("read timed out".to_string()),
        Ok(Err(e)) => Err(format!("read: {e}")),
        Ok(Ok(0)) => Err("connection closed".to_string()),
        Ok(Ok(_)) => Ok(line),
    }
}

/// Read one reply, following multi-line continuations; the text is the last
/// line's.
async fn read_reply(io: &mut BufReader<Stream>) -> Result<(u16, String), String> {
    loop {
        let line = read_line(io).await?;
        let trimmed = line.trim_end();
        if trimmed.len() < 3 {
            return Err(format!("short reply: {trimmed}"));
        }
        let code: u16 = trimmed[..3]
            .parse()
            .map_err(|_| format!("unparseable reply: {trimmed}"))?;
        if trimmed.as_bytes().get(3) == Some(&b'-') {
            continue;
        }
        return Ok((code, trimmed.get(4..).unwrap_or_default().to_string()));
    }
}

/// Base64 without a dependency — `base64` is a runtime dependency of the library
/// and would be available, but this binary is deliberately standalone.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
