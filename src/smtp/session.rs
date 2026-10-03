//! §5.2 — the SMTP state machine.
//!
//! Hand-rolled rather than delegated to a server crate, per §5.2: "quota-aware
//! responses need to be emitted at specific points in the conversation, and the
//! reply mapping in §10 is not expressible through a generic callback interface".
//!
//! ## Pipelining
//!
//! §5.2 advertises `PIPELINING`, which means a client may send
//! `MAIL`/`RCPT`/`DATA` in one write and only then wait. Two consequences are
//! load-bearing here:
//!
//! * Replies must be emitted **in order, one per command**, and never coalesced.
//!   A client counts them.
//! * Buffered-but-unread input must never be discarded on an error. RFC 2920 §3.1
//!   requires the server to keep reading and keep answering; dropping the buffer
//!   desynchronises the connection and the client attributes the next reply to
//!   the wrong command.
//!
//! Both fall out of reading through one [`BufReader`] for the whole session and
//! never reaching past it — which is also why the `DATA` payload is read from the
//! same reader rather than from the raw socket.
//!
//! ## STARTTLS (D-070)
//!
//! RFC 3207 inverts the pipelining rule at exactly one point. Bytes already
//! buffered behind a `STARTTLS` were sent in cleartext by whoever is on the
//! wire, and accepting them would let an attacker inject commands that appear to
//! have arrived over the encrypted channel (CVE-2011-0411 and its descendants).
//! So there, and only there, buffered input is **not** kept: the connection is
//! dropped. It is the same check `downstream::client` makes on the way out — the
//! same defect, seen from the other end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::field::Empty;
use tracing::{Instrument, Span};

use super::acl::Acl;
use super::auth::{self, AuthState, AuthStep, Verifier, VerifyLimit};
use super::buffer::{self, MessageBuffer};
use super::command::{self, Command, MailParams, ParseError};
use super::reply::{self, Reply};
use super::{Policy, Shutdown};
use crate::capture;
use crate::config::{CaptureOnError, Config, IngressAuth, IngressTls};
use crate::downstream::stream::Stream;
use crate::metrics;
use crate::relay::{self, Engine};
use crate::routing::ramp_select;
use crate::routing::sender_match::Senders;

/// RFC 5321 §4.5.3.1: 512 octets for a command line including CRLF.
///
/// Generously rounded up. Without a cap a client can exhaust memory before
/// `SIZE` has anything to say — `max_message_bytes` only governs `DATA`.
/// See `DECISIONS.md` D-020.
const MAX_COMMAND_LINE: u64 = 4096;

/// RFC 5321 §4.5.3.1: 1000 octets for a text line including CRLF. Raised well
/// above the minimum because real mail exceeds it constantly, but still bounded.
const MAX_DATA_LINE: usize = 65_536;

/// How much of a message's head to scan for `From:` (§5.4).
const MAX_HEADER_SCAN: usize = 256 * 1024;

/// How long [`Session::close`] waits for a `close_notify` to flush.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// D-081 — once the session's deadline has passed, how long Simmer waits for the
/// client's next command so that its `421` answers that command. Bounds the
/// overrun an idle client adds.
const DEADLINE_GRACE: Duration = Duration::from_secs(2);

/// Per-connection state.
pub struct Session {
    io: BufReader<Stream>,
    peer: SocketAddr,
    engine: Engine,
    verifier: Arc<Verifier>,
    acl: Arc<Acl>,
    /// D-079 — permits for argon2 verification, shared with every other session.
    verifies: VerifyLimit,
    /// The listener's TLS and AUTH policy (D-070).
    policy: Arc<Policy>,

    /// `None` until `EHLO`/`HELO`.
    greeted: Option<String>,
    /// Whether the client used `EHLO` (so extensions are in play) or `HELO`.
    esmtp: bool,
    /// The authenticated username, once `AUTH` succeeds. Consulted by the D-071
    /// ACL and never by routing (§5.3).
    user: Option<String>,
    /// §5.3's three strikes. Per *connection*, so it deliberately survives the
    /// `STARTTLS` reset: clearing it would sell unlimited password guesses for
    /// one extra round trip.
    auth_failures: u32,
    /// Set while an `AUTH` exchange is mid-flight.
    auth_state: Option<AuthState>,

    transaction: Option<Transaction>,
    /// §9.5 — one per message, regenerated at each `MAIL FROM`.
    correlation_id: String,
    /// D-081 — when `timeouts.session` runs out. Every wait on the client is
    /// capped by it; a relay in flight never is.
    deadline: tokio::time::Instant,
    /// §9.6 (D-126) — the connection's `smtp.session` span, which the caller
    /// has entered. Held so that `username` can be recorded on it from inside a
    /// transaction, where it is not the current span.
    session_span: Span,
    /// §10.4's hard stop (D-106). Observed exactly where the deadline is — at
    /// waits on the client — and never mid-relay.
    hard_stop: Shutdown,
}

/// State between `MAIL FROM` and the final dot.
struct Transaction {
    mail_from: Option<String>,
    params: MailParams,
    recipients: Vec<String>,
    /// §9.6 (D-126) — `smtp.transaction`, from the accepted `MAIL FROM` to its
    /// final reply. Every command of the transaction runs inside it, and
    /// dropping it with the transaction is what ends it.
    span: Span,
}

/// Why a session ended. The caller logs it; nothing else depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    Quit,
    ClientClosed,
    CommandTimeout,
    SessionTimeout,
    ProtocolAbuse,
    AuthAbuse,
    IoError,
    ShuttingDown,
    /// The `STARTTLS` handshake failed, or plaintext was pipelined behind it.
    TlsFailed,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        io: Stream,
        peer: SocketAddr,
        engine: Engine,
        verifier: Arc<Verifier>,
        acl: Arc<Acl>,
        verifies: VerifyLimit,
        policy: Arc<Policy>,
        hard_stop: Shutdown,
    ) -> Self {
        // D-081 — from here, the instant the caller's timer used to start.
        let deadline = tokio::time::Instant::now() + engine.config.server.timeouts.session;
        Self {
            io: BufReader::new(io),
            peer,
            engine,
            verifier,
            acl,
            verifies,
            policy,
            greeted: None,
            esmtp: false,
            user: None,
            auth_failures: 0,
            auth_state: None,
            transaction: None,
            correlation_id: new_correlation_id(),
            deadline,
            session_span: Span::current(),
            hard_stop,
        }
    }

    fn config(&self) -> &Config {
        &self.engine.config
    }

    /// D-081 — how long a wait may take: a stage's own budget, or what is left of
    /// `timeouts.session` if that is shorter. The flag says the deadline is what
    /// bounds it, so its expiry is reported as the session's and not the stage's.
    fn budget(&self, stage: Duration) -> (Duration, bool) {
        let left = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if left < stage {
            (left, true)
        } else {
            (stage, false)
        }
    }

    fn encrypted(&self) -> bool {
        self.io.get_ref().is_encrypted()
    }

    /// Whether `AUTH` can succeed on this session *right now* (D-070): the
    /// listener allows it, there is somebody to authenticate as, and either the
    /// channel is encrypted or plaintext credentials are explicitly allowed.
    fn auth_usable(&self) -> bool {
        self.policy.auth != IngressAuth::Disabled
            && !self.verifier.is_empty()
            && (self.encrypted() || self.config().server.auth.allow_insecure_auth)
    }

    /// Whether the RFC 3207 §4 gate is closed: a `starttls_required` listener
    /// before its handshake.
    fn awaiting_required_tls(&self) -> bool {
        self.policy.tls == IngressTls::StarttlsRequired && !self.encrypted()
    }

    /// Drive the session to completion.
    ///
    /// `timeouts.session` is enforced here rather than by the caller (D-081):
    /// every wait on the client is capped by what the session has left, and a
    /// relay in flight is never cut. A session can therefore overrun its deadline
    /// by one relay's downstream budget, and by no more.
    pub async fn run(&mut self) -> SessionEnd {
        let hostname = self.config().server.hostname.clone();
        if self.send(&reply::greeting(&hostname)).await.is_err() {
            return SessionEnd::IoError;
        }

        loop {
            let line = match self.read_command_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return SessionEnd::ClientClosed,
                Err(ReadError::Timeout) => {
                    let _ = self.send(&reply::command_timeout()).await;
                    return SessionEnd::CommandTimeout;
                }
                Err(ReadError::SessionTimeout) => {
                    let _ = self.send(&reply::session_timeout()).await;
                    return SessionEnd::SessionTimeout;
                }
                Err(ReadError::ShuttingDown) => {
                    let _ = self.send(&reply::shutting_down()).await;
                    return SessionEnd::ShuttingDown;
                }
                Err(ReadError::TooLong) => {
                    // Do not close: RFC 2920 wants us to stay in step. But the
                    // rest of the over-long line is still in the stream, so it
                    // will be read as a (bogus) command and answered 500. That is
                    // the same thing a real MTA does.
                    if self.send(&reply::line_too_long()).await.is_err() {
                        return SessionEnd::IoError;
                    }
                    continue;
                }
                Err(ReadError::Io) => return SessionEnd::IoError,
            };

            // An AUTH exchange in flight consumes raw base64 lines, not commands.
            if self.auth_state.is_some() {
                match self.continue_auth(&line).await {
                    Ok(Some(end)) => return end,
                    Ok(None) => continue,
                    Err(()) => return SessionEnd::IoError,
                }
            }

            // Inside the transaction's span when there is one, so that what
            // RCPT TO and DATA log — and the relay below them — is attributed
            // to the message rather than only to the connection.
            let span = self
                .transaction
                .as_ref()
                .map_or_else(Span::none, |t| t.span.clone());
            match self.handle_line(&line).instrument(span).await {
                Ok(Some(end)) => return end,
                Ok(None) => continue,
                Err(()) => return SessionEnd::IoError,
            }
        }
    }

    /// `Ok(Some(end))` ends the session, `Ok(None)` continues, `Err(())` is I/O.
    async fn handle_line(&mut self, line: &str) -> Result<Option<SessionEnd>, ()> {
        let cmd = match command::parse(line) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(peer = %self.peer, error = %e, "rejected command");
                let r = match e {
                    ParseError::UnknownParameter(p) => {
                        // RFC 1869 §6: 555 for a parameter we do not implement.
                        Reply::new(555, format!("5.5.4 unrecognised parameter: {p}"))
                    }
                    _ => reply::syntax_error(),
                };
                self.send(&r).await.map_err(|_| ())?;
                return Ok(None);
            }
        };

        // RFC 3207 §4: on a listener that requires TLS, "the server SHOULD reply
        // to every command other than NOOP, EHLO, STARTTLS, or QUIT with the
        // reply code 530". RSET joins them because it changes nothing a client
        // could exploit — there can be no transaction to reset — and clients send
        // it in unexpected places.
        if self.awaiting_required_tls()
            && !matches!(
                cmd,
                Command::Ehlo(_)
                    | Command::Noop
                    | Command::Rset
                    | Command::Quit
                    | Command::StartTls
            )
        {
            return self.reply(reply::must_starttls_first()).await;
        }

        match cmd {
            Command::Quit => {
                let hostname = self.config().server.hostname.clone();
                self.send(&reply::bye(&hostname)).await.map_err(|_| ())?;
                Ok(Some(SessionEnd::Quit))
            }

            Command::Ehlo(domain) => {
                self.reset_transaction();
                self.greeted = Some(domain.clone());
                self.esmtp = true;

                let cfg = self.config();
                let caps = reply::Capabilities {
                    max_size: cfg.server.max_message_bytes,
                    smtputf8: cfg.advertise_smtputf8(),
                    // D-070: offered until the handshake, then never again.
                    starttls: self.policy.offers_starttls() && !self.encrypted(),
                    // Not advertised where it could not succeed — before the
                    // handshake on a port that requires it, or over plaintext
                    // when plaintext credentials are refused. Advertising AUTH
                    // there invites a client to send its password in the clear
                    // only to be told no.
                    auth: self.auth_usable() && !self.awaiting_required_tls(),
                };
                let r = reply::ehlo(&cfg.server.hostname, &domain, caps);
                self.send(&r).await.map_err(|_| ())?;
                Ok(None)
            }

            Command::Helo(domain) => {
                self.reset_transaction();
                self.greeted = Some(domain);
                // §5.2: "HELO is accepted." No extensions are in play afterwards,
                // so SIZE/8BITMIME/SMTPUTF8 parameters become syntax errors.
                self.esmtp = false;
                let hostname = self.config().server.hostname.clone();
                self.send(&reply::helo(&hostname)).await.map_err(|_| ())?;
                Ok(None)
            }

            Command::Noop => self.reply(reply::ok()).await,

            Command::Rset => {
                self.reset_transaction();
                self.reply(reply::ok()).await
            }

            // §5.2: VRFY is always 252, EXPN always 502.
            Command::Vrfy => self.reply(reply::vrfy()).await,
            Command::Expn | Command::Bdat => self.reply(reply::not_implemented()).await,
            Command::Unknown(_) => self.reply(reply::unrecognised()).await,

            Command::Auth { mechanism, initial } => {
                self.begin_auth(&mechanism, initial.as_deref()).await
            }

            Command::Mail { from, params } => self.mail_from(from, params).await,
            Command::Rcpt { to } => self.rcpt_to(to).await,
            Command::Data => self.data().await,
            Command::StartTls => self.starttls().await,
        }
    }

    // -- STARTTLS (RFC 3207, D-070) --------------------------------------

    async fn starttls(&mut self) -> Result<Option<SessionEnd>, ()> {
        if self.encrypted() {
            return self.reply(reply::tls_already_active()).await;
        }
        if !self.policy.offers_starttls() {
            // Recognised and refused, like BDAT: 502 rather than 500 tells the
            // client to carry on without it rather than that it misspoke.
            return self.reply(reply::not_implemented()).await;
        }
        if self.transaction.is_some() || self.auth_state.is_some() {
            return self.reply(reply::bad_sequence()).await;
        }

        // The injection check. Anything already buffered arrived in cleartext
        // *after* the STARTTLS line and before we have said yes, so it cannot
        // have come from the TLS peer. RFC 2920 would have us keep it and answer
        // it; RFC 3207 §6 is the exception, and it wins. Dropped, not drained:
        // draining would mean deciding which of an attacker's bytes to believe.
        let buffered = self.io.buffer().len();
        if buffered > 0 {
            tracing::warn!(
                peer = %self.peer,
                bytes = buffered,
                "plaintext pipelined behind STARTTLS; dropping the connection"
            );
            metrics::inbound_tls_failure("starttls", "plaintext_after_starttls");
            return Ok(Some(SessionEnd::TlsFailed));
        }

        self.send(&reply::starttls_ready()).await.map_err(|_| ())?;

        let acceptor = self
            .policy
            .acceptor
            .clone()
            .expect("offers_starttls implies a certificate (Listener::bind)");
        let Stream::Plain(tcp) = std::mem::replace(self.io.get_mut(), Stream::Taken) else {
            // `encrypted()` was false and nothing else takes the stream, so
            // this is Plain. `Taken` reports itself on next use rather than
            // panicking a live session, which is why the variant exists.
            return Ok(Some(SessionEnd::TlsFailed));
        };

        // A handshake that the deadline cuts short ends like one that timed out:
        // there is no channel left to report on either way.
        let (timeout, _) = self.budget(self.config().server.timeouts.command);
        let stop = self.hard_stop.clone();
        let handshake = tokio::select! {
            biased;
            // D-106 — a wait on the client like any other. There is no channel
            // to send a 421 on mid-handshake, so the close is the answer.
            _ = stop.cancelled() => return Ok(Some(SessionEnd::ShuttingDown)),
            h = tokio::time::timeout(timeout, acceptor.accept(tcp)) => h,
        };
        match handshake {
            Ok(Ok(tls)) => *self.io.get_mut() = Stream::Tls(Box::new(tls.into())),
            // There is no channel left to report on: the TLS layer owns the
            // socket and the handshake did not produce one. The close is the
            // answer, and it is what RFC 3207 §4.1 expects.
            Ok(Err(e)) => {
                tracing::info!(peer = %self.peer, error = %e, "STARTTLS handshake failed");
                metrics::inbound_tls_failure("starttls", "handshake");
                return Ok(Some(SessionEnd::TlsFailed));
            }
            Err(_) => {
                tracing::info!(peer = %self.peer, "STARTTLS handshake timed out");
                metrics::inbound_tls_failure("starttls", "handshake");
                return Ok(Some(SessionEnd::TlsFailed));
            }
        }

        // RFC 3207 §4.2: "the server MUST discard any knowledge obtained from the
        // client, such as the argument to the EHLO command, which was not obtained
        // from the TLS negotiation itself." The client re-issues EHLO.
        // `auth_failures` is kept — see its field comment.
        self.greeted = None;
        self.esmtp = false;
        self.user = None;
        self.auth_state = None;
        self.transaction = None;
        Ok(None)
    }

    async fn reply(&mut self, r: Reply) -> Result<Option<SessionEnd>, ()> {
        let closes = r.closes_connection();
        self.send(&r).await.map_err(|_| ())?;
        Ok(closes.then_some(SessionEnd::ProtocolAbuse))
    }

    // -- AUTH (§5.3) -----------------------------------------------------

    async fn begin_auth(
        &mut self,
        mechanism: &str,
        initial: Option<&str>,
    ) -> Result<Option<SessionEnd>, ()> {
        if self.greeted.is_none() {
            return self.reply(reply::bad_sequence()).await;
        }
        if self.policy.auth == IngressAuth::Disabled {
            return self.reply(reply::auth_not_available()).await;
        }
        if self.user.is_some() {
            return self.reply(reply::auth_already_done()).await;
        }
        if self.transaction.is_some() {
            // RFC 4954 §4: AUTH is not permitted during a mail transaction.
            return self.reply(reply::bad_sequence()).await;
        }
        if self.config().server.auth.users.is_empty() {
            return self.reply(reply::auth_mechanism_unsupported()).await;
        }
        // D-070: plaintext credentials are refused unless allowed outright. Before
        // the exchange starts, so the password is never sent at all — PLAIN with
        // an initial response has already crossed the wire by now, which is the
        // client's choice and the reason AUTH is not advertised here.
        if !self.encrypted() && !self.config().server.auth.allow_insecure_auth {
            return self.reply(reply::encryption_required_for_auth()).await;
        }

        let mechanisms = self.config().server.auth.mechanisms.clone();
        let Some(step) = auth::begin(mechanism, initial, &mechanisms) else {
            return self.reply(reply::auth_mechanism_unsupported()).await;
        };
        self.apply_auth_step(step).await
    }

    async fn continue_auth(&mut self, line: &str) -> Result<Option<SessionEnd>, ()> {
        let state = self.auth_state.clone().expect("checked by caller");
        let step = auth::advance(&state, line);
        self.apply_auth_step(step).await
    }

    async fn apply_auth_step(&mut self, step: AuthStep) -> Result<Option<SessionEnd>, ()> {
        match step {
            AuthStep::Challenge { b64, next } => {
                self.auth_state = Some(next);
                self.send(&reply::auth_challenge(&b64))
                    .await
                    .map_err(|_| ())?;
                Ok(None)
            }

            AuthStep::Cancelled => {
                self.auth_state = None;
                self.reply(reply::auth_cancelled()).await
            }

            AuthStep::BadEncoding => {
                self.auth_state = None;
                tracing::info!(peer = %self.peer, "AUTH payload was not valid base64; refused");
                // A malformed payload is not a failed password: it does not
                // count toward the three-strike budget, because a client with a
                // broken encoder would otherwise be disconnected rather than
                // told what is wrong.
                self.reply(reply::auth_bad_encoding()).await
            }

            AuthStep::Credentials { username, password } => {
                self.auth_state = None;

                // argon2id costs tens of milliseconds of CPU by design. Running
                // it on the async runtime would let a burst of AUTH attempts
                // stall every other session sharing the worker.
                //
                // D-079 — and not more than the verification bound at once.
                let verifier = Arc::clone(&self.verifier);
                let u = username.clone();
                // D-081 — waiting for a permit is not waiting on the client, but
                // it is a wait inside the session, so the deadline bounds it too.
                let (left, _) = self.budget(Duration::MAX);
                let stop = self.hard_stop.clone();
                let waited = tokio::select! {
                    biased;
                    _ = stop.cancelled() => {
                        let _ = self.send(&reply::shutting_down()).await;
                        return Ok(Some(SessionEnd::ShuttingDown));
                    }
                    w = tokio::time::timeout(left, self.verifies.acquire()) => w,
                };
                let Ok(permit) = waited else {
                    let _ = self.send(&reply::session_timeout()).await;
                    return Ok(Some(SessionEnd::SessionTimeout));
                };
                let ok =
                    tokio::task::spawn_blocking(move || verifier.verify_blocking(&u, &password))
                        .await
                        .unwrap_or(false);
                drop(permit);

                if ok {
                    tracing::info!(
                        peer = %self.peer,
                        username = %username,
                        tls = self.encrypted(),
                        "authenticated"
                    );
                    self.session_span.record("username", username.as_str());
                    self.user = Some(username);
                    self.reply(reply::auth_succeeded()).await
                } else {
                    self.auth_failures += 1;
                    tracing::warn!(
                        peer = %self.peer,
                        username = %username,
                        failures = self.auth_failures,
                        "authentication failed"
                    );
                    if self.auth_failures >= auth::MAX_FAILURES {
                        self.send(&reply::auth_too_many_failures())
                            .await
                            .map_err(|_| ())?;
                        return Ok(Some(SessionEnd::AuthAbuse));
                    }
                    self.reply(reply::auth_failed()).await
                }
            }
        }
    }

    // -- the mail transaction --------------------------------------------

    async fn mail_from(
        &mut self,
        from: Option<String>,
        params: MailParams,
    ) -> Result<Option<SessionEnd>, ()> {
        if self.greeted.is_none() {
            return self.reply(reply::bad_sequence()).await;
        }
        if self.policy.auth == IngressAuth::Required && self.user.is_none() {
            tracing::info!(
                peer = %self.peer,
                "MAIL FROM before AUTH on an auth: required listener; refused"
            );
            return self.reply(reply::auth_required()).await;
        }
        if self.transaction.is_some() {
            return self.reply(reply::bad_sequence()).await;
        }

        // Extension parameters are only legal after EHLO.
        if !self.esmtp && (params.size.is_some() || params.body_8bitmime || params.smtputf8) {
            return self.reply(reply::syntax_error()).await;
        }

        // §5.5 — reject an oversized message before the body is transferred,
        // which is the entire reason SIZE is advertised.
        if let Some(size) = params.size {
            if size > self.config().server.max_message_bytes {
                tracing::info!(
                    peer = %self.peer,
                    size,
                    max = self.config().server.max_message_bytes,
                    "MAIL FROM declared a SIZE above max_message_bytes; refused"
                );
                return self.reply(reply::message_too_large()).await;
            }
        }

        // D-018 / O-10 — answer here rather than discovering it mid-relay.
        let utf8_needed = params.smtputf8 || from.as_deref().is_some_and(command::needs_smtputf8);
        if utf8_needed && !self.config().advertise_smtputf8() {
            return self.reply(reply::smtputf8_unsupported()).await;
        }

        // D-071 — the envelope half of the ACL, here where it is cheap and
        // before a body is transferred. The null sender has no identity to
        // grant: a bounce is permitted to anyone who authenticated, and its
        // `From:` is still checked at the final dot.
        if let (Some(user), Some(sender)) = (self.user.as_deref(), from.as_deref()) {
            if !self.acl.permits(user, sender) {
                tracing::warn!(
                    peer = %self.peer,
                    username = %user,
                    sender = %sender,
                    stage = "mail_from",
                    "sender not permitted by the user's grants"
                );
                metrics::sender_not_permitted("mail_from");
                return self.reply(reply::sender_not_permitted()).await;
            }
        }

        self.correlation_id = new_correlation_id();
        // A child of the session span, which is current here: MAIL FROM is
        // handled outside any transaction, since there is none yet.
        let span = tracing::info_span!(
            "smtp.transaction",
            otel.name = "smtp.transaction",
            otel.status_code = Empty,
            correlation_id = %self.correlation_id,
            smtp.reply.code = Empty,
            bytes = Empty,
        );
        self.transaction = Some(Transaction {
            mail_from: from,
            params,
            recipients: Vec::new(),
            span,
        });
        self.reply(reply::ok()).await
    }

    async fn rcpt_to(&mut self, to: String) -> Result<Option<SessionEnd>, ()> {
        let Some(tx) = self.transaction.as_ref() else {
            return self.reply(reply::bad_sequence()).await;
        };

        // D-018 again: a UTF-8 recipient is as much an RFC 6531 address as a
        // UTF-8 sender.
        if command::needs_smtputf8(&to) && !self.config().advertise_smtputf8() {
            return self.reply(reply::smtputf8_unsupported()).await;
        }

        let cfg = &self.engine.config;
        // D-047 — one recipient per transaction, unconditionally. Collapsing
        // several per-recipient outcomes into one reply is lossy: SMTP allows one
        // reply, so a mixed result has to be reported as a single code and the
        // client cannot be told which recipients it applies to. Refusing is the
        // honest answer, and it is `452` rather than `5xx` because §14.1 will not
        // have a deliverable recipient suppressed by a limit of ours.
        //
        // §5.5's `max_recipients` is subsumed: no value of it is reachable past
        // the first recipient. §4.2 warns when it is set above 1.
        if !tx.recipients.is_empty() {
            tracing::info!(
                correlation_id = %self.correlation_id,
                "second RCPT TO in one transaction; refused 452 (D-047)"
            );
            return self.reply(reply::multiple_recipients_not_permitted()).await;
        }

        // §5.4: "When all rules use `envelope`, Simmer should decide early and
        // reject at RCPT TO to avoid a wasted body transfer."
        //
        // O-1's split lives here: this evaluates *eligibility* and takes no
        // reservation. The authoritative check is the reservation itself, taken
        // immediately before the downstream conversation, so a reservation never
        // spans the DATA transfer.
        //
        // D-099 narrows it once more: only when the ramp is already fixed here,
        // before `X-Simmer-Ramp` has arrived. Otherwise a header-selected ramp
        // could be refused because a different ramp is exhausted.
        let ingress = ramp_select::ingress(
            cfg,
            self.policy.ramp.as_deref(),
            self.policy.header_overrides_affinity,
            self.user.as_deref(),
        );
        let early = ramp_select::fixed_at_rcpt(cfg, &ingress)
            .filter(|ramp| crate::routing::sender_match::can_decide_at_rcpt(ramp))
            .map(|ramp| ramp.name.clone());
        if let Some(ramp_name) = early {
            let senders = Senders::new(tx.mail_from.as_deref(), None);
            let engine = self.engine.clone();
            let ramp = engine
                .config
                .ramps
                .get(&ramp_name)
                .expect("fixed_at_rcpt returns a configured ramp");
            if let Err(e) = relay::check_early(&engine, ramp, &senders, &to).await {
                let r = e.to_reply(ramp);
                tracing::info!(
                    correlation_id = %self.correlation_id,
                    reason = ?e,
                    "rejected at RCPT TO"
                );
                return self.reply(r).await;
            }
        }

        self.transaction
            .as_mut()
            .expect("checked above")
            .recipients
            .push(to);
        self.reply(reply::ok()).await
    }

    async fn data(&mut self) -> Result<Option<SessionEnd>, ()> {
        let Some(tx) = self.transaction.as_ref() else {
            return self.reply(reply::bad_sequence()).await;
        };
        if tx.recipients.is_empty() {
            // RFC 5321 §3.3: DATA with no valid recipients is 503.
            return self.reply(reply::bad_sequence()).await;
        }

        self.send(&reply::start_mail_input())
            .await
            .map_err(|_| ())?;

        let max = self.config().server.max_message_bytes;
        let data_timeout = self.config().server.timeouts.data;

        let mut body = MessageBuffer::new();
        match self.read_data(&mut body, max, data_timeout).await {
            Ok(()) => {}
            Err(DataError::TooLarge) => {
                // §5.5. The connection is closed rather than resynchronised:
                // staying in step would mean reading and discarding an unbounded
                // remainder, which is the denial of service the limit exists to
                // prevent. See DECISIONS.md D-020.
                tracing::info!(
                    correlation_id = %self.correlation_id,
                    max,
                    "DATA exceeded max_message_bytes; refused and closing (D-020)"
                );
                let r = reply::message_too_large();
                self.finish_transaction(&r);
                self.send(&r).await.map_err(|_| ())?;
                return Ok(Some(SessionEnd::ProtocolAbuse));
            }
            Err(DataError::Timeout) => {
                tracing::info!(
                    correlation_id = %self.correlation_id,
                    "DATA timed out mid-transfer; nothing relayed"
                );
                let r = reply::data_timeout();
                self.finish_transaction(&r);
                let _ = self.send(&r).await;
                return Ok(Some(SessionEnd::CommandTimeout));
            }
            Err(DataError::SessionTimeout) => {
                let r = reply::session_timeout();
                self.finish_transaction(&r);
                let _ = self.send(&r).await;
                return Ok(Some(SessionEnd::SessionTimeout));
            }
            Err(DataError::ShuttingDown) => {
                // D-106 — the message was never complete, so nothing was
                // relayed and nothing was reserved.
                self.reset_transaction();
                let _ = self.send(&reply::shutting_down()).await;
                return Ok(Some(SessionEnd::ShuttingDown));
            }
            Err(DataError::AmbiguousTerminator) => {
                // Nothing was relayed. The session is still in step — the real
                // terminator has been read — so it carries on, exactly as it
                // would after any other refused message.
                tracing::warn!(
                    peer = %self.peer,
                    correlation_id = %self.correlation_id,
                    "DATA held an end-of-data marker with a bare line ending; message refused"
                );
                let r = reply::ambiguous_terminator();
                self.finish_transaction(&r);
                self.send(&r).await.map_err(|_| ())?;
                return Ok(None);
            }
            Err(DataError::Closed) => return Ok(Some(SessionEnd::ClientClosed)),
            Err(DataError::Io) => return Err(()),
        }

        if let Some(tx) = &self.transaction {
            tx.span.record("bytes", body.len());
        }
        let outcome_reply = self.route_and_relay(&mut body).await;
        self.finish_transaction(&outcome_reply);
        self.send(&outcome_reply).await.map_err(|_| ())?;
        Ok(None)
    }

    /// §6.1 steps 1–3 and §10.1: buffer, resolve identity, select a route and
    /// reserve quota, relay, map the reply.
    async fn route_and_relay(&mut self, body: &mut MessageBuffer) -> Reply {
        let tx = self.transaction.as_ref().expect("checked by caller");

        // §5.4 — the From: header, from the head of the buffer only. A spilled
        // 25 MiB message must not be read back whole to answer this.
        //
        // The block is kept rather than consumed: D-085's capture wants the
        // `Subject:` out of the same bytes, and reading the head twice for two
        // headers would be a second seek and a second parse per message.
        let head = match body.header_block(MAX_HEADER_SCAN).await {
            Ok(head) => head,
            Err(e) => {
                tracing::error!(correlation_id = %self.correlation_id, error = %e, "reading buffered message");
                return Reply::new(451, "4.3.0 internal buffering error");
            }
        };
        let from_header = first_from_address(&head);

        // D-071 — the header half of the ACL. `From:` first exists here, which is
        // the same split §5.4 lives with. A message with no parseable `From:` has
        // no identity to find inside the grant, and default deny means it is
        // refused rather than waved through on the envelope alone.
        if let Some(user) = self.user.as_deref() {
            let permitted = from_header
                .as_deref()
                .is_some_and(|from| self.acl.permits(user, from));
            if !permitted {
                tracing::warn!(
                    correlation_id = %self.correlation_id,
                    peer = %self.peer,
                    username = %user,
                    from_header = from_header.as_deref().unwrap_or("<absent or unparseable>"),
                    stage = "from_header",
                    "sender not permitted by the user's grants"
                );
                metrics::sender_not_permitted("from_header");
                return reply::sender_not_permitted();
            }
        }

        let senders = Senders::new(tx.mail_from.as_deref(), from_header.as_deref());

        // §5.8 (D-099) — the ramp's header, from the same head of the buffer.
        // Stripped from the outbound message whatever becomes of it (§6.5).
        let ramp_header = crate::rewrite::headers::split(&head)
            .headers
            .get_all(ramp_select::HEADER);

        let bytes = match body.read_all().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(correlation_id = %self.correlation_id, error = %e, "reading buffered message");
                return Reply::new(451, "4.3.0 internal buffering error");
            }
        };

        tracing::debug!(
            correlation_id = %self.correlation_id,
            bytes = bytes.len(),
            spilled = body.is_spilled(),
            "message buffered"
        );

        // D-085 — the debugging capture, here and deliberately *before* the
        // relay.
        //
        // Two things follow from the position, and both are the point. A record
        // written here cannot carry an outcome, a route or a retry count,
        // because none of them exists yet — so nothing can mistake a capture
        // directory for a spool, and §2.2 stays true. And `on_error: defer` can
        // answer `451` honestly, because nothing has been relayed: raised after
        // the downstream conversation, that reply would defer a message the
        // downstream had already accepted and the client's retry would deliver
        // it twice (§10.2, D-068).
        //
        // What it does not see: a message the D-071 header ACL refused above,
        // and a message refused before the final dot. Neither was ever accepted.
        // The reply this message does get is joinable on `correlation_id`, which
        // §9.5 puts on every log line.
        if let Some(cap) = self.engine.capture.clone() {
            let record = capture::Record::build(
                capture::Ingress {
                    correlation_id: &self.correlation_id,
                    at: chrono::Utc::now(),
                    peer: self.peer,
                    listener: &self.policy.address,
                    helo: self.greeted.as_deref().unwrap_or_default(),
                    tls: self.encrypted(),
                    auth_user: self.user.as_deref(),
                    mail_from: tx.mail_from.as_deref(),
                    rcpt_to: &tx.recipients,
                    // A label for whoever reads the file, from the header block
                    // already in hand. Never read back: a replay sends the body.
                    subject: capture::subject_from_headers(&head),
                    params: capture::Params {
                        size: tx.params.size,
                        body_8bitmime: tx.params.body_8bitmime,
                        smtputf8: tx.params.smtputf8,
                        auth_identity: tx.params.auth_identity.clone(),
                    },
                    body: &bytes,
                },
                cap.max_body_bytes(),
            );

            match cap.on_error() {
                CaptureOnError::Continue => {
                    // Never awaited: a slow disk must not become backpressure on
                    // the relay. A drop is a gap in a debugging artefact.
                    let _ = cap.offer(record);
                }
                CaptureOnError::Defer => {
                    if let Err(reason) = cap.offer_durable(record).await {
                        tracing::error!(
                            correlation_id = %self.correlation_id,
                            reason = %reason,
                            "the message was not captured and capture.on_error is defer; \
                             deferring it. Nothing has been relayed"
                        );
                        metrics::capture_deferred();
                        return reply::capture_unavailable();
                    }
                }
            }
        }

        // Everything from here is §7.4: reserve, relay, commit or release. The
        // reservation is taken inside, immediately before the conversation, and
        // resolved on every path out (O-1, §3.3's no-failover rule).
        let engine = self.engine.clone();
        // §6.1 step 8's raw material. `greeted` is `Some` by here — §5.2 rejects
        // `MAIL FROM` before a greeting — but a missing one is not worth failing
        // an accepted message over.
        let helo = self.greeted.clone().unwrap_or_default();
        let peer = self.peer.ip().to_string();

        // §5.8 — chosen here, at the final dot, and nowhere else.
        let ingress = ramp_select::ingress(
            &engine.config,
            self.policy.ramp.as_deref(),
            self.policy.header_overrides_affinity,
            self.user.as_deref(),
        );
        let selection = ramp_select::select(&engine.config, &ingress, &ramp_header);
        ramp_select::record(&selection, &self.correlation_id);

        relay::reserve_relay_commit(
            &engine,
            &selection,
            &senders,
            relay::Message {
                mail_from: tx.mail_from.as_deref(),
                recipients: &tx.recipients,
                body: &bytes,
                smtputf8: tx.params.smtputf8,
                body_8bitmime: tx.params.body_8bitmime,
                helo: &helo,
                peer: &peer,
                authenticated: self.user.is_some(),
                tls: self.encrypted(),
            },
            &self.correlation_id,
        )
        .await
    }

    // -- I/O -------------------------------------------------------------

    async fn read_command_line(&mut self) -> Result<Option<String>, ReadError> {
        // D-106 — §10.4's hard stop, observed here and not mid-relay. Checked
        // before the read for the same reason the zero deadline is below: a
        // pipelined command would otherwise be served after the stop.
        if self.hard_stop.is_cancelled() {
            return Err(ReadError::ShuttingDown);
        }
        let stop = self.hard_stop.clone();
        let (timeout, by_deadline) = self.budget(self.config().server.timeouts.command);
        // `timeout` polls the read once before looking at the clock, so a command
        // already pipelined would still be served with no time left. Without this,
        // a client that keeps its pipe full is never refused.
        if by_deadline && timeout.is_zero() {
            // D-081 — the deadline passed while Simmer was busy, typically in a
            // relay. The client is usually writing its next command right now,
            // so the refusal answers that command rather than arriving
            // unprompted: a 421 sent and the socket closed while the client
            // writes left it with a broken pipe and no reply at all (soak V4).
            // The command is read and refused, never served, and a client that
            // sends nothing gets the 421 once the grace has passed.
            let mut discard = String::new();
            let _ = tokio::time::timeout(
                DEADLINE_GRACE,
                (&mut self.io)
                    .take(MAX_COMMAND_LINE)
                    .read_line(&mut discard),
            )
            .await;
            return Err(ReadError::SessionTimeout);
        }
        let read = async {
            let mut line = String::new();
            let n = (&mut self.io)
                .take(MAX_COMMAND_LINE)
                .read_line(&mut line)
                .await;
            (n, line)
        };

        let read = tokio::time::timeout(timeout, read);
        let read = tokio::select! {
            biased;
            _ = stop.cancelled() => return Err(ReadError::ShuttingDown),
            r = read => r,
        };
        match read {
            Err(_) if by_deadline => Err(ReadError::SessionTimeout),
            Err(_) => Err(ReadError::Timeout),
            Ok((Err(_), _)) => Err(ReadError::Io),
            Ok((Ok(0), _)) => Ok(None),
            Ok((Ok(n), line)) => {
                // No CRLF within the cap means the line is over-long.
                if n as u64 >= MAX_COMMAND_LINE && !line.ends_with('\n') {
                    return Err(ReadError::TooLong);
                }
                Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
            }
        }
    }

    /// Read the `DATA` payload into `body`, unstuffing and normalising as it goes.
    ///
    /// The whole transfer shares one `timeouts.data` budget rather than one per
    /// line: a per-line timeout cannot distinguish a slow link from a stalled one
    /// on a message with a million lines. It is capped by what the session has
    /// left (D-081).
    async fn read_data(
        &mut self,
        body: &mut MessageBuffer,
        max: u64,
        timeout: Duration,
    ) -> Result<(), DataError> {
        let (budget, by_deadline) = self.budget(timeout);
        // As in `read_command_line`: a body already buffered is not read with no
        // time left.
        if by_deadline && budget.is_zero() {
            return Err(DataError::SessionTimeout);
        }
        if self.hard_stop.is_cancelled() {
            return Err(DataError::ShuttingDown);
        }
        let stop = self.hard_stop.clone();
        let read = tokio::select! {
            biased;
            _ = stop.cancelled() => return Err(DataError::ShuttingDown),
            r = tokio::time::timeout(budget, self.read_data_inner(body, max)) => r,
        };
        match read {
            Err(_) if by_deadline => Err(DataError::SessionTimeout),
            Err(_) => Err(DataError::Timeout),
            Ok(r) => r,
        }
    }

    async fn read_data_inner(
        &mut self,
        body: &mut MessageBuffer,
        max: u64,
    ) -> Result<(), DataError> {
        let mut line = Vec::with_capacity(1024);
        let mut over = false;
        // The 354 reply leaves the client at the start of a line, so the first
        // line of the payload counts as following a CRLF.
        let mut after_crlf = true;
        let mut ambiguous = false;

        loop {
            line.clear();
            // D-082 (finding F1): at most one byte past the cap, so an over-long
            // line is known to be one without being held whole. `read_until`
            // alone kept a line with no LF for as long as the client kept sending.
            let n = (&mut self.io)
                .take(MAX_DATA_LINE as u64 + 1)
                .read_until(b'\n', &mut line)
                .await
                .map_err(|_| DataError::Io)?;

            if n == 0 {
                // EOF mid-DATA. Nothing was promised — no 250 has been sent —
                // and there is nobody left to tell.
                return Err(DataError::Closed);
            }

            if line.len() > MAX_DATA_LINE {
                over = true;
                // The discarded remainder ends in an LF we do not inspect, so
                // what precedes the next line is unknown. Treating it as a CRLF
                // is safe: the message is already refused as too large.
                after_crlf = true;
                // If the cap stopped the read short of the LF, the rest of the
                // line is still in the stream, and is thrown away as it arrives.
                // It cannot hold the terminator, which is a line of its own.
                if !line.ends_with(b"\n") {
                    self.discard_line().await?;
                }
                continue;
            }

            // Strip the line ending. A bare LF is promoted to CRLF by virtue of
            // being stripped here and re-added on transmit, which is the
            // normalisation §8.1's "canonical message" depends on.
            let content = strip_eol(&line);
            let ends_crlf = line.ends_with(b"\r\n");
            let preceded_by_crlf = std::mem::replace(&mut after_crlf, ends_crlf);

            // The terminator is `CRLF . CRLF` and nothing else (RFC 5321
            // §4.1.1.4). A dot line with a bare LF on either side is what SMTP
            // smuggling rides on: a sender that honours only CRLF.CRLF writes it
            // inside one message, and a receiver that also honours LF.LF reads
            // everything after it as fresh commands — a second envelope with
            // recipients the first never had, relayed through a route whose
            // reputation is the whole point. Such a line is consumed as data, so
            // what follows it is never executed and the session stays in step,
            // and the message is refused once the real terminator arrives.
            if content == b"." {
                if !(ends_crlf && preceded_by_crlf) {
                    ambiguous = true;
                    continue;
                }
                if ambiguous {
                    // Counted here rather than in the caller's error arm: an
                    // over-long message is answered `552` and disconnected
                    // (D-020) whatever else it held, and padding one line past
                    // `MAX_DATA_LINE` must not be a way to mute the signal this
                    // counter exists to raise.
                    metrics::ambiguous_terminator();
                }
                if over {
                    return Err(DataError::TooLarge);
                }
                if ambiguous {
                    return Err(DataError::AmbiguousTerminator);
                }
                // D-080: the spill file's last batch is written here, inside
                // DATA, so a full spill area fails the way it always has (F5).
                body.finish().await.map_err(|_| DataError::Io)?;
                return Ok(());
            }

            // The same attack with CR: Simmer never ends DATA on `<CR>.<CR>`, but
            // it forwards bare CRs verbatim, and a downstream that does honour it
            // would split the message on the way out instead.
            //
            // Scanned over `line`, not `content`: `strip_eol` has already taken
            // the line's own CR, and `...<CR>.` plus the CRLF that transmission
            // restores is `...<CR>.<CR><LF>` on the wire — the same marker, and
            // the spelling the published attack uses. `line` is what gets
            // forwarded, so `line` is what has to be clean.
            if has_cr_dot_cr(&line) {
                ambiguous = true;
            }

            let content = buffer::unstuff(content);

            // +2 for the CRLF that transmission will restore.
            if body.len() as u64 + content.len() as u64 + 2 > max {
                over = true;
            }

            if !over {
                body.append(content).await.map_err(|_| DataError::Io)?;
                body.append(b"\r\n").await.map_err(|_| DataError::Io)?;
            }
        }
    }

    /// The rest of an over-long `DATA` line, up to and including its LF, read and
    /// dropped a buffer at a time (D-082).
    async fn discard_line(&mut self) -> Result<(), DataError> {
        loop {
            let buf = self.io.fill_buf().await.map_err(|_| DataError::Io)?;
            if buf.is_empty() {
                return Err(DataError::Closed);
            }
            let (used, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (buf.len(), false),
            };
            self.io.consume(used);
            if done {
                return Ok(());
            }
        }
    }

    /// Write one reply, bounded by `timeouts.command`.
    ///
    /// Unbounded, a peer that stops reading blocks `write_all` forever once the
    /// socket buffers fill, and the session holds its `max_concurrent_sessions`
    /// permit for as long (D-101). Expiry is reported as an I/O error, which ends
    /// the session: nothing useful can be said to a peer that is not listening.
    async fn send(&mut self, reply: &Reply) -> std::io::Result<()> {
        tracing::trace!(peer = %self.peer, reply = %reply, "-->");
        let budget = self.config().server.timeouts.command;
        let io = self.io.get_mut();
        let write = async {
            io.write_all(reply.to_wire().as_bytes()).await?;
            io.flush().await
        };
        match tokio::time::timeout(budget, write).await {
            Ok(r) => r,
            Err(_) => {
                tracing::info!(peer = %self.peer, "client stopped reading; reply write timed out");
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "reply write timed out",
                ))
            }
        }
    }

    fn reset_transaction(&mut self) {
        self.transaction = None;
    }

    /// End the transaction with its final reply: §9.6's `smtp.transaction`
    /// records the code — a status of `ERROR` for anything but `2xx` — and ends
    /// as the transaction is dropped.
    fn finish_transaction(&mut self, r: &Reply) {
        if let Some(tx) = &self.transaction {
            tx.span.record("smtp.reply.code", r.code);
            if r.code >= 400 {
                tx.span.record("otel.status_code", "ERROR");
            }
        }
        self.reset_transaction();
    }

    /// End the connection cleanly: TLS `close_notify` then a TCP FIN on an
    /// encrypted session, a FIN on a plaintext one.
    ///
    /// Dropping a TLS stream sends no `close_notify`, and a client cannot tell
    /// that from a truncation attack — rustls reports it as an unexpected EOF
    /// rather than a close, so a `221` after `QUIT` would arrive followed by an
    /// error (D-070). Bounded, because a peer that has stopped reading must not
    /// hold the session's permit while the alert fails to flush.
    pub async fn close(&mut self) {
        let _ = tokio::time::timeout(CLOSE_BUDGET, self.io.get_mut().shutdown()).await;
    }
}

enum ReadError {
    Timeout,
    /// D-081 — the session's deadline ran out, not the command budget.
    SessionTimeout,
    /// D-106 — §10.4's hard stop fired while waiting on the client.
    ShuttingDown,
    TooLong,
    Io,
}

enum DataError {
    Timeout,
    /// D-081 — the session's deadline ran out, not the data budget.
    SessionTimeout,
    /// D-106 — §10.4's hard stop fired mid-transfer.
    ShuttingDown,
    TooLarge,
    /// A `.` line with a bare LF beside it, or a `<CR>.<CR>`: an end-of-data
    /// marker that other SMTP implementations may honour and this one does not.
    AmbiguousTerminator,
    Closed,
    Io,
}

/// Whether `content` holds `<CR>.<CR>`, the bare-CR spelling of a terminator.
fn has_cr_dot_cr(content: &[u8]) -> bool {
    content.windows(3).any(|w| w == b"\r.\r")
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

fn new_correlation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The first address of the `From:` header (§5.4, §6.3).
///
/// Returns `None` when the header is absent, unparseable, or a group with no
/// addresses — which §5.4 turns into `550 5.6.0` if a rule needs it.
pub fn first_from_address(header_block: &[u8]) -> Option<String> {
    let parsed = mail_parser::MessageParser::default().parse_headers(header_block)?;
    let from = parsed.from()?;
    from.first()
        .and_then(|addr| addr.address())
        .map(|a| a.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_both_line_endings_and_neither() {
        assert_eq!(strip_eol(b"abc\r\n"), b"abc");
        assert_eq!(strip_eol(b"abc\n"), b"abc");
        assert_eq!(strip_eol(b"abc"), b"abc");
        assert_eq!(strip_eol(b"\r\n"), b"");
        assert_eq!(strip_eol(b""), b"");
    }

    #[test]
    fn finds_a_bare_cr_terminator_anywhere_in_a_line() {
        // Fed the whole line, line ending included, because that is what gets
        // forwarded.
        assert!(has_cr_dot_cr(b"hello\r.\rMAIL FROM:<a@b>\r\n"));
        assert!(has_cr_dot_cr(b"\r.\r"));
        // The line's own CR completes the marker: on the wire this is
        // `hello<CR>.<CR><LF>`, and `strip_eol` would have hidden it.
        assert!(has_cr_dot_cr(b"hello\r.\r\n"));
        assert!(has_cr_dot_cr(b"\r.\r\n"));
        assert!(!has_cr_dot_cr(b"a.b\r\n"));
        assert!(!has_cr_dot_cr(b"end.\r\n"));
        assert!(!has_cr_dot_cr(b"\r.x\r\n"));
    }

    // -- §5.4 From: extraction -------------------------------------------

    #[test]
    fn extracts_a_plain_from_address() {
        assert_eq!(
            first_from_address(b"From: jane@oldbrand.com\r\n\r\n").as_deref(),
            Some("jane@oldbrand.com")
        );
    }

    #[test]
    fn extracts_the_address_from_a_display_name_form() {
        assert_eq!(
            first_from_address(b"From: Jane Smith <jane@oldbrand.com>\r\n\r\n").as_deref(),
            Some("jane@oldbrand.com")
        );
    }

    #[test]
    fn takes_the_first_address_when_from_has_several() {
        // §5.4: "the domain (or address) of the **first** address in the From:
        // header".
        assert_eq!(
            first_from_address(b"From: a@one.com, b@two.com\r\n\r\n").as_deref(),
            Some("a@one.com")
        );
    }

    #[test]
    fn survives_a_folded_from_header() {
        // Folding is the case a naive line-oriented scan gets wrong.
        assert_eq!(
            first_from_address(b"From: Jane Smith\r\n <jane@oldbrand.com>\r\n\r\n").as_deref(),
            Some("jane@oldbrand.com")
        );
    }

    #[test]
    fn ignores_other_headers_including_a_later_sender() {
        let head = b"Received: from x\r\nSender: bounce@other.com\r\n\
                     From: jane@oldbrand.com\r\nSubject: hi\r\n\r\n";
        assert_eq!(
            first_from_address(head).as_deref(),
            Some("jane@oldbrand.com")
        );
    }

    #[test]
    fn returns_none_for_an_absent_or_empty_from() {
        // §5.4 turns each of these into 550 5.6.0 when match_on requires the
        // header.
        assert_eq!(first_from_address(b"Subject: no from here\r\n\r\n"), None);
        assert_eq!(first_from_address(b""), None);
        // A group with no addresses — the case §5.4 names explicitly.
        assert_eq!(
            first_from_address(b"From: undisclosed-recipients:;\r\n\r\n"),
            None
        );
    }

    #[test]
    fn tolerates_a_truncated_header_block() {
        // header_block() caps its scan, so the parser must cope with a message
        // cut mid-header rather than failing the whole routing decision.
        assert_eq!(
            first_from_address(b"From: jane@oldbrand.com\r\nSubject: trunc").as_deref(),
            Some("jane@oldbrand.com")
        );
    }
}
