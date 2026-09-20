//! The SMTP client `server replay` speaks with.
//!
//! ## Why this is not `downstream::client`
//!
//! `downstream::client` is the **relay leg**: it needs a `&Route` for its
//! identity and timeouts, a `&Pool` for §8.3's connection bound, and it carries
//! D-068's single retry with the three conditions that make it safe. None of
//! that fits here. A replay has no route, wants no pool, and must never retry —
//! D-068's third condition exists because a retry at the final dot is how one
//! message becomes two, and replay is already sending a second copy on purpose.
//!
//! ## Why this is not `loadgen`'s conversation
//!
//! `src/bin/loadgen.rs` has the same conversation and cannot be reached from
//! here: it is a `[[bin]]`, and it is not in the shipped image. Lifting it whole
//! would bring `Behaviour::{Silent, NoopIdle, DataTrickle, NoLf}` — a client that
//! holds a connection open and dribbles one byte at a time — into the runtime
//! image, where §2.3's trusted-segment assumption is doing real work. This
//! surface **cannot express any of them**: there is no hold, no trickle, no
//! partial-line write. That is structural, not a comment.
//!
//! ## Why it speaks SMTP by hand
//!
//! D-022, the same reason `downstream::client` does: a client crate normalises
//! the reply into an error type, and the reply code is exactly what a replay
//! reports.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::downstream::stream::Stream;
use crate::smtp::buffer::stuff_into;

/// How long to wait for any single reply. Generous: the target is a Simmer, and
/// a Simmer holds the client connection for the whole downstream conversation
/// (§2.2), so a reply to the final dot legitimately takes as long as the real
/// downstream does.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where to send, and how.
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    /// The name to verify the certificate against, and to send as SNI. Defaults
    /// to `host`.
    pub tls_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Plaintext throughout.
    Off,
    /// `STARTTLS` after the first `EHLO`, then a second `EHLO` (RFC 3207 §4.2).
    Starttls,
    /// TLS from the first byte (RFC 8314).
    Implicit,
}

impl TlsMode {
    pub fn parse(s: &str) -> Option<TlsMode> {
        match s {
            "off" | "plain" => Some(TlsMode::Off),
            "starttls" => Some(TlsMode::Starttls),
            "implicit" => Some(TlsMode::Implicit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMech {
    Plain,
    Login,
}

impl AuthMech {
    pub fn parse(s: &str) -> Option<AuthMech> {
        match s {
            "plain" => Some(AuthMech::Plain),
            "login" => Some(AuthMech::Login),
            _ => None,
        }
    }
}

/// A username and the password resolved for it.
///
/// `Debug` is hand-written and prints the password as `<redacted>`, the
/// convention `config::User` and `config::Admin` follow: a panic message or a
/// stray `dbg!` must not be able to put a credential in a terminal.
#[derive(Clone)]
pub struct Credentials {
    pub user: String,
    pub password: String,
    pub mech: AuthMech,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("mech", &self.mech)
            .finish()
    }
}

/// One transaction, as the original client presented it.
pub struct Submission<'a> {
    pub helo: &'a str,
    /// `None` and `Some("")` both mean the null sender on the wire; the record
    /// keeps them distinct, and so does this.
    pub mail_from: Option<&'a str>,
    pub rcpt_to: &'a [String],
    /// The canonical message: unstuffed and CRLF-normalised. Re-stuffed here.
    pub body: &'a [u8],
    pub smtputf8: bool,
    pub body_8bitmime: bool,
}

/// How far the conversation got, and what it was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The reply code, or `0` when there was no reply at all (a transport
    /// failure). `0` is never a real SMTP code, so it cannot be confused for one.
    pub code: u16,
    pub stage: Stage,
    pub text: String,
}

impl Outcome {
    pub fn is_accepted(&self) -> bool {
        (200..300).contains(&self.code)
    }

    fn at(stage: Stage, code: u16, text: impl Into<String>) -> Outcome {
        Outcome {
            code,
            stage,
            text: text.into(),
        }
    }
}

/// The vocabulary is `loadgen`'s, so a replay's output and a load run's are
/// directly comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Transport,
    Banner,
    Tls,
    Ehlo,
    Auth,
    MailFrom,
    RcptTo,
    Data,
    FinalDot,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Banner => "banner",
            Self::Tls => "tls",
            Self::Ehlo => "ehlo",
            Self::Auth => "auth",
            Self::MailFrom => "mail_from",
            Self::RcptTo => "rcpt_to",
            Self::Data => "data",
            Self::FinalDot => "final_dot",
        }
    }
}

/// Connect, greet, optionally authenticate, send one message, and quit.
///
/// One connection per message. Not an optimisation to undo later: a captured
/// record says nothing about which connection carried it, so reusing one would
/// be inventing a grouping that was never observed.
pub async fn send_one(
    target: &Target,
    tls: Option<&tokio_rustls::TlsConnector>,
    credentials: Option<&Credentials>,
    message: &Submission<'_>,
) -> Outcome {
    let tcp = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((target.host.as_str(), target.port)),
    )
    .await
    {
        Err(_) => return Outcome::at(Stage::Transport, 0, "connect timed out"),
        Ok(Err(e)) => return Outcome::at(Stage::Transport, 0, format!("connect: {e}")),
        Ok(Ok(s)) => s,
    };

    let stream = if target.tls == TlsMode::Implicit {
        let Some(connector) = tls else {
            return Outcome::at(Stage::Transport, 0, "implicit TLS without a connector");
        };
        match handshake(connector, &target.tls_name, tcp).await {
            Ok(s) => s,
            Err(e) => return Outcome::at(Stage::Transport, 0, e),
        }
    } else {
        Stream::Plain(tcp)
    };

    let mut io = BufReader::new(stream);

    match read_reply(&mut io).await {
        Err(e) => return Outcome::at(Stage::Transport, 0, e),
        Ok((code, text)) if code != 220 => return Outcome::at(Stage::Banner, code, text),
        Ok(_) => {}
    }

    if let Err(o) = ehlo(&mut io, message.helo, Stage::Ehlo).await {
        return o;
    }

    if target.tls == TlsMode::Starttls {
        if let Err(o) = write(&mut io, "STARTTLS\r\n", Stage::Tls).await {
            return o;
        }
        match read_reply(&mut io).await {
            Err(e) => return Outcome::at(Stage::Transport, 0, e),
            Ok((code, text)) if code != 220 => return Outcome::at(Stage::Tls, code, text),
            Ok(_) => {}
        }
        let Stream::Plain(tcp) = std::mem::replace(io.get_mut(), Stream::Taken) else {
            return Outcome::at(Stage::Tls, 0, "STARTTLS on a stream that is not plaintext");
        };
        let Some(connector) = tls else {
            return Outcome::at(Stage::Tls, 0, "STARTTLS without a connector");
        };
        match handshake(connector, &target.tls_name, tcp).await {
            Ok(s) => io = BufReader::new(s),
            Err(e) => return Outcome::at(Stage::Tls, 0, e),
        }
        // RFC 3207 §4.2: everything learned before the handshake is void.
        if let Err(o) = ehlo(&mut io, message.helo, Stage::Tls).await {
            return o;
        }
    }

    if let Some(c) = credentials {
        match auth(&mut io, c).await {
            Err(o) => return o,
            Ok((code, text)) if code != 235 => {
                let _ = write(&mut io, "QUIT\r\n", Stage::Auth).await;
                return Outcome::at(Stage::Auth, code, text);
            }
            Ok(_) => {}
        }
    }

    let outcome = transaction(&mut io, message).await;
    let _ = write(&mut io, "QUIT\r\n", Stage::Transport).await;
    outcome
}

/// `MAIL FROM` through the final dot.
async fn transaction(io: &mut BufReader<Stream>, m: &Submission<'_>) -> Outcome {
    // The parameters the original client presented. Omitting them would not be
    // replaying the same transaction: `SMTPUTF8` in particular changes whether
    // the target accepts the addresses at all (D-018).
    let mut params = String::new();
    if m.body_8bitmime {
        params.push_str(" BODY=8BITMIME");
    }
    if m.smtputf8 {
        params.push_str(" SMTPUTF8");
    }

    let from = m.mail_from.unwrap_or_default();
    if let Err(o) = step(
        io,
        &format!("MAIL FROM:<{from}>{params}\r\n"),
        250,
        Stage::MailFrom,
    )
    .await
    {
        return o;
    }

    // D-047 pins a transaction at one recipient, so this is a loop of one in
    // practice. It is written over whatever the record holds so that a capture
    // taken by a build with a different rule still replays.
    for rcpt in m.rcpt_to {
        if let Err(o) = step(io, &format!("RCPT TO:<{rcpt}>\r\n"), 250, Stage::RcptTo).await {
            return o;
        }
    }

    if let Err(o) = step(io, "DATA\r\n", 354, Stage::Data).await {
        return o;
    }

    // `stuff_into` is the library's own, the same function the outbound leg
    // uses, so a replay and a relay cannot disagree about what dot-stuffing is.
    let mut wire = Vec::with_capacity(m.body.len() + 64);
    stuff_into(&mut wire, m.body);
    if let Err(e) = io.get_mut().write_all(&wire).await {
        return Outcome::at(Stage::FinalDot, 0, format!("write: {e}"));
    }
    if let Err(e) = io.get_mut().flush().await {
        return Outcome::at(Stage::FinalDot, 0, format!("flush: {e}"));
    }

    match read_reply(io).await {
        Err(e) => Outcome::at(Stage::Transport, 0, e),
        Ok((code, text)) => Outcome::at(Stage::FinalDot, code, text),
    }
}

async fn ehlo(io: &mut BufReader<Stream>, helo: &str, stage: Stage) -> Result<(), Outcome> {
    // A captured `helo` is whatever the application sent; an empty one would
    // produce a syntactically invalid command.
    let name = if helo.trim().is_empty() {
        "replay.invalid"
    } else {
        helo
    };
    write(io, &format!("EHLO {name}\r\n"), stage).await?;
    match read_reply(io).await {
        Err(e) => Err(Outcome::at(Stage::Transport, 0, e)),
        Ok((code, text)) if code != 250 => Err(Outcome::at(stage, code, text)),
        Ok(_) => Ok(()),
    }
}

async fn auth(io: &mut BufReader<Stream>, c: &Credentials) -> Result<(u16, String), Outcome> {
    match c.mech {
        AuthMech::Login => {
            write(io, "AUTH LOGIN\r\n", Stage::Auth).await?;
            let (code, text) = reply(io).await?;
            if code != 334 {
                return Ok((code, text));
            }
            write(
                io,
                &format!("{}\r\n", B64.encode(c.user.as_bytes())),
                Stage::Auth,
            )
            .await?;
            let (code, text) = reply(io).await?;
            if code != 334 {
                return Ok((code, text));
            }
            write(
                io,
                &format!("{}\r\n", B64.encode(c.password.as_bytes())),
                Stage::Auth,
            )
            .await?;
            reply(io).await
        }
        AuthMech::Plain => {
            // RFC 4616: NUL authzid, NUL-separated.
            let payload = format!("\0{}\0{}", c.user, c.password);
            write(
                io,
                &format!("AUTH PLAIN {}\r\n", B64.encode(payload.as_bytes())),
                Stage::Auth,
            )
            .await?;
            reply(io).await
        }
    }
}

async fn reply(io: &mut BufReader<Stream>) -> Result<(u16, String), Outcome> {
    read_reply(io)
        .await
        .map_err(|e| Outcome::at(Stage::Transport, 0, e))
}

/// Send one command and require one reply code. Anything else — including a
/// transport failure — is the outcome, and the conversation stops there.
async fn step(
    io: &mut BufReader<Stream>,
    line: &str,
    want: u16,
    stage: Stage,
) -> Result<(), Outcome> {
    write(io, line, stage).await?;
    match read_reply(io).await {
        Err(e) => Err(Outcome::at(Stage::Transport, 0, e)),
        Ok((code, text)) if code != want => Err(Outcome::at(stage, code, text)),
        Ok(_) => Ok(()),
    }
}

async fn write(io: &mut BufReader<Stream>, s: &str, stage: Stage) -> Result<(), Outcome> {
    io.get_mut()
        .write_all(s.as_bytes())
        .await
        .map_err(|e| Outcome::at(stage, 0, format!("write: {e}")))?;
    io.get_mut()
        .flush()
        .await
        .map_err(|e| Outcome::at(stage, 0, format!("flush: {e}")))
}

async fn handshake(
    connector: &tokio_rustls::TlsConnector,
    name: &str,
    tcp: TcpStream,
) -> Result<Stream, String> {
    let server_name = rustls::pki_types::ServerName::try_from(name.to_string())
        .map_err(|_| format!("'{name}' is not a valid TLS server name"))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| format!("TLS handshake: {e}"))?;
    Ok(Stream::Tls(Box::new(tls.into())))
}

/// One reply, following multi-line continuations. The text is the last line's.
pub(crate) async fn read_reply(io: &mut BufReader<Stream>) -> Result<(u16, String), String> {
    loop {
        let mut line = String::new();
        match tokio::time::timeout(REPLY_TIMEOUT, io.read_line(&mut line)).await {
            Err(_) => return Err("read timed out".to_string()),
            Ok(Err(e)) => return Err(format!("read: {e}")),
            Ok(Ok(0)) => return Err("connection closed".to_string()),
            Ok(Ok(_)) => {}
        }
        let trimmed = line.trim_end();
        if trimmed.len() < 3 {
            return Err(format!("short reply: {trimmed}"));
        }
        let code: u16 = trimmed[..3]
            .parse()
            .map_err(|_| format!("unparseable reply: {trimmed}"))?;
        // A dash in the fourth column means another line follows.
        if trimmed.as_bytes().get(3) == Some(&b'-') {
            continue;
        }
        return Ok((code, trimmed.get(4..).unwrap_or_default().to_string()));
    }
}

/// A TLS client that trusts `ca` — a PEM file, or `os` for the platform store.
///
/// A replay against a Simmer with a certificate should verify it. An
/// unverified handshake would show the bytes were encrypted, not that the
/// target is the target.
pub fn connector(ca: &str) -> Result<tokio_rustls::TlsConnector, String> {
    use rustls::pki_types::pem::PemObject as _;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    if ca == "os" {
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            return Err("--ca os: the platform trust store is empty".to_string());
        }
    } else {
        let cert = rustls::pki_types::CertificateDer::from_pem_file(ca)
            .map_err(|e| format!("reading --ca {ca}: {e}"))?;
        roots
            .add(cert)
            .map_err(|e| format!("--ca {ca} is not a usable trust anchor: {e}"))?;
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(
        config,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_never_prints_its_password() {
        let c = Credentials {
            user: "cfapp".to_string(),
            password: "hunter2".to_string(),
            mech: AuthMech::Plain,
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(shown.contains("cfapp"), "the username is not a secret");
    }

    #[test]
    fn the_modes_and_mechanisms_parse_from_what_the_flags_accept() {
        assert_eq!(TlsMode::parse("off"), Some(TlsMode::Off));
        assert_eq!(TlsMode::parse("starttls"), Some(TlsMode::Starttls));
        assert_eq!(TlsMode::parse("implicit"), Some(TlsMode::Implicit));
        assert_eq!(TlsMode::parse("tls"), None);
        assert_eq!(AuthMech::parse("plain"), Some(AuthMech::Plain));
        assert_eq!(AuthMech::parse("login"), Some(AuthMech::Login));
        assert_eq!(AuthMech::parse("cram-md5"), None);
    }

    #[test]
    fn a_transport_failure_is_code_zero_which_is_never_a_real_reply() {
        let o = Outcome::at(Stage::Transport, 0, "connect refused");
        assert!(!o.is_accepted());
        assert_eq!(o.stage.as_str(), "transport");
    }

    #[test]
    fn only_2xx_counts_as_accepted() {
        for (code, want) in [
            (250, true),
            (200, true),
            (299, true),
            (354, false),
            (451, false),
            (550, false),
            (0, false),
        ] {
            assert_eq!(
                Outcome::at(Stage::FinalDot, code, "").is_accepted(),
                want,
                "{code}"
            );
        }
    }
}
