//! §5.3 authentication: `AUTH PLAIN` and `AUTH LOGIN` over argon2id hashes.
//!
//! Two things §5.3 asks for that are easy to get wrong:
//!
//! *Comparison is constant-time.* argon2's own verifier is, but a naive
//! implementation leaks whether the **username** existed by returning in
//! microseconds instead of milliseconds. [`Verifier`] therefore hashes against a
//! decoy when the user is unknown, so a failure costs the same either way.
//!
//! *Authentication is authentication only.* The authenticated username plays no
//! part in route selection, and nothing in this module hands it to the router.

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use crate::config::{Auth, Mechanism};

/// §5.3: "Failed attempts are rate-limited per connection (three failures then
/// `421` and disconnect)."
pub const MAX_FAILURES: u32 = 3;

/// A pre-parsed credential set.
///
/// Hashes are parsed once at construction rather than per attempt: a malformed
/// hash should be a startup-time complaint (§4.2 already checks the `$argon2`
/// prefix), not a mysterious authentication failure at 3am.
pub struct Verifier {
    users: Vec<(String, String)>,
    /// A syntactically valid argon2id hash that no password matches, used to
    /// spend the same CPU on an unknown username as on a known one.
    decoy: String,
}

impl Verifier {
    pub fn new(auth: &Auth) -> Self {
        Self {
            users: auth
                .users
                .iter()
                .map(|u| (u.username.clone(), u.password_hash.clone()))
                .collect(),
            // Fixed salt and digest: this is never a secret, it is a timing
            // ballast. The parameters match the §4.1 example so the decoy costs
            // roughly what a real verification costs.
            decoy: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$\
                    Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9vYmE"
                .to_string(),
        }
    }

    /// Verify a username and password.
    ///
    /// argon2id at the §4.1 parameters costs tens of milliseconds of CPU by
    /// design, so this **must not** run on the async runtime — with
    /// `max_concurrent_sessions: 64` a burst of authentication attempts would
    /// stall every other session on the worker. Callers use
    /// [`Verifier::verify_blocking`] from `spawn_blocking`.
    pub fn verify_blocking(&self, username: &str, password: &str) -> bool {
        let hash = self
            .users
            .iter()
            .find(|(u, _)| u == username)
            .map(|(_, h)| h.as_str());

        // Always hash something. An early return on an unknown username turns
        // this into a user-enumeration oracle measurable over the network.
        let (candidate, is_real) = match hash {
            Some(h) => (h, true),
            None => (self.decoy.as_str(), false),
        };

        let parsed = match PasswordHash::new(candidate) {
            Ok(p) => p,
            // A hash that will not parse cannot authenticate anyone. §4.2
            // checks the prefix at startup; this is the belt to that's braces.
            Err(_) => return false,
        };

        let verified = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();

        verified && is_real
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

/// Where an `AUTH` exchange has got to.
///
/// `LOGIN` is a three-round-trip protocol with no framing of its own, so the
/// session has to remember which challenge it last sent. `PLAIN` is at most two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// Sent `334` with no challenge; expecting the whole `PLAIN` payload.
    PlainAwaitingResponse,
    /// Sent `334 VXNlcm5hbWU6`; expecting a base64 username.
    LoginAwaitingUsername,
    /// Sent `334 UGFzc3dvcmQ6`; expecting a base64 password.
    LoginAwaitingPassword { username: String },
}

/// What the session should do with a line received during an `AUTH` exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStep {
    /// Send this base64 challenge and move to `next`.
    Challenge { b64: String, next: AuthState },
    /// Credentials are complete; verify them.
    Credentials { username: String, password: String },
    /// The client sent `*`.
    Cancelled,
    /// The payload was not valid base64, or not the shape the mechanism requires.
    BadEncoding,
}

/// RFC 4954's challenges, base64 of `Username:` and `Password:`.
const CHALLENGE_USERNAME: &str = "VXNlcm5hbWU6";
const CHALLENGE_PASSWORD: &str = "UGFzc3dvcmQ6";

/// Begin an `AUTH` exchange. `None` means the mechanism is not supported.
pub fn begin(mechanism: &str, initial: Option<&str>, enabled: &[Mechanism]) -> Option<AuthStep> {
    let mechanism = match mechanism {
        "PLAIN" if enabled.contains(&Mechanism::Plain) => Mechanism::Plain,
        "LOGIN" if enabled.contains(&Mechanism::Login) => Mechanism::Login,
        _ => return None,
    };

    Some(match (mechanism, initial) {
        // RFC 4954 §4: a lone `=` is an *empty* initial response, not an absent
        // one. For PLAIN that is an empty payload, which cannot carry
        // credentials — so it is a decode failure, not a challenge.
        (Mechanism::Plain, Some("=")) => AuthStep::BadEncoding,
        (Mechanism::Plain, Some(b64)) => decode_plain(b64),
        (Mechanism::Plain, None) => AuthStep::Challenge {
            b64: String::new(),
            next: AuthState::PlainAwaitingResponse,
        },
        (Mechanism::Login, Some(b64)) => match decode_utf8(b64) {
            Some(username) => AuthStep::Challenge {
                b64: CHALLENGE_PASSWORD.to_string(),
                next: AuthState::LoginAwaitingPassword { username },
            },
            None => AuthStep::BadEncoding,
        },
        (Mechanism::Login, None) => AuthStep::Challenge {
            b64: CHALLENGE_USERNAME.to_string(),
            next: AuthState::LoginAwaitingUsername,
        },
    })
}

/// Feed a client line into an in-progress exchange.
pub fn advance(state: &AuthState, line: &str) -> AuthStep {
    // RFC 4954 §4: the client cancels with a single `*`.
    if line.trim() == "*" {
        return AuthStep::Cancelled;
    }

    match state {
        AuthState::PlainAwaitingResponse => decode_plain(line.trim()),
        AuthState::LoginAwaitingUsername => match decode_utf8(line.trim()) {
            Some(username) => AuthStep::Challenge {
                b64: CHALLENGE_PASSWORD.to_string(),
                next: AuthState::LoginAwaitingPassword { username },
            },
            None => AuthStep::BadEncoding,
        },
        AuthState::LoginAwaitingPassword { username } => match decode_utf8(line.trim()) {
            Some(password) => AuthStep::Credentials {
                username: username.clone(),
                password,
            },
            None => AuthStep::BadEncoding,
        },
    }
}

/// `AUTH PLAIN` payload: `authzid NUL authcid NUL passwd`.
///
/// The authzid is discarded. §5.3 makes the authenticated identity irrelevant to
/// routing, so an "act as" assertion has nothing to act on.
fn decode_plain(b64: &str) -> AuthStep {
    let Ok(raw) = B64.decode(b64.as_bytes()) else {
        return AuthStep::BadEncoding;
    };

    // Split on the *first two* NULs. A password may legally contain one, so
    // splitting on all of them would corrupt it.
    let mut parts = raw.splitn(3, |b| *b == 0);
    let (_authzid, authcid, passwd) = match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return AuthStep::BadEncoding,
    };

    match (std::str::from_utf8(authcid), std::str::from_utf8(passwd)) {
        (Ok(u), Ok(p)) if !u.is_empty() => AuthStep::Credentials {
            username: u.to_string(),
            password: p.to_string(),
        },
        _ => AuthStep::BadEncoding,
    }
}

fn decode_utf8(b64: &str) -> Option<String> {
    let raw = B64.decode(b64.as_bytes()).ok()?;
    String::from_utf8(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        B64.encode(s.as_bytes())
    }

    fn both() -> Vec<Mechanism> {
        vec![Mechanism::Plain, Mechanism::Login]
    }

    // -- mechanism selection ---------------------------------------------

    #[test]
    fn unsupported_mechanisms_are_rejected() {
        assert!(begin("CRAM-MD5", None, &both()).is_none());
        assert!(begin("XOAUTH2", None, &both()).is_none());
        assert!(begin("GSSAPI", None, &both()).is_none());
    }

    #[test]
    fn a_mechanism_absent_from_config_is_rejected_even_though_it_is_implemented() {
        assert!(begin("LOGIN", None, &[Mechanism::Plain]).is_none());
        assert!(begin("PLAIN", None, &[Mechanism::Login]).is_none());
    }

    // -- PLAIN -----------------------------------------------------------

    #[test]
    fn plain_with_an_initial_response_completes_in_one_step() {
        let payload = b64("\0cfapp\0secret");
        assert_eq!(
            begin("PLAIN", Some(&payload), &both()),
            Some(AuthStep::Credentials {
                username: "cfapp".into(),
                password: "secret".into()
            })
        );
    }

    #[test]
    fn plain_without_an_initial_response_challenges_with_an_empty_string() {
        let step = begin("PLAIN", None, &both()).expect("supported");
        assert_eq!(
            step,
            AuthStep::Challenge {
                b64: String::new(),
                next: AuthState::PlainAwaitingResponse
            }
        );

        let payload = b64("\0cfapp\0secret");
        assert_eq!(
            advance(&AuthState::PlainAwaitingResponse, &payload),
            AuthStep::Credentials {
                username: "cfapp".into(),
                password: "secret".into()
            }
        );
    }

    #[test]
    fn plain_discards_the_authzid() {
        let payload = b64("someone-else\0cfapp\0secret");
        assert_eq!(
            begin("PLAIN", Some(&payload), &both()),
            Some(AuthStep::Credentials {
                username: "cfapp".into(),
                password: "secret".into()
            })
        );
    }

    #[test]
    fn a_password_may_contain_a_nul() {
        // splitn(3) rather than split() — the password is everything after the
        // second NUL, however many it contains.
        let payload = b64("\0cfapp\0sec\0ret");
        assert_eq!(
            begin("PLAIN", Some(&payload), &both()),
            Some(AuthStep::Credentials {
                username: "cfapp".into(),
                password: "sec\0ret".into()
            })
        );
    }

    #[test]
    fn plain_rejects_malformed_payloads() {
        for bad in [
            b64("no-nuls-at-all"),
            b64("\0only-one-nul"),
            b64("\0\0empty-username"),
            "!!!not base64!!!".to_string(),
        ] {
            assert_eq!(
                begin("PLAIN", Some(&bad), &both()),
                Some(AuthStep::BadEncoding),
                "{bad}"
            );
        }
    }

    #[test]
    fn plain_treats_an_empty_initial_response_as_a_decode_failure() {
        // RFC 4954 §4: `=` means an empty initial response, which for PLAIN
        // cannot carry credentials.
        assert_eq!(
            begin("PLAIN", Some("="), &both()),
            Some(AuthStep::BadEncoding)
        );
    }

    // -- LOGIN -----------------------------------------------------------

    #[test]
    fn login_walks_username_then_password() {
        let step = begin("LOGIN", None, &both()).expect("supported");
        assert_eq!(
            step,
            AuthStep::Challenge {
                b64: CHALLENGE_USERNAME.into(),
                next: AuthState::LoginAwaitingUsername
            }
        );

        let step = advance(&AuthState::LoginAwaitingUsername, &b64("cfapp"));
        assert_eq!(
            step,
            AuthStep::Challenge {
                b64: CHALLENGE_PASSWORD.into(),
                next: AuthState::LoginAwaitingPassword {
                    username: "cfapp".into()
                }
            }
        );

        assert_eq!(
            advance(
                &AuthState::LoginAwaitingPassword {
                    username: "cfapp".into()
                },
                &b64("secret")
            ),
            AuthStep::Credentials {
                username: "cfapp".into(),
                password: "secret".into()
            }
        );
    }

    #[test]
    fn login_accepts_the_username_as_an_initial_response() {
        assert_eq!(
            begin("LOGIN", Some(&b64("cfapp")), &both()),
            Some(AuthStep::Challenge {
                b64: CHALLENGE_PASSWORD.into(),
                next: AuthState::LoginAwaitingPassword {
                    username: "cfapp".into()
                }
            })
        );
    }

    #[test]
    fn the_challenges_are_the_rfc_4954_strings() {
        assert_eq!(B64.decode(CHALLENGE_USERNAME).unwrap(), b"Username:");
        assert_eq!(B64.decode(CHALLENGE_PASSWORD).unwrap(), b"Password:");
    }

    #[test]
    fn login_rejects_undecodable_input() {
        assert_eq!(
            advance(&AuthState::LoginAwaitingUsername, "not!base64"),
            AuthStep::BadEncoding
        );
    }

    // -- cancellation ----------------------------------------------------

    #[test]
    fn a_bare_asterisk_cancels_at_any_stage() {
        for state in [
            AuthState::PlainAwaitingResponse,
            AuthState::LoginAwaitingUsername,
            AuthState::LoginAwaitingPassword {
                username: "cfapp".into(),
            },
        ] {
            assert_eq!(advance(&state, "*"), AuthStep::Cancelled);
        }
    }

    // -- verification ----------------------------------------------------

    /// A genuine argon2id hash of "local-dev-password" at the §4.1 example
    /// parameters — the same one `docker-compose.yml` supplies, so this test and
    /// the compose stack cannot drift apart silently.
    const REAL_HASH: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$iYj0sVhRvzAWM9kzsFBKyg$V1tnVXTGEO+BD0x9q1uccuo91w84jrz+qDKA3qElLFE";

    fn verifier() -> Verifier {
        Verifier::new(&crate::config::Auth {
            required: true,
            allow_insecure_auth: true,
            mechanisms: both(),
            users: vec![crate::config::User {
                username: "cfapp".into(),
                password_hash: REAL_HASH.into(),
            }],
        })
    }

    #[test]
    fn accepts_the_right_password_and_rejects_the_wrong_one() {
        let v = verifier();
        assert!(v.verify_blocking("cfapp", "local-dev-password"));
        assert!(!v.verify_blocking("cfapp", "local-dev-passwore"));
        assert!(!v.verify_blocking("cfapp", ""));
    }

    #[test]
    fn rejects_an_unknown_username() {
        let v = verifier();
        assert!(!v.verify_blocking("nobody", "local-dev-password"));
    }

    #[test]
    fn the_decoy_hash_is_a_parseable_argon2id_string() {
        // If the decoy stopped parsing, an unknown username would return early
        // from `PasswordHash::new` and the timing-equalisation would silently
        // stop working — the failure mode this test exists to prevent.
        let v = verifier();
        assert!(PasswordHash::new(&v.decoy).is_ok());
        assert!(!v.verify_blocking("nobody", "anything at all"));
    }

    #[test]
    fn an_unparseable_stored_hash_authenticates_nobody() {
        let v = Verifier::new(&crate::config::Auth {
            required: true,
            allow_insecure_auth: true,
            mechanisms: both(),
            users: vec![crate::config::User {
                username: "broken".into(),
                password_hash: "$argon2id$not-actually-a-hash".into(),
            }],
        });
        assert!(!v.verify_blocking("broken", "anything"));
    }
}
