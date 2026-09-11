//! §5.1 inbound TLS: the certificate, loaded once at startup (D-070).
//!
//! One chain and one key, PEM, read from disk. No ACME, and no reload — §2.2's
//! "no hot config reload" makes rotation a restart.
//!
//! [`load`] is called twice: by `config::validate`, so that a missing,
//! unreadable, unparseable or mismatched file is a §4.2 violation reported with
//! everything else, and by the listener, which keeps the acceptor. Both go
//! through this one function so the check and the listener cannot disagree about
//! what the file says — the same reason §6.6 is checked with the real engine.
//!
//! Two properties are *warnings* rather than violations, because neither stops
//! the certificate being presented and refusing to start over them would take
//! the plaintext listeners down with it: a certificate that does not cover
//! `server.hostname`, and one that has expired or is about to. See
//! [`advisories`].

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::config::ServerTls;

/// How far ahead of expiry the startup log starts warning. Long enough to cover
/// a fortnight's holiday, which is how certificates usually expire unnoticed.
const EXPIRY_WARNING: chrono::Duration = chrono::Duration::days(14);

/// A problem with `server.tls`, in §4.2's path-and-message form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsProblem {
    pub path: String,
    pub message: String,
}

/// The loaded certificate: an acceptor for the listeners, and what startup
/// logs about the leaf.
#[derive(Clone)]
pub struct Loaded {
    pub acceptor: TlsAcceptor,
    pub leaf: CertificateDer<'static>,
    /// `None` only if the leaf's validity could not be read, which a
    /// certificate rustls accepted should never produce.
    pub not_after: Option<DateTime<Utc>>,
}

/// Read, parse and pair the certificate and key. Reports every problem found,
/// not just the first — both files are read even when the first fails.
pub fn load(cfg: &ServerTls) -> Result<Loaded, Vec<TlsProblem>> {
    let mut problems = Vec::new();

    let certs = match read("server.tls.certificate", &cfg.certificate) {
        Ok(bytes) => match CertificateDer::pem_slice_iter(&bytes).collect::<Result<Vec<_>, _>>() {
            Ok(certs) if certs.is_empty() => {
                problems.push(TlsProblem {
                    path: "server.tls.certificate".into(),
                    message: format!(
                        "'{}' contains no PEM CERTIFICATE block{}",
                        cfg.certificate,
                        if PrivateKeyDer::from_pem_slice(&bytes).is_ok() {
                            " (it holds a private key; are certificate and private_key \
                                 swapped?)"
                        } else {
                            ""
                        }
                    ),
                });
                None
            }
            Ok(certs) => Some(certs),
            Err(e) => {
                problems.push(TlsProblem {
                    path: "server.tls.certificate".into(),
                    message: format!("'{}' is not valid PEM: {e}", cfg.certificate),
                });
                None
            }
        },
        Err(p) => {
            problems.push(p);
            None
        }
    };

    let key = match read("server.tls.private_key", &cfg.private_key) {
        Ok(bytes) => match PrivateKeyDer::from_pem_slice(&bytes) {
            Ok(key) => Some(key),
            Err(e) => {
                problems.push(TlsProblem {
                    path: "server.tls.private_key".into(),
                    message: format!(
                        "'{}' holds no usable private key (PKCS#8, PKCS#1 or SEC1 PEM): {e}",
                        cfg.private_key
                    ),
                });
                None
            }
        },
        Err(p) => {
            problems.push(p);
            None
        }
    };

    let (Some(certs), Some(key)) = (certs, key) else {
        return Err(problems);
    };
    let leaf = certs[0].clone();

    // An explicit provider rather than the process default: `ring`, the one
    // provider in the graph (Cargo.toml's rustls comment), and no dependence on
    // whether something else has installed a default first.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let built = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_no_client_auth().with_single_cert(certs, key));

    match built {
        Ok(config) => Ok(Loaded {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            not_after: not_after(&leaf),
            leaf,
        }),
        Err(e) => {
            // `with_single_cert` compares the key's public half with the leaf's,
            // so a mismatched pair lands here — the commonest real mistake after
            // a rotation that replaced one file and not the other.
            let message = match e {
                rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch) => {
                    format!(
                        "'{}' does not match the first certificate in '{}'. After a \
                         rotation, check both files were replaced",
                        cfg.private_key, cfg.certificate
                    )
                }
                other => format!("the certificate and key cannot be used together: {other}"),
            };
            problems.push(TlsProblem {
                path: "server.tls".into(),
                message,
            });
            Err(problems)
        }
    }
}

/// Read a file, turning a permission failure into a message that names the
/// actual cause. The container runs as UID 1000 (`Dockerfile`), so a key
/// mounted readable by root only is the likeliest failure there is, and the bare
/// `Permission denied (os error 13)` says nothing about which user was denied.
fn read(path: &str, file: &str) -> Result<Vec<u8>, TlsProblem> {
    std::fs::read(file).map_err(|e| TlsProblem {
        path: path.to_string(),
        message: match e.kind() {
            std::io::ErrorKind::PermissionDenied => format!(
                "'{file}' is not readable by this process ({}){}. The container runs as UID \
                 1000; mount the file readable by that user",
                current_uid().map_or_else(|| "uid unknown".to_string(), |u| format!("uid {u}")),
                owner_and_mode(Path::new(file))
                    .map(|(uid, mode)| format!("; the file is owned by uid {uid}, mode {mode:04o}"))
                    .unwrap_or_default(),
            ),
            std::io::ErrorKind::NotFound => format!("'{file}' does not exist"),
            _ => format!("'{file}' cannot be read: {e}"),
        },
    })
}

fn owner_and_mode(file: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(file).ok()?;
    Some((m.uid(), m.mode() & 0o7777))
}

/// The real uid, from procfs. Linux only, which is the only platform the image
/// targets; elsewhere the message just says it does not know.
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Whether the leaf certificate is valid for `hostname`, by `rustls-webpki`'s
/// own name matching — the same code a verifying client would run.
pub fn covers(leaf: &CertificateDer<'_>, hostname: &str) -> bool {
    let Ok(name) = ServerName::try_from(hostname) else {
        return false;
    };
    webpki::EndEntityCert::try_from(leaf)
        .and_then(|cert| cert.verify_is_valid_for_subject_name(&name))
        .is_ok()
}

/// What startup should say about the certificate, worst first. Empty when
/// there is nothing to say beyond the not-after date, which is always logged.
pub fn advisories(loaded: &Loaded, hostname: &str, now: DateTime<Utc>) -> Vec<String> {
    let mut out = Vec::new();
    match loaded.not_after {
        Some(t) if t <= now => out.push(format!(
            "server.tls.certificate EXPIRED at {}; verifying clients will refuse every \
             handshake on the TLS listeners",
            t.to_rfc3339()
        )),
        Some(t) if t - now <= EXPIRY_WARNING => out.push(format!(
            "server.tls.certificate expires at {} ({} days from now); rotation is a restart",
            t.to_rfc3339(),
            (t - now).num_days()
        )),
        Some(_) => {}
        None => out.push(
            "server.tls.certificate's validity period could not be read, so its expiry is \
             not being reported"
                .to_string(),
        ),
    }
    if !covers(&loaded.leaf, hostname) {
        out.push(format!(
            "server.tls.certificate does not cover server.hostname '{hostname}'; a client \
             that verifies the certificate against the name it connected to will refuse it"
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// notAfter, read by hand
// ---------------------------------------------------------------------------

/// The leaf's `notAfter` (RFC 5280 §4.1.2.5).
///
/// Read by hand rather than through `x509-parser`: one field at a fixed
/// structural position does not justify a dependency tree, and rustls has
/// already validated the DER by the time this runs. Anything unexpected is
/// `None`, which [`advisories`] reports rather than guessing.
///
/// ```text
/// Certificate  ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
/// TBSCertificate ::= SEQUENCE {
///     version         [0] EXPLICIT Version DEFAULT v1,
///     serialNumber    INTEGER,
///     signature       AlgorithmIdentifier,   -- SEQUENCE
///     issuer          Name,                  -- SEQUENCE
///     validity        SEQUENCE { notBefore Time, notAfter Time },
///     ... }
/// ```
pub fn not_after(cert: &[u8]) -> Option<DateTime<Utc>> {
    const SEQUENCE: u8 = 0x30;
    const INTEGER: u8 = 0x02;
    const VERSION: u8 = 0xA0;

    let (SEQUENCE, certificate, _) = tlv(cert)? else {
        return None;
    };
    let (SEQUENCE, tbs, _) = tlv(certificate)? else {
        return None;
    };

    let mut rest = tbs;
    if rest.first() == Some(&VERSION) {
        rest = tlv(rest)?.2;
    }
    for expected in [INTEGER, SEQUENCE, SEQUENCE] {
        let (tag, _, next) = tlv(rest)?;
        if tag != expected {
            return None;
        }
        rest = next;
    }
    let (SEQUENCE, validity, _) = tlv(rest)? else {
        return None;
    };
    let (_, _, after_not_before) = tlv(validity)?;
    let (tag, time, _) = tlv(after_not_before)?;
    parse_time(tag, time)
}

/// One DER tag-length-value: `(tag, contents, remainder)`. Definite lengths
/// only, which is all DER permits.
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// RFC 5280 §4.1.2.5: `UTCTime` (`YYMMDDHHMMSSZ`, years 1950–2049) or
/// `GeneralizedTime` (`YYYYMMDDHHMMSSZ`). Both are required to carry seconds
/// and to be in UTC, so nothing else is accepted.
fn parse_time(tag: u8, bytes: &[u8]) -> Option<DateTime<Utc>> {
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;

    let s = std::str::from_utf8(bytes).ok()?.strip_suffix('Z')?;
    let full = match tag {
        UTC_TIME if s.len() == 12 => {
            let yy: u32 = s[..2].parse().ok()?;
            let century = if yy < 50 { "20" } else { "19" };
            format!("{century}{s}")
        }
        GENERALIZED_TIME if s.len() == 14 => s.to_string(),
        _ => return None,
    };
    NaiveDateTime::parse_from_str(&full, "%Y%m%d%H%M%S")
        .ok()
        .map(|t| t.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed certificate for `names`, valid until `not_after`, as
    /// `(certificate PEM, key PEM, certificate DER)`.
    fn self_signed(names: &[&str], not_after: (i32, u8, u8)) -> (String, String, Vec<u8>) {
        let mut params =
            rcgen::CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .expect("params");
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("cert");
        (cert.pem(), key.serialize_pem(), cert.der().to_vec())
    }

    fn write(dir: &tempfile::TempDir, name: &str, contents: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write");
        path.display().to_string()
    }

    fn cfg(certificate: String, private_key: String) -> ServerTls {
        ServerTls {
            certificate,
            private_key,
        }
    }

    // -- notAfter --------------------------------------------------------

    #[test]
    fn reads_not_after_as_utc_time() {
        // rcgen writes UTCTime for years before 2050, as RFC 5280 requires.
        let (_, _, der) = self_signed(&["simmer.test"], (2031, 3, 4));
        assert_eq!(
            not_after(&der).map(|t| t.to_rfc3339()),
            Some("2031-03-04T00:00:00+00:00".to_string())
        );
    }

    #[test]
    fn reads_not_after_as_generalized_time() {
        // And GeneralizedTime from 2050 onward — the other branch.
        let (_, _, der) = self_signed(&["simmer.test"], (2061, 12, 31));
        assert_eq!(
            not_after(&der).map(|t| t.to_rfc3339()),
            Some("2061-12-31T00:00:00+00:00".to_string())
        );
    }

    #[test]
    fn utc_time_pivots_at_1950() {
        assert_eq!(
            parse_time(0x17, b"491231235959Z").map(|t| t.to_rfc3339()),
            Some("2049-12-31T23:59:59+00:00".to_string())
        );
        assert_eq!(
            parse_time(0x17, b"500101000000Z").map(|t| t.to_rfc3339()),
            Some("1950-01-01T00:00:00+00:00".to_string())
        );
    }

    #[test]
    fn times_without_seconds_or_zulu_are_refused() {
        // RFC 5280 forbids both; a reader that tolerated them would be guessing.
        assert_eq!(parse_time(0x17, b"3103040000Z"), None);
        assert_eq!(parse_time(0x17, b"310304000000"), None);
        assert_eq!(parse_time(0x18, b"20310304000000+0100"), None);
        assert_eq!(parse_time(0x04, b"310304000000Z"), None);
    }

    #[test]
    fn garbage_is_none_and_never_panics() {
        assert_eq!(not_after(b""), None);
        assert_eq!(not_after(b"\x30"), None);
        assert_eq!(not_after(b"\x30\x84\xff\xff\xff\xff"), None);
        assert_eq!(not_after(b"\x30\x03\x02\x01\x00"), None);
        let (_, _, der) = self_signed(&["simmer.test"], (2031, 3, 4));
        for cut in 0..der.len() {
            let _ = not_after(&der[..cut]);
        }
    }

    // -- name coverage ---------------------------------------------------

    #[test]
    fn covers_its_own_names_and_wildcards_but_nothing_else() {
        let (_, _, der) = self_signed(&["simmer.internal", "*.mail.example"], (2040, 1, 1));
        let leaf = CertificateDer::from(der);
        assert!(covers(&leaf, "simmer.internal"));
        assert!(covers(&leaf, "smtp.mail.example"));
        assert!(!covers(&leaf, "mail.example"));
        assert!(!covers(&leaf, "other.internal"));
        assert!(!covers(&leaf, "not a hostname"));
    }

    // -- load ------------------------------------------------------------

    #[test]
    fn loads_a_matching_pair() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, _) = self_signed(&["simmer.test"], (2040, 1, 1));
        let loaded = load(&cfg(
            write(&dir, "c.pem", &cert),
            write(&dir, "k.pem", &key),
        ))
        .unwrap_or_else(|p| panic!("{p:?}"));
        assert!(loaded.not_after.is_some());
    }

    #[test]
    fn a_mismatched_key_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _, _) = self_signed(&["simmer.test"], (2040, 1, 1));
        let (_, other_key, _) = self_signed(&["simmer.test"], (2040, 1, 1));
        let Err(problems) = load(&cfg(
            write(&dir, "c.pem", &cert),
            write(&dir, "k.pem", &other_key),
        )) else {
            panic!("a mismatched pair loaded");
        };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.contains("does not match"),
            "{problems:?}"
        );
    }

    #[test]
    fn both_missing_files_are_reported_together() {
        // §4.2's "report all violations": a rotation that got both paths wrong
        // should need one restart to discover, not two.
        let Err(problems) = load(&cfg(
            "/nonexistent/c.pem".into(),
            "/nonexistent/k.pem".into(),
        )) else {
            panic!("loaded from nowhere");
        };
        let paths: Vec<_> = problems.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            paths,
            ["server.tls.certificate", "server.tls.private_key"],
            "{problems:?}"
        );
        assert!(problems
            .iter()
            .all(|p| p.message.contains("does not exist")));
    }

    #[test]
    fn swapped_files_are_diagnosed() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, _) = self_signed(&["simmer.test"], (2040, 1, 1));
        let Err(problems) = load(&cfg(
            write(&dir, "k.pem", &key),
            write(&dir, "c.pem", &cert),
        )) else {
            panic!("swapped files loaded");
        };
        assert!(
            problems.iter().any(|p| p.message.contains("swapped")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unreadable_key_names_the_uid_problem() {
        use std::os::unix::fs::PermissionsExt;

        // Root reads anything, so the condition cannot be produced as root.
        if current_uid() == Some(0) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, _) = self_signed(&["simmer.test"], (2040, 1, 1));
        let key_path = write(&dir, "k.pem", &key);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let Err(problems) = load(&cfg(write(&dir, "c.pem", &cert), key_path)) else {
            panic!("an unreadable key loaded");
        };
        let msg = &problems[0].message;
        assert!(msg.contains("not readable by this process"), "{msg}");
        assert!(msg.contains("UID 1000"), "{msg}");
        assert!(msg.contains("mode 0000"), "{msg}");
    }

    // -- advisories ------------------------------------------------------

    fn loaded(names: &[&str], not_after: (i32, u8, u8)) -> Loaded {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, _) = self_signed(names, not_after);
        load(&cfg(
            write(&dir, "c.pem", &cert),
            write(&dir, "k.pem", &key),
        ))
        .unwrap()
    }

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn a_healthy_certificate_has_nothing_to_say() {
        let l = loaded(&["simmer.internal"], (2040, 1, 1));
        assert!(advisories(&l, "simmer.internal", at(2030, 1, 1)).is_empty());
    }

    #[test]
    fn expiry_is_warned_about_before_and_after_it_happens() {
        let l = loaded(&["simmer.internal"], (2030, 1, 10));
        let soon = advisories(&l, "simmer.internal", at(2030, 1, 1));
        assert!(soon.len() == 1 && soon[0].contains("9 days"), "{soon:?}");
        let late = advisories(&l, "simmer.internal", at(2030, 2, 1));
        assert!(late.len() == 1 && late[0].contains("EXPIRED"), "{late:?}");
    }

    #[test]
    fn a_certificate_for_another_name_is_warned_about() {
        let l = loaded(&["other.internal"], (2040, 1, 1));
        let a = advisories(&l, "simmer.internal", at(2030, 1, 1));
        assert!(
            a.len() == 1 && a[0].contains("does not cover server.hostname"),
            "{a:?}"
        );
    }
}
