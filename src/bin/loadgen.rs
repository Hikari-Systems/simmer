//! The acceptance suite's bulk sender (`docs/ACCEPTANCE.md` §2).
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
//! Deliberately dumb. It sends, it records `(recipient, code, text)`, and it
//! writes that to stdout as JSON. **It asserts nothing** — every assertion lives
//! in `tests/acceptance.rs`, where a failure is legible and where the expected
//! numbers come from `simmer.acceptance.yaml` rather than from here.
//!
//! ## Why it speaks SMTP by hand
//!
//! The same reason `src/downstream/client.rs` does (D-022): the client crates
//! normalise the reply into an error type, and this harness's entire output is
//! the reply codes. `swaks` or a shell loop would work too, but this way the
//! sender and the service under test are built from one `cargo build` and cannot
//! drift apart in the image.
//!
//! ## `--starttls` (D-070)
//!
//! Submits over RFC 3207 instead of plaintext, **verifying** the server's
//! certificate against `--ca` — the CA the compose `tls-init` service minted —
//! for `--tls-name`. Verification is the point: an unverified handshake would
//! show the bytes were encrypted, not that Simmer served the certificate it was
//! configured with. The upgrade reuses the library's own [`Stream`], as the
//! outbound leg does.

use std::sync::Arc;
use std::time::Duration;

use simmer::downstream::stream::Stream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug)]
struct Args {
    host: String,
    port: u16,
    username: String,
    password: String,
    count: usize,
    /// Envelope sender, which is what §5.4 matches on in the acceptance config.
    from: String,
    /// `From:` header, so the harness can send arrangement A and arrangement B
    /// of §1.1 through the same binary.
    from_header: String,
    recipient_domain: String,
    /// Distinguishes one run's recipients from the next in the same trap.
    tag: String,
    subject: String,
    /// RFC 3207 before AUTH, verifying against `ca` for `tls_name`.
    starttls: bool,
    ca: String,
    tls_name: String,
}

fn args() -> Args {
    let mut a = Args {
        host: "app".to_string(),
        port: 25,
        username: "cfapp".to_string(),
        password: String::new(),
        count: 1,
        from: "jane@oldbrand.com".to_string(),
        from_header: "Jane Smith <jane@oldbrand.com>".to_string(),
        recipient_domain: "example.net".to_string(),
        tag: "run".to_string(),
        subject: "Your order has shipped".to_string(),
        starttls: false,
        ca: "/tls/ca.pem".to_string(),
        tls_name: "simmer.acceptance".to_string(),
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let value = || {
            argv.get(i + 1)
                .unwrap_or_else(|| panic!("{} needs a value", argv[i]))
                .clone()
        };
        // The one flag that takes no value.
        if argv[i] == "--starttls" {
            a.starttls = true;
            i += 1;
            continue;
        }
        match argv[i].as_str() {
            "--host" => a.host = value(),
            "--port" => a.port = value().parse().expect("--port"),
            "--username" => a.username = value(),
            "--password" => a.password = value(),
            "--count" => a.count = value().parse().expect("--count"),
            "--from" => a.from = value(),
            "--from-header" => a.from_header = value(),
            "--recipient-domain" => a.recipient_domain = value(),
            "--tag" => a.tag = value(),
            "--subject" => a.subject = value(),
            "--ca" => a.ca = value(),
            "--tls-name" => a.tls_name = value(),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }

    if a.password.is_empty() {
        a.password = std::env::var("SIMMER_PASSWORD").unwrap_or_default();
    }
    a
}

#[tokio::main]
async fn main() {
    let args = args();
    let tls = args.starttls.then(|| connector(&args.ca));
    let mut results = Vec::with_capacity(args.count);

    for n in 0..args.count {
        let recipient = format!("{}-{n}@{}", args.tag, args.recipient_domain);
        let outcome = send_one(&args, tls.as_ref(), &recipient).await;
        let (code, text) = match outcome {
            Ok((code, text)) => (code, text),
            // A transport failure is reported as a code of 0 rather than
            // crashing the run: the interesting case is "message 6 of 8 got a
            // 451", and aborting would lose the five that succeeded.
            Err(e) => (0, e),
        };
        results.push(format!(
            r#"{{"recipient":{},"code":{code},"text":{}}}"#,
            json_string(&recipient),
            json_string(&text)
        ));
    }

    println!("[{}]", results.join(","));
}

/// One connection per message. Slower than reusing one, and the point: it is
/// what an application sending in bulk through a pool actually looks like to
/// Simmer, and it exercises the §5.1 session cap.
async fn send_one(
    args: &Args,
    tls: Option<&tokio_rustls::TlsConnector>,
    recipient: &str,
) -> Result<(u16, String), String> {
    let stream = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect((args.host.as_str(), args.port)),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| format!("connect: {e}"))?;

    let mut io = BufReader::new(Stream::Plain(stream));
    expect(&mut io, 220).await?;

    write(&mut io, "EHLO loadgen.acceptance\r\n").await?;
    read_multiline(&mut io, 250).await?;

    if let Some(connector) = tls {
        write(&mut io, "STARTTLS\r\n").await?;
        expect(&mut io, 220).await?;
        let Stream::Plain(tcp) = std::mem::replace(io.get_mut(), Stream::Taken) else {
            return Err("STARTTLS on a stream that is not plaintext".to_string());
        };
        let name = rustls::pki_types::ServerName::try_from(args.tls_name.clone())
            .map_err(|e| format!("--tls-name: {e}"))?;
        let upgraded = connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS handshake: {e}"))?;
        io = BufReader::new(Stream::Tls(Box::new(upgraded.into())));
        // RFC 3207 §4.2: everything learned before the handshake is void.
        write(&mut io, "EHLO loadgen.acceptance\r\n").await?;
        read_multiline(&mut io, 250).await?;
    }

    if !args.password.is_empty() {
        // AUTH PLAIN: NUL authzid, NUL-separated (RFC 4616).
        let payload = format!("\0{}\0{}", args.username, args.password);
        let encoded = base64_encode(payload.as_bytes());
        write(&mut io, &format!("AUTH PLAIN {encoded}\r\n")).await?;
        expect(&mut io, 235).await?;
    }

    write(&mut io, &format!("MAIL FROM:<{}>\r\n", args.from)).await?;
    let (code, text) = read_reply(&mut io).await?;
    if code != 250 {
        return Ok((code, text));
    }

    write(&mut io, &format!("RCPT TO:<{recipient}>\r\n")).await?;
    let (code, text) = read_reply(&mut io).await?;
    if code != 250 {
        return Ok((code, text));
    }

    write(&mut io, "DATA\r\n").await?;
    let (code, text) = read_reply(&mut io).await?;
    if code != 354 {
        return Ok((code, text));
    }

    write(&mut io, &message(args, recipient)).await?;
    let (code, text) = read_reply(&mut io).await?;

    // Best effort — the verdict is already in hand.
    let _ = write(&mut io, "QUIT\r\n").await;
    Ok((code, text))
}

/// A client that trusts the CA in `path` and nothing else. A panic rather than
/// a reported error: without the CA there is no run to report on.
fn connector(path: &str) -> tokio_rustls::TlsConnector {
    use rustls::pki_types::pem::PemObject;
    let ca = rustls::pki_types::CertificateDer::from_pem_file(path)
        .unwrap_or_else(|e| panic!("reading --ca {path}: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).expect("--ca is not a usable trust anchor");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// The message body, carrying one of everything the §4.3 assertions look for.
fn message(args: &Args, recipient: &str) -> String {
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
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Your order has shipped.\r\n\
         Track it at https://oldbrand.com/track\r\n\
         .\r\n",
        args.from_header,
        args.subject,
        args.tag,
        recipient.split('@').next().unwrap_or("x"),
    )
}

// ---------------------------------------------------------------------------
// a very small SMTP client
// ---------------------------------------------------------------------------

async fn write(io: &mut BufReader<Stream>, s: &str) -> Result<(), String> {
    io.get_mut()
        .write_all(s.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))
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

/// Read one reply, following multi-line continuations.
async fn read_reply(io: &mut BufReader<Stream>) -> Result<(u16, String), String> {
    loop {
        let line = read_line(io).await?;
        let trimmed = line.trim_end();
        if trimmed.len() < 4 {
            return Err(format!("short reply: {trimmed}"));
        }
        let code: u16 = trimmed[..3]
            .parse()
            .map_err(|_| format!("unparseable reply: {trimmed}"))?;
        // `250-` continues, `250 ` ends.
        if trimmed.as_bytes()[3] == b'-' {
            continue;
        }
        return Ok((code, trimmed[4..].to_string()));
    }
}

async fn read_multiline(io: &mut BufReader<Stream>, want: u16) -> Result<(), String> {
    let (code, text) = read_reply(io).await?;
    if code != want {
        return Err(format!("expected {want}, got {code} {text}"));
    }
    Ok(())
}

async fn expect(io: &mut BufReader<Stream>, want: u16) -> Result<(), String> {
    read_multiline(io, want).await
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
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}
