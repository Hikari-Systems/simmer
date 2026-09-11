//! §8.2 outbound TLS: the four modes, and a stream that can be either.
//!
//! | Mode | Behaviour |
//! |---|---|
//! | `off` | Plaintext; never issues `STARTTLS` |
//! | `opportunistic` | `STARTTLS` if advertised, continue in plaintext if not or if it fails |
//! | `required` | `STARTTLS` mandatory; certificate not validated |
//! | `required_verify` | `STARTTLS` mandatory with full chain and hostname validation |
//!
//! `rustls` with the platform root store, per §8.2.
//!
//! [`Stream`] is also the inbound session's stream since D-070 (§5.1): the
//! `STARTTLS` upgrade is the same move-out-and-back from either end, so there is
//! one type with one `Taken` state rather than two.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

use crate::config::TlsMode;

/// An SMTP connection, before or after `STARTTLS` — downstream (§8.2) or
/// inbound (§5.1). `TlsStream` is tokio-rustls's client-or-server enum, so the
/// one variant serves both ends.
///
/// An enum rather than `Box<dyn AsyncRead + AsyncWrite>`: the conversation code
/// is written once against this type, and upgrading in place is a `mem::replace`
/// rather than a re-plumb.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    /// Held for the few instructions of a `STARTTLS` upgrade, while the socket
    /// has been moved out to the TLS layer and the encrypted replacement has not
    /// yet been moved in.
    ///
    /// This variant exists so that the upgrade is a total function. The obvious
    /// alternative — synthesising a throwaway socket to `mem::replace` with —
    /// can fail, and a failure there would panic inside a session holding a live
    /// client connection. Reading or writing a `Taken` stream is a bug, and it
    /// reports itself as one rather than by aborting the process.
    Taken,
}

impl Stream {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Stream::Tls(_))
    }
}

fn taken() -> std::io::Error {
    std::io::Error::other("stream used during a STARTTLS upgrade")
}

// Delegating the three traits by hand is tedious but keeps the enum concrete.
macro_rules! delegate {
    ($self:ident, $inner:ident => $call:expr) => {
        match std::pin::Pin::into_inner($self) {
            Stream::Plain($inner) => {
                let $inner = std::pin::Pin::new($inner);
                $call
            }
            Stream::Tls($inner) => {
                let $inner = std::pin::Pin::new($inner);
                $call
            }
            Stream::Taken => std::task::Poll::Ready(Err(taken())),
        }
    };
}

impl AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => s.poll_read(cx, buf))
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        delegate!(self, s => s.poll_write(cx, buf))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => s.poll_flush(cx))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => s.poll_shutdown(cx))
    }
}

/// TLS configuration, built once at startup and shared by every route.
///
/// Loading the platform root store is a filesystem walk, and building a
/// `ClientConfig` derives key schedules; doing either per message would put both
/// on the latency path of every relay.
#[derive(Clone)]
pub struct TlsConfigs {
    verifying: Arc<ClientConfig>,
    /// For `TlsMode::Required` — encrypted but unauthenticated, which §8.2
    /// permits for a self-signed internal Postal.
    non_verifying: Arc<ClientConfig>,
}

impl TlsConfigs {
    /// Build both configurations, loading the platform root store.
    ///
    /// Returns the number of roots loaded so startup can log it: zero roots
    /// means every `required_verify` route will fail, and discovering that from
    /// a `451` storm rather than a startup line is a bad afternoon.
    pub fn load() -> anyhow::Result<(Self, usize)> {
        // The process-wide crypto provider. Installed here rather than in main
        // so that any entry point which builds TLS gets it, and ignoring the
        // error is correct — it fails only if one is already installed, which is
        // the outcome we wanted.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut roots = RootCertStore::empty();
        let loaded = rustls_native_certs::load_native_certs();
        for cert in loaded.certs {
            // Individual malformed certificates in a system bundle are common
            // and not fatal; the count is what matters.
            let _ = roots.add(cert);
        }
        let count = roots.len();

        let verifying = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let non_verifying = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification))
            .with_no_client_auth();

        Ok((
            Self {
                verifying: Arc::new(verifying),
                non_verifying: Arc::new(non_verifying),
            },
            count,
        ))
    }

    fn connector(&self, mode: TlsMode) -> tokio_rustls::TlsConnector {
        let cfg = match mode {
            TlsMode::RequiredVerify => Arc::clone(&self.verifying),
            // `opportunistic` does not validate either: it already tolerates no
            // encryption at all, so demanding a valid chain when encryption
            // happens to be offered would make it *stricter* than plaintext,
            // which inverts the mode's meaning.
            TlsMode::Required | TlsMode::Opportunistic => Arc::clone(&self.non_verifying),
            TlsMode::Off => Arc::clone(&self.non_verifying),
        };
        tokio_rustls::TlsConnector::from(cfg)
    }

    /// Perform the TLS handshake over an already-`STARTTLS`ed stream.
    pub async fn upgrade(
        &self,
        mode: TlsMode,
        hostname: &str,
        tcp: TcpStream,
    ) -> Result<Stream, String> {
        let server_name = ServerName::try_from(hostname.to_string())
            .map_err(|_| format!("'{hostname}' is not a valid TLS server name"))?;

        let stream = self
            .connector(mode)
            .connect(server_name, tcp)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Stream::Tls(Box::new(stream.into())))
    }
}

/// §8.2 `required`: "certificate not validated (self-signed internal Postal)".
///
/// The traffic is encrypted against a passive observer and unauthenticated
/// against an active one. That is a deliberate, named mode in the spec for a
/// downstream on a trusted segment — it is not a default, and `required_verify`
/// is.
#[derive(Debug)]
struct NoVerification;

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_platform_root_store() {
        let (_cfg, count) = TlsConfigs::load().expect("TLS config builds");
        // The runtime image installs ca-certificates precisely so this is
        // nonzero. A zero here on a developer machine is a missing
        // ca-certificates package, not a code fault — hence the message.
        assert!(
            count > 0,
            "no platform root certificates found; required_verify routes cannot work"
        );
    }

    #[test]
    fn building_tls_configs_is_idempotent() {
        // `install_default` fails on the second call; `load` must tolerate it,
        // or the second route to be dialled would break.
        assert!(TlsConfigs::load().is_ok());
        assert!(TlsConfigs::load().is_ok());
    }
}
