//! §5.3 authentication: `AUTH PLAIN` and `AUTH LOGIN` over argon2id hashes.
//!
//! Two things §5.3 asks for that are easy to get wrong:
//!
//! *Comparison is constant-time.* argon2's own verifier is, but a naive
//! implementation leaks whether the **username** existed by returning in
//! microseconds instead of milliseconds. [`Verifier`] therefore hashes against a
//! decoy when the user is unknown, so a failure costs the same either way.
//!
//! The decoy is **derived from the credentials the configuration actually
//! holds**, and that is not a detail. Phase 2 minted it at the §4.1 example's
//! parameters and left a comment saying it cost "roughly" what a real
//! verification costs — but argon2 verification is parameter-agnostic:
//! `PasswordHash::new` reads `m`, `t` and `p` from the *stored* string and
//! re-derives at those. An operator who minted their hashes at anything else
//! therefore had two paths of visibly different cost and a username-enumeration
//! oracle back, with the mitigation still apparently in place. Borrowing the
//! costliest parameters in the ACL makes the unknown-user path run the very
//! derivation a real login runs: exact when parameters are uniform, and wrong in
//! the safe direction — an unknown user costing *more* than a known one — when
//! they are not. See `DECISIONS.md` D-066.
//!
//! *Authentication is authentication only.* The authenticated username plays no
//! part in route selection, and nothing in this module hands it to the router.

use std::sync::Arc;

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

/// D-079 — how many argon2 verifications may run at once, and the permits that
/// hold to it.
///
/// Each verification holds `m_cost` (19 MiB at §4.1's example parameters) for as
/// long as it runs, and `spawn_blocking` will start a thread per waiting session.
/// Unbounded, the peak is therefore whatever the race between thread creation and
/// hash duration happens to allow — measured at about 32 in flight, 608 MiB, on a
/// synchronised burst of 64 logins, with nothing in the design holding it there.
/// This makes the ceiling a property of the configuration instead.
#[derive(Clone)]
pub struct VerifyLimit {
    permits: Arc<tokio::sync::Semaphore>,
    max: usize,
}

impl VerifyLimit {
    pub fn new(max: usize) -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(max)),
            max,
        }
    }

    /// Wait for a permit, and report how many verifications are now in flight.
    ///
    /// Waiting costs a burst of clients some latency, bounded by the command
    /// timeout; the alternative is unbounded memory.
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the verification semaphore is never closed");
        crate::metrics::auth_verifies_in_flight(self.permits.available_permits(), self.max);
        permit
    }
}

/// The decoy used when the configuration holds no usable hash to take parameters
/// from — an empty ACL, or one whose every entry fails to parse.
///
/// Never a secret: it is timing ballast, and the digest is deliberately not the
/// hash of anything. The parameters are §4.1's example, which is the best guess
/// available when there is nothing to copy.
const FALLBACK_DECOY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzYWx0$\
                              Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9vYmE";

impl Verifier {
    pub fn new(auth: &Auth) -> Self {
        let users: Vec<(String, String)> = auth
            .users
            .iter()
            .map(|u| (u.username.clone(), u.password_hash.clone()))
            .collect();

        Self {
            decoy: decoy_for(&users),
            users,
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

/// Build the decoy from the costliest hash the ACL holds.
///
/// "Costliest" is `m_cost × t_cost`: argon2 computes that many memory blocks
/// whatever `p` is, and `p` divides them between lanes rather than reducing the
/// total. So this is the work factor a verification pays, and taking the maximum
/// means every real verification costs *at most* what the unknown-user path does.
fn decoy_for(users: &[(String, String)]) -> String {
    users
        .iter()
        .filter_map(|(_, h)| PasswordHash::new(h).ok())
        .max_by_key(work_factor)
        .and_then(|h| blind(&h))
        .unwrap_or_else(|| FALLBACK_DECOY.to_string())
}

fn work_factor(hash: &PasswordHash) -> u64 {
    let param = |name: &str| u64::from(hash.params.get_decimal(name).unwrap_or(0));
    param("m").saturating_mul(param("t").max(1))
}

/// The same algorithm, version, parameters, salt and digest length — with a
/// digest that is not the hash of anything.
///
/// Keeping the *salt* matters as much as the parameters: argon2's cost does not
/// vary with salt content, but copying it means the decoy is byte-for-byte the
/// same work, and reconstructing one would be a way to get the length subtly
/// wrong. Replacing only the digest is what stops a real credential hash being
/// held in a second place in memory, and makes "no password matches it" true by
/// construction rather than by luck.
fn blind(hash: &PasswordHash) -> Option<String> {
    let digest = hash.hash?;
    let blinded = argon2::password_hash::Output::new(&vec![0u8; digest.len()]).ok()?;
    Some(
        PasswordHash {
            algorithm: hash.algorithm,
            version: hash.version,
            params: hash.params.clone(),
            salt: hash.salt,
            hash: Some(blinded),
        }
        .to_string(),
    )
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

    /// D-079 — the bound is a bound: past `max`, a verification waits for a
    /// permit rather than starting and holding another `m_cost` of memory.
    ///
    /// The sizing rule itself (`max(4, 2 × cores)`) is deliberately not asserted:
    /// a test of that arithmetic would only compare it with itself.
    #[tokio::test]
    async fn a_verification_past_the_bound_waits_for_a_permit() {
        let limit = VerifyLimit::new(2);
        let first = limit.acquire().await;
        let _second = limit.acquire().await;

        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(50), limit.acquire()).await;
        assert!(
            blocked.is_err(),
            "a third verification started while both permits were held"
        );

        // And a released permit is handed straight on.
        drop(first);
        let third =
            tokio::time::timeout(std::time::Duration::from_millis(500), limit.acquire()).await;
        assert!(third.is_ok(), "releasing a permit did not admit a waiter");
    }

    /// Grants play no part in verification; any will do.
    fn grants() -> crate::config::Grants {
        crate::config::Grants {
            send_as: vec!["oldbrand.com".into()],
        }
    }

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
            allow_insecure_auth: true,
            mechanisms: both(),
            users: vec![crate::config::User {
                username: "cfapp".into(),
                password_hash: REAL_HASH.into(),
                grants: grants(),
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

    // -- the decoy's cost, which is the defect D-066 fixed ------------------

    /// Mint a real argon2id hash at chosen parameters. Cheap ones, deliberately:
    /// these tests are about which parameters end up in the decoy, not about
    /// spending 19 MiB of CPU to find out.
    fn hash_at(password: &str, m: u32, t: u32) -> String {
        use argon2::password_hash::{PasswordHasher, SaltString};
        use argon2::{Algorithm, Params, Version};

        let argon = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(m, t, 1, None).expect("valid parameters"),
        );
        let salt = SaltString::encode_b64(b"sixteen-byte-slt").expect("valid salt");
        argon
            .hash_password(password.as_bytes(), &salt)
            .expect("hashes")
            .to_string()
    }

    fn verifier_over(users: &[(&str, String)]) -> Verifier {
        Verifier::new(&crate::config::Auth {
            allow_insecure_auth: true,
            mechanisms: both(),
            users: users
                .iter()
                .map(|(u, h)| crate::config::User {
                    username: (*u).into(),
                    password_hash: h.clone(),
                    grants: grants(),
                })
                .collect(),
        })
    }

    fn params_of(phc: &str) -> (u32, u32) {
        let parsed = PasswordHash::new(phc).expect("parses");
        (
            parsed.params.get_decimal("m").expect("m"),
            parsed.params.get_decimal("t").expect("t"),
        )
    }

    #[test]
    fn the_decoy_takes_its_parameters_from_the_configured_hash() {
        // The defect in phase 2's code: the decoy was minted at m=19456,t=2 no
        // matter what the ACL held, and verification re-derives at whatever the
        // *stored* string says. An operator minting at anything else had a known
        // username and an unknown one costing measurably different amounts.
        let v = verifier_over(&[("cfapp", hash_at("pw", 64, 3))]);
        assert_eq!(
            params_of(&v.decoy),
            (64, 3),
            "the unknown-user path must run the derivation a real login runs"
        );
    }

    #[test]
    fn the_decoy_takes_the_costliest_of_several() {
        // Degrade in the safe direction: with mixed parameters, an unknown user
        // costs at least what any known one costs, never less. The other way
        // round is the oracle.
        let v = verifier_over(&[
            ("cheap", hash_at("pw", 32, 1)),
            ("dear", hash_at("pw", 128, 2)),
            ("middling", hash_at("pw", 64, 2)),
        ]);
        assert_eq!(params_of(&v.decoy), (128, 2));
    }

    #[test]
    fn the_decoy_is_not_a_copy_of_anybodys_hash() {
        // It borrows the *cost*, never the credential. If the digest came across
        // intact, a decoy would be a second copy of a real hash — and the claim
        // that no password matches it would hold only by luck.
        let real = hash_at("pw", 64, 2);
        let v = verifier_over(&[("cfapp", real.clone())]);

        assert_ne!(v.decoy, real);
        assert!(
            !v.verify_blocking("nobody", "pw"),
            "the right password under the wrong username must still fail"
        );
        assert!(
            v.verify_blocking("cfapp", "pw"),
            "and the real credential must still work"
        );
    }

    #[test]
    fn an_acl_with_nothing_usable_falls_back_rather_than_losing_the_ballast() {
        // An empty ACL — §5.3 permits `required: false` with no users — and one
        // whose entries do not parse both have no parameters to copy. Falling
        // back keeps the unknown-user path expensive; failing to would make it
        // free, which is the oracle again.
        for users in [
            vec![],
            vec![("broken", "$argon2id$not-actually-a-hash".to_string())],
        ] {
            let v = verifier_over(&users);
            assert_eq!(params_of(&v.decoy), (19456, 2));
            assert!(!v.verify_blocking("nobody", "anything"));
        }
    }

    #[test]
    fn an_unparseable_stored_hash_authenticates_nobody() {
        let v = Verifier::new(&crate::config::Auth {
            allow_insecure_auth: true,
            mechanisms: both(),
            users: vec![crate::config::User {
                username: "broken".into(),
                password_hash: "$argon2id$not-actually-a-hash".into(),
                grants: grants(),
            }],
        });
        assert!(!v.verify_blocking("broken", "anything"));
    }
}
