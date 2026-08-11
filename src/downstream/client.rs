//! The outbound SMTP conversation — hand-rolled, like the ingress side.
//!
//! `LICENSES.md` §1 records why no client crate is used: §8.2's four TLS modes,
//! §8.4's per-stage timeouts, §8.3's pool semantics and §10.2's final-dot
//! ambiguity all need control a client library abstracts away. §10.2 in
//! particular requires distinguishing "no reply was read" from "an error
//! occurred", which client crates normalise into a single error type — and that
//! distinction is the whole of §10.2.
//!
//! Phase 2 opened one connection per message (`DECISIONS.md` D-019). The
//! conversation was written against an owned [`Stream`] so that §8.3's pool could
//! wrap it in phase 10 without touching it, and that is what happened: [`relay`]
//! now checks a [`Connection`] out of [`super::pool`] instead of dialling, and
//! everything below `open` is unchanged.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::outcome::{Delivered, RelayError, Stage};
use super::pool::Pool;
use super::stream::{Stream, TlsConfigs};
use crate::config::{Route, TlsMode};
use crate::metrics;
use crate::smtp::buffer::stuff_into;

/// §8.4 defaults, used when a route declares no `timeouts` block.
///
/// "The sum of the stage budgets should be comfortably below the client's own
/// timeout" — these total 10 + 30×n + 120, which sits under a typical 10-minute
/// client budget for any realistic recipient count.
const DEFAULT_CONNECT: Duration = Duration::from_secs(10);
const DEFAULT_COMMAND: Duration = Duration::from_secs(30);
const DEFAULT_DATA: Duration = Duration::from_secs(120);

/// The longest reply line and the most continuation lines we will read.
///
/// A downstream that streams unbounded data at us instead of a reply must not be
/// able to exhaust memory in a process holding a client connection open.
const MAX_REPLY_LINE: u64 = 4096;
const MAX_REPLY_LINES: usize = 100;

/// What to relay: the envelope and the message. Phase 2 passes all of it through
/// unmodified; phase 4 rewrites between buffering and this call.
pub struct Message<'a> {
    pub mail_from: Option<&'a str>,
    pub recipients: &'a [String],
    pub body: &'a [u8],
    /// The client asked for these; the route's downstream must be able to honour
    /// them or the relay fails with [`RelayError::MissingCapability`].
    pub smtputf8: bool,
    pub body_8bitmime: bool,
}

/// Timeouts resolved for one route (§8.4).
///
/// Resolved once per route when its pool is built, rather than once per message:
/// the answer is a pure function of configuration, and §8.3's own traffic —
/// `NOOP`, `RSET`, the `QUIT` that retires a connection — needs the same budget
/// with no message in hand.
pub(super) struct Budget {
    pub(super) connect: Duration,
    pub(super) command: Duration,
    pub(super) data: Duration,
}

impl Budget {
    pub(super) fn for_route(route: &Route) -> Self {
        let t = route.downstream.timeouts.as_ref();
        Self {
            connect: t.and_then(|t| t.connect).unwrap_or(DEFAULT_CONNECT),
            command: t.and_then(|t| t.command).unwrap_or(DEFAULT_COMMAND),
            data: t.and_then(|t| t.data).unwrap_or(DEFAULT_DATA),
        }
    }
}

/// A parsed downstream reply.
#[derive(Debug, Clone)]
struct WireReply {
    code: u16,
    text: String,
    /// Capability tokens from a multiline `EHLO` response, uppercased.
    lines: Vec<String>,
}

impl WireReply {
    fn is_positive(&self) -> bool {
        (200..400).contains(&self.code)
    }

    fn advertises(&self, capability: &str) -> bool {
        self.lines.iter().any(|l| {
            l.split_whitespace()
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case(capability))
        })
    }

    fn max_size(&self) -> Option<u64> {
        self.lines.iter().find_map(|l| {
            let mut parts = l.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(k), Some(v)) if k.eq_ignore_ascii_case("SIZE") => v.parse().ok(),
                _ => None,
            }
        })
    }
}

/// Relay one message to a route's downstream over a pooled connection (§8.3).
pub async fn relay(
    route: &Route,
    tls: &TlsConfigs,
    pools: &Pool,
    hostname: &str,
    message: &Message<'_>,
) -> Result<Delivered, RelayError> {
    let pool = pools.for_route(route);
    let budget = pool.budget();
    let mut checkout = pool.checkout(route, tls).await?;

    let mut result = checkout.conn().deliver(hostname, message, budget).await;

    // The one retry, and the exact shape of it matters.
    //
    // A downstream that closed an idle connection while we held it gives EOF on
    // the first command of the *reused* conversation — a `Protocol` error
    // indistinguishable from a real one, which would become a `451` for a
    // message that would have delivered perfectly well on a fresh socket. So it
    // is retried once, on a connection this process has just opened.
    //
    // Three conditions, each load-bearing:
    //
    // - only on a **reused** connection, because a failure on a socket we opened
    //   a millisecond ago is the downstream talking, not a stale pool entry;
    // - only on a **protocol** error, not a timeout — a downstream slow enough to
    //   blow the stage budget is slow, and retrying spends the budget twice;
    // - **never at the final dot**, which is §10.2's window. Past the terminating
    //   dot the message may already be accepted, and a retry there is how one
    //   message becomes two.
    if checkout.reused() && is_stale_connection(&result) {
        tracing::info!(
            route = %route.name,
            error = ?result.as_ref().err(),
            "pooled connection was dead on reuse; retrying once on a fresh connection"
        );
        metrics::pool_retry(&route.name);
        checkout.reopen(route, tls).await?;
        result = checkout.conn().deliver(hostname, message, budget).await;
    }

    checkout.release(reusable(&result)).await;
    result
}

/// Whether a failure looks like a connection the downstream had already closed,
/// rather than anything it said. See [`relay`] for why the final dot is excluded.
fn is_stale_connection(result: &Result<Delivered, RelayError>) -> bool {
    matches!(result, Err(RelayError::Protocol(stage, _)) if *stage != Stage::FinalDot)
}

/// Whether the connection can go back in the pool afterwards.
///
/// §8.3: "discarded on any protocol error rather than returned to the pool". The
/// distinction that decides it is not success versus failure but *whose* failure:
/// a rejection is the downstream's considered answer over a connection that is
/// still perfectly well, and `RSET` puts it back to a clean transaction state. A
/// timeout, a protocol error or §10.2's ambiguity all leave a connection whose
/// state we cannot describe, and one of those must never be handed to the next
/// message.
fn reusable(result: &Result<Delivered, RelayError>) -> bool {
    matches!(
        result,
        Ok(_) | Err(RelayError::Rejected { .. }) | Err(RelayError::MissingCapability(_))
    )
}

pub(super) struct Connection {
    reader: BufReader<Stream>,
    /// Capabilities from the post-`STARTTLS` `EHLO`, which is the one that counts
    /// — RFC 3207 requires the server to discard prior state on upgrade.
    caps: WireReply,
}

impl Connection {
    pub(super) async fn open(
        route: &Route,
        tls: &TlsConfigs,
        budget: &Budget,
    ) -> Result<Connection, RelayError> {
        let ds = &route.downstream;
        let addr = format!("{}:{}", ds.host, ds.port);

        // §8.2 `opportunistic` says to "continue in plaintext ... if it fails".
        // A failed STARTTLS leaves the connection unusable — the peer is waiting
        // for TLS bytes — so honouring that means dialling again without
        // offering STARTTLS. Tried at most once.
        let mut allow_starttls = true;
        loop {
            let tcp = connect(&addr, budget.connect).await?;

            let mut conn = Connection {
                reader: BufReader::new(Stream::Plain(tcp)),
                caps: WireReply {
                    code: 0,
                    text: String::new(),
                    lines: Vec::new(),
                },
            };

            let greeting = conn.read_reply(Stage::Greeting, budget.command).await?;
            if !greeting.is_positive() {
                return Err(RelayError::Rejected {
                    stage: Stage::Greeting,
                    code: greeting.code,
                    text: greeting.text,
                });
            }

            conn.caps = conn.ehlo(&ds.host, budget).await?;

            let wants_tls = allow_starttls && ds.tls != TlsMode::Off;
            if wants_tls {
                match conn.try_starttls(route, tls, budget).await {
                    Ok(true) => {}
                    // Not advertised.
                    Ok(false) => match ds.tls {
                        TlsMode::Opportunistic | TlsMode::Off => {}
                        TlsMode::Required | TlsMode::RequiredVerify => {
                            return Err(RelayError::Tls(format!(
                                "{} does not advertise STARTTLS and tls mode is {:?}",
                                ds.host, ds.tls
                            )));
                        }
                    },
                    Err(e) => match ds.tls {
                        TlsMode::Opportunistic => {
                            tracing::warn!(
                                route = %route.name,
                                error = ?e,
                                "opportunistic STARTTLS failed; retrying in plaintext"
                            );
                            allow_starttls = false;
                            continue;
                        }
                        _ => return Err(e),
                    },
                }
            }

            if let Some(auth) = &ds.auth {
                conn.auth(&auth.username, &auth.password, budget).await?;
            }

            return Ok(conn);
        }
    }

    async fn ehlo(&mut self, host: &str, budget: &Budget) -> Result<WireReply, RelayError> {
        self.write(&format!("EHLO {host}\r\n"), Stage::Ehlo, budget.command)
            .await?;
        let reply = self.read_reply(Stage::Ehlo, budget.command).await?;
        if !reply.is_positive() {
            return Err(RelayError::Rejected {
                stage: Stage::Ehlo,
                code: reply.code,
                text: reply.text,
            });
        }
        Ok(reply)
    }

    /// Returns `Ok(true)` if the connection is now encrypted, `Ok(false)` if
    /// `STARTTLS` was not advertised.
    async fn try_starttls(
        &mut self,
        route: &Route,
        tls: &TlsConfigs,
        budget: &Budget,
    ) -> Result<bool, RelayError> {
        if !self.caps.advertises("STARTTLS") {
            return Ok(false);
        }

        self.write("STARTTLS\r\n", Stage::StartTls, budget.command)
            .await?;
        let reply = self.read_reply(Stage::StartTls, budget.command).await?;
        if !reply.is_positive() {
            return Err(RelayError::Tls(format!(
                "STARTTLS refused: {} {}",
                reply.code, reply.text
            )));
        }

        // Take the TCP stream back out to hand to the TLS layer. Anything
        // buffered past the STARTTLS reply would be a protocol violation (RFC
        // 3207 §6 — a buffering attack), so refuse rather than discard it.
        let buffered = self.reader.buffer().len();
        if buffered > 0 {
            return Err(RelayError::Protocol(
                Stage::StartTls,
                format!("{buffered} bytes sent before the TLS handshake"),
            ));
        }

        let stream = std::mem::replace(self.reader.get_mut(), Stream::Taken);
        let Stream::Plain(tcp) = stream else {
            return Err(RelayError::Protocol(
                Stage::StartTls,
                "STARTTLS issued on an already-encrypted connection".into(),
            ));
        };

        let upgraded = tls
            .upgrade(route.downstream.tls, &route.downstream.host, tcp)
            .await
            .map_err(RelayError::Tls)?;

        self.reader = BufReader::new(upgraded);

        // RFC 3207 §4.2: the client MUST discard knowledge from the previous
        // EHLO and re-issue it. A downstream can legitimately advertise AUTH
        // only after the channel is encrypted.
        self.caps = self.ehlo(&route.downstream.host, budget).await?;
        Ok(true)
    }

    async fn auth(
        &mut self,
        username: &str,
        password: &str,
        budget: &Budget,
    ) -> Result<(), RelayError> {
        if !self.caps.advertises("AUTH") {
            // Configuration says to authenticate and the downstream offers no
            // way to. That is a configuration fault, and D-008's reasoning says
            // it must not become a 550.
            return Err(RelayError::MissingCapability("AUTH"));
        }

        // AUTH PLAIN with an initial response: one round trip, and the only
        // mechanism worth attempting when we hold a cleartext password.
        let payload = B64.encode(format!("\0{username}\0{password}").as_bytes());
        self.write(
            &format!("AUTH PLAIN {payload}\r\n"),
            Stage::Auth,
            budget.command,
        )
        .await?;

        let reply = self.read_reply(Stage::Auth, budget.command).await?;
        if !reply.is_positive() {
            return Err(RelayError::Rejected {
                stage: Stage::Auth,
                code: reply.code,
                text: reply.text,
            });
        }
        Ok(())
    }

    pub(super) async fn deliver(
        &mut self,
        _hostname: &str,
        message: &Message<'_>,
        budget: &Budget,
    ) -> Result<Delivered, RelayError> {
        // D-018: capability shortfalls are caught here, before the envelope, so
        // the failure is attributable rather than showing up as a mystery 5xx.
        if message.smtputf8 && !self.caps.advertises("SMTPUTF8") {
            return Err(RelayError::MissingCapability("SMTPUTF8"));
        }
        if message.body_8bitmime && !self.caps.advertises("8BITMIME") {
            return Err(RelayError::MissingCapability("8BITMIME"));
        }
        if let Some(max) = self.caps.max_size() {
            // A downstream SIZE of 0 means "no stated limit" (RFC 1870 §6.2).
            if max > 0 && message.body.len() as u64 > max {
                return Err(RelayError::Rejected {
                    stage: Stage::MailFrom,
                    code: 552,
                    text: format!("message exceeds downstream SIZE limit of {max}"),
                });
            }
        }

        // -- MAIL FROM --
        let mut params = String::new();
        if message.body_8bitmime {
            params.push_str(" BODY=8BITMIME");
        }
        if message.smtputf8 {
            params.push_str(" SMTPUTF8");
        }
        if self.caps.advertises("SIZE") {
            params.push_str(&format!(" SIZE={}", message.body.len()));
        }

        let from = message.mail_from.unwrap_or("");
        self.command(
            &format!("MAIL FROM:<{from}>{params}\r\n"),
            Stage::MailFrom,
            budget.command,
        )
        .await?;

        // -- RCPT TO --
        for rcpt in message.recipients {
            self.command(
                &format!("RCPT TO:<{rcpt}>\r\n"),
                Stage::RcptTo,
                budget.command,
            )
            .await?;
        }

        // -- DATA --
        self.command("DATA\r\n", Stage::Data, budget.command)
            .await?;

        let mut wire = Vec::with_capacity(message.body.len() + 64);
        stuff_into(&mut wire, message.body);
        self.write_bytes(&wire, Stage::FinalDot, budget.data)
            .await?;

        // §10.2 lives here. Everything from the terminating dot until a reply is
        // read is the ambiguous window, and a read failure in it is NOT the same
        // as a read failure anywhere else — the message may already have been
        // accepted.
        let reply = match self.read_reply(Stage::FinalDot, budget.data).await {
            Ok(r) => r,
            Err(RelayError::Timeout(_)) | Err(RelayError::Protocol(_, _)) => {
                return Err(RelayError::Ambiguous)
            }
            Err(other) => return Err(other),
        };

        if !reply.is_positive() {
            return Err(RelayError::Rejected {
                stage: Stage::FinalDot,
                code: reply.code,
                text: reply.text,
            });
        }

        Ok(Delivered {
            code: reply.code,
            text: reply.text,
        })
    }

    pub(super) async fn quit(&mut self, budget: &Budget) -> Result<(), RelayError> {
        self.write("QUIT\r\n", Stage::Quit, budget.command).await?;
        let _ = self.read_reply(Stage::Quit, budget.command).await;
        Ok(())
    }

    /// §8.3 — "validated with `NOOP` before reuse if idle beyond a short
    /// threshold".
    ///
    /// This is the cheap half of the staleness problem: a downstream that closed
    /// the connection politely is discovered here, before a message is committed
    /// to it. The expensive half — a close we only discover mid-conversation —
    /// is what [`relay`]'s single retry exists for. Neither is sufficient alone:
    /// a connection can die between the `NOOP` and the `MAIL FROM`.
    pub(super) async fn noop(&mut self, budget: &Budget) -> Result<(), RelayError> {
        self.command("NOOP\r\n", Stage::Keepalive, budget.command)
            .await
            .map(|_| ())
    }

    /// §8.3 — "`RSET` between messages on a reused connection".
    ///
    /// Issued when the connection goes back to the pool rather than when it comes
    /// out, so that what sits idle is always a connection with no half-finished
    /// transaction on it. A rejection at `RCPT TO` leaves one; the next message
    /// must not inherit it.
    pub(super) async fn rset(&mut self, budget: &Budget) -> Result<(), RelayError> {
        self.command("RSET\r\n", Stage::Keepalive, budget.command)
            .await
            .map(|_| ())
    }

    /// Write a command and require a positive reply.
    async fn command(
        &mut self,
        line: &str,
        stage: Stage,
        timeout: Duration,
    ) -> Result<WireReply, RelayError> {
        self.write(line, stage, timeout).await?;
        let reply = self.read_reply(stage, timeout).await?;
        if !reply.is_positive() {
            return Err(RelayError::Rejected {
                stage,
                code: reply.code,
                text: reply.text,
            });
        }
        Ok(reply)
    }

    async fn write(&mut self, s: &str, stage: Stage, timeout: Duration) -> Result<(), RelayError> {
        self.write_bytes(s.as_bytes(), stage, timeout).await
    }

    async fn write_bytes(
        &mut self,
        bytes: &[u8],
        stage: Stage,
        timeout: Duration,
    ) -> Result<(), RelayError> {
        let write = async {
            self.reader.get_mut().write_all(bytes).await?;
            self.reader.get_mut().flush().await
        };
        match tokio::time::timeout(timeout, write).await {
            Err(_) => Err(RelayError::Timeout(stage)),
            Ok(Err(e)) => Err(RelayError::Protocol(stage, e.to_string())),
            Ok(Ok(())) => Ok(()),
        }
    }

    /// Read a complete reply, following `250-` continuations.
    async fn read_reply(
        &mut self,
        stage: Stage,
        timeout: Duration,
    ) -> Result<WireReply, RelayError> {
        match tokio::time::timeout(timeout, self.read_reply_inner(stage)).await {
            Err(_) => Err(RelayError::Timeout(stage)),
            Ok(r) => r,
        }
    }

    async fn read_reply_inner(&mut self, stage: Stage) -> Result<WireReply, RelayError> {
        let mut code = None;
        let mut lines: Vec<String> = Vec::new();

        for _ in 0..MAX_REPLY_LINES {
            let mut line = String::new();
            let n = (&mut self.reader)
                .take(MAX_REPLY_LINE)
                .read_line(&mut line)
                .await
                .map_err(|e| RelayError::Protocol(stage, e.to_string()))?;

            if n == 0 {
                return Err(RelayError::Protocol(
                    stage,
                    "connection closed before a reply was received".into(),
                ));
            }

            let line = line.trim_end_matches(['\r', '\n']);
            if line.len() < 3 {
                return Err(RelayError::Protocol(
                    stage,
                    format!("reply line too short: {line:?}"),
                ));
            }

            let this_code: u16 = line[..3]
                .parse()
                .map_err(|_| RelayError::Protocol(stage, format!("unparseable reply: {line:?}")))?;

            match code {
                None => code = Some(this_code),
                // RFC 5321 §4.2.1: every line of a multiline reply carries the
                // same code. A downstream changing it mid-reply is desynchronised
                // and must not be trusted for the rest of the conversation.
                Some(first) if first != this_code => {
                    return Err(RelayError::Protocol(
                        stage,
                        format!("reply code changed from {first} to {this_code} mid-reply"),
                    ))
                }
                Some(_) => {}
            }

            let text = line[3..].trim_start_matches([' ', '-']).to_string();
            lines.push(text);

            match line.as_bytes().get(3) {
                Some(b'-') => continue,
                // A bare code with no separator terminates the reply.
                Some(b' ') | None => {
                    let code = code.unwrap_or(0);
                    return Ok(WireReply {
                        code,
                        text: lines.join(" "),
                        lines,
                    });
                }
                Some(other) => {
                    return Err(RelayError::Protocol(
                        stage,
                        format!("invalid reply separator {:?}", *other as char),
                    ))
                }
            }
        }

        Err(RelayError::Protocol(
            stage,
            format!("reply exceeded {MAX_REPLY_LINES} lines"),
        ))
    }
}

async fn connect(addr: &str, timeout: Duration) -> Result<TcpStream, RelayError> {
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Err(_) => Err(RelayError::Timeout(Stage::Connect)),
        Ok(Err(e)) => Err(RelayError::Connect(format!("{addr}: {e}"))),
        Ok(Ok(s)) => {
            // Replies are small and latency-sensitive; Nagle would add up to
            // 40ms per round trip to a conversation that has at least five.
            let _ = s.set_nodelay(true);
            Ok(s)
        }
    }
}
