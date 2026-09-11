//! §7.3 — the recipient-frequency constraint: normalisation, the salted hash,
//! and the rolling window.
//!
//! ```text
//! An optional per-route constraint. Over threshold makes the route
//! **ineligible**, so the message falls through to the next link — it is a
//! steering rule, not a suppression rule. Nothing is ever dropped by it.
//! ```
//!
//! That sentence governs everything here. Every failure mode in this module
//! resolves to "this route is not eligible", never to "this message is refused":
//! a chain whose every link is over threshold ends at §10.3's `451`, exactly as
//! an exhausted quota does, and §14.1 is why it must.
//!
//! ## What is stored
//!
//! §7.3: "The stored key is a **salted hash** of the normalised value, never
//! plaintext. The salt is generated once and persisted. This bounds row size and
//! avoids the container accumulating a plaintext record of every address mailed,
//! which is a data-protection liability with no operational benefit."
//!
//! So the plaintext address exists in this process only as long as the message
//! does, and reaches neither the database nor a log line nor a metric label.
//! [`Key`] is deliberately not `Display`.
//!
//! ## Where the check sits
//!
//! In the chain walk, at §3.2 step 3b's position — above the quota check, which
//! §3.2 calls out ("Evaluated **first** — it can eliminate routes outright") —
//! but *outside* the transaction that takes the quota row lock (D-049). It is an
//! unlocked read of a different table.

pub mod sweeper;

use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::{Config, FrequencyMode, RecipientFrequency};

/// The salted hash of one normalised recipient, as stored.
///
/// Truncated to 16 bytes of HMAC-SHA256. §7.3 asks the key to "bound row size";
/// 128 bits is far beyond what a collision would need to be unlikely, and a
/// collision costs one message steered to the next link rather than anything
/// permanent. Untruncated would be twice the index for no gain.
///
/// No `Display`, no `Debug` output of the address it came from: the whole reason
/// §7.3 hashes is that the plaintext must not accumulate anywhere.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 16]);

impl Key {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Enough to correlate two keys in a test failure, not enough to be a
        // record of anything.
        write!(f, "Key({:02x}{:02x}…)", self.0[0], self.0[1])
    }
}

/// The persisted salt, and the keying it does.
///
/// §7.3: "The salt is generated once and persisted." Once per *instance*, not
/// once per process — a restart that minted a new one would silently reset every
/// window, and a second replica minting its own would give the two of them
/// different views of the same recipient. `instance_config` holds it and the
/// insert is idempotent (D-050).
pub struct Keyer {
    salt: Vec<u8>,
}

impl Keyer {
    pub fn new(salt: Vec<u8>) -> Self {
        Self { salt }
    }

    /// Hash one already-normalised value.
    pub fn key(&self, normalised: &str) -> Key {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.salt)
            // HMAC accepts a key of any length; this cannot fail.
            .expect("HMAC accepts any key length");
        mac.update(normalised.as_bytes());
        let full = mac.finalize().into_bytes();

        let mut out = [0u8; 16];
        out.copy_from_slice(&full[..16]);
        Key(out)
    }

    /// Normalise and hash in one step, which is the only way the relay uses it.
    pub fn key_for(&self, address: &str, mode: FrequencyMode, dot_insensitive: &[String]) -> Key {
        self.key(&normalise(address, mode, dot_insensitive))
    }
}

/// The instance's [`Keyer`], resolved from storage on first use and then held.
///
/// Lazy rather than loaded at startup, because §7.5 is explicit that an
/// unreachable database is not a startup failure — "reply `451 4.3.0 quota
/// service unavailable` and send nothing" while the listener stays up. Resolving
/// the salt in `main` would turn a database that is merely late into a process
/// that will not start. Resolving it on the message path makes a failure the same
/// `451` every other storage failure produces, and the first message after the
/// database returns picks it up.
///
/// A route with no `recipient_frequency` never asks for it, so a deployment that
/// configures no constraint never touches the table.
#[derive(Default)]
pub struct Frequency {
    keyer: tokio::sync::OnceCell<Keyer>,
}

impl Frequency {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin a known salt. For tests, and for `main`'s best-effort startup warm.
    pub fn with_salt(salt: Vec<u8>) -> Self {
        let keyer = tokio::sync::OnceCell::new();
        let _ = keyer.set(Keyer::new(salt));
        Self { keyer }
    }

    /// The keyer, minting and persisting a salt if this is a fresh instance.
    pub async fn keyer(
        &self,
        store: &dyn crate::quota::store::QuotaStore,
    ) -> Result<&Keyer, crate::quota::store::QuotaError> {
        self.keyer
            .get_or_try_init(|| async { Ok(Keyer::new(store.recipient_hash_salt().await?)) })
            .await
    }
}

/// A fresh salt, as bytes. 32 of them.
///
/// From two v4 UUIDs rather than by taking a dependency on `rand`: each carries
/// 122 bits of the operating system's randomness through `getrandom`, and `uuid`
/// is already a direct dependency for §9.5's correlation id. 244 bits is a great
/// deal more than an HMAC key needs.
pub fn generate_salt() -> Vec<u8> {
    let mut salt = Vec::with_capacity(32);
    salt.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    salt.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    salt
}

/// §7.3's normalisation, as a pure function.
///
/// > Normalisation for `to_address`: lowercase, strip everything from `+` to `@`
/// > in the local part, and remove dots from the local part when the domain is a
/// > known dot-insensitive provider (configurable list, defaulting to the
/// > `google` group's domains). `Bob.Smith+news@gmail.com` and
/// > `bobsmith@gmail.com` are the same inbox and a determined recipient will
/// > complain about both.
///
/// `mode: to_domain` keys on the domain alone.
///
/// The list of dot-insensitive domains is configuration rather than the `google`
/// group's membership (D-010): the spec's default couples address normalisation
/// to a group *name* that a deployment is free to rename or not have.
pub fn normalise(address: &str, mode: FrequencyMode, dot_insensitive: &[String]) -> String {
    let lowered = address.trim().to_lowercase();

    // `rsplit_once` rather than `split_once`: an unquoted local part may not
    // contain `@`, but a quoted one may, and the last `@` is the separator in
    // both cases.
    let Some((local, domain)) = lowered.rsplit_once('@') else {
        // A domainless recipient — `RCPT TO:<postmaster>` is legal SMTP. There is
        // no domain to key on and nothing to normalise, so both modes key on the
        // whole string. Its own bucket, which is the safe answer: it groups only
        // with itself.
        return lowered;
    };

    if mode == FrequencyMode::ToDomain {
        return domain.to_string();
    }

    // A quoted local part is opaque: the characters inside it are literal, so
    // `"a.b"@x.com` and `"ab"@x.com` are genuinely different mailboxes and a `+`
    // inside the quotes is part of the address rather than a tag. Neither
    // transformation is safe, so neither is applied.
    if local.starts_with('"') {
        return lowered;
    }

    // "strip everything from `+` to `@`" — the first `+`, so that a tag
    // containing its own `+` is removed entirely.
    let local = match local.split_once('+') {
        Some((before, _tag)) => before,
        None => local,
    };

    let local = if dot_insensitive
        .iter()
        .any(|d| d.eq_ignore_ascii_case(domain))
    {
        local.replace('.', "")
    } else {
        local.to_string()
    };

    format!("{local}@{domain}")
}

/// The start of §7.3's rolling window: everything at or after this instant counts.
///
/// > Window is `count × unit` (`hourly`, `daily`, `weekly`) evaluated as a
/// > **rolling** window, so per-event timestamps are stored rather than a
/// > counter.
///
/// Rolling, so the boundary moves with `now` — there is no bucket that empties
/// all at once, which is the property that makes a threshold of 3/day mean "three
/// in any 24 hours" rather than "three since midnight".
pub fn window_start(constraint: &RecipientFrequency, now: DateTime<Utc>) -> DateTime<Utc> {
    // The `unwrap_or` is only reachable past ~292 million years of window, which
    // §4.2's `count >= 1` does not bound. Saturating is better than a panic on
    // the message path, and a window that long counts everything either way.
    now - chrono::Duration::from_std(constraint.window.as_duration())
        .unwrap_or(chrono::Duration::MAX)
}

/// How long `recipient_event` rows are worth keeping, or `None` when no route
/// declares a constraint and none are ever written.
///
/// > A sweeper evicts rows older than the longest configured window plus a
/// > margin, on an interval.
///
/// The margin is proportional rather than fixed, so an hourly window is not kept
/// for a day and a weekly window keeps more than an hour of slack. It exists so
/// that a row is never evicted while it could still be inside somebody's window —
/// the sweep and the count race otherwise, and the sweep would win by deleting a
/// row the count needed.
pub fn retention(cfg: &Config) -> Option<Duration> {
    let longest = cfg
        .routes
        .iter()
        .filter_map(|r| r.recipient_frequency.as_ref())
        .map(|f| f.window.as_duration())
        .max()?;

    Some(longest + longest / 10)
}

/// Does any route declare a constraint? If not, nothing writes `recipient_event`
/// and the sweeper is not started at all.
pub fn any_configured(cfg: &Config) -> bool {
    cfg.routes.iter().any(|r| r.recipient_frequency.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Window, WindowUnit};

    fn google() -> Vec<String> {
        vec!["gmail.com".to_string(), "googlemail.com".to_string()]
    }

    fn address(a: &str) -> String {
        normalise(a, FrequencyMode::ToAddress, &google())
    }

    fn domain(a: &str) -> String {
        normalise(a, FrequencyMode::ToDomain, &google())
    }

    // -- §7.3 normalisation, to_address ------------------------------------

    #[test]
    fn the_headline_case_from_the_spec() {
        // §7.3 names both of these and says they are the same inbox.
        assert_eq!(address("Bob.Smith+news@gmail.com"), "bobsmith@gmail.com");
        assert_eq!(address("bobsmith@gmail.com"), "bobsmith@gmail.com");
    }

    #[test]
    fn case_is_folded_across_the_whole_address() {
        assert_eq!(address("BOB@EXAMPLE.COM"), "bob@example.com");
        assert_eq!(address("Bob@Example.Com"), "bob@example.com");
    }

    #[test]
    fn a_plus_tag_is_stripped_whatever_the_domain() {
        // §7.3 puts the `+` rule before the dot rule and does not condition it on
        // the provider: sub-addressing is near-universal, and where it is not
        // supported the tagged form does not deliver anyway.
        assert_eq!(address("bob+news@example.com"), "bob@example.com");
        assert_eq!(address("bob+news@gmail.com"), "bob@gmail.com");
    }

    #[test]
    fn everything_after_the_first_plus_goes() {
        // "strip everything from + to @" — including a second `+`.
        assert_eq!(address("bob+a+b@example.com"), "bob@example.com");
    }

    #[test]
    fn a_bare_plus_leaves_an_empty_local_part_rather_than_failing() {
        // Degenerate, and it still has to produce a key rather than an error:
        // §7.3 steers, and a steering rule that can fail would have to decide
        // what failing means.
        assert_eq!(address("+news@example.com"), "@example.com");
    }

    #[test]
    fn dots_go_only_for_a_dot_insensitive_provider() {
        // The distinction that matters: at gmail.com these are one inbox, and
        // almost everywhere else they are two different people.
        assert_eq!(address("bob.smith@gmail.com"), "bobsmith@gmail.com");
        assert_eq!(
            address("bob.smith@googlemail.com"),
            "bobsmith@googlemail.com"
        );
        assert_eq!(address("bob.smith@example.com"), "bob.smith@example.com");
    }

    #[test]
    fn the_dot_insensitive_list_is_matched_case_insensitively() {
        assert_eq!(address("bob.smith@GMail.COM"), "bobsmith@gmail.com");
    }

    #[test]
    fn an_empty_dot_insensitive_list_keeps_every_dot() {
        // A deployment that clears the list gets no dot folding at all — D-010's
        // point is that this is configuration, not a hardcoded provider table.
        assert_eq!(
            normalise("bob.smith@gmail.com", FrequencyMode::ToAddress, &[]),
            "bob.smith@gmail.com"
        );
    }

    #[test]
    fn dots_in_the_domain_are_never_touched() {
        // The rule is about the *local part*. Folding the domain would collapse
        // gmail.com and gmailcom, and worse, mail.example.com and mailexample.com.
        assert_eq!(address("bob@mail.gmail.com"), "bob@mail.gmail.com");
    }

    #[test]
    fn a_quoted_local_part_is_left_alone() {
        // Inside quotes the characters are literal: `"a.b"` and `"ab"` are
        // different mailboxes, and a `+` is part of the address rather than a tag.
        assert_eq!(
            address("\"bob.smith\"@gmail.com"),
            "\"bob.smith\"@gmail.com"
        );
        assert_eq!(
            address("\"bob+news\"@example.com"),
            "\"bob+news\"@example.com"
        );
    }

    #[test]
    fn the_last_at_separates_local_from_domain() {
        // A quoted local part may contain `@`.
        assert_eq!(address("\"a@b\"@example.com"), "\"a@b\"@example.com");
        assert_eq!(domain("\"a@b\"@example.com"), "example.com");
    }

    #[test]
    fn a_domainless_recipient_keys_on_itself() {
        // `RCPT TO:<postmaster>` is legal SMTP. There is no domain to key on, so
        // both modes fall back to the whole string rather than to an empty key
        // that every malformed address would share.
        assert_eq!(address("Postmaster"), "postmaster");
        assert_eq!(domain("Postmaster"), "postmaster");
    }

    #[test]
    fn surrounding_whitespace_does_not_make_a_second_bucket() {
        assert_eq!(address("  bob@example.com  "), "bob@example.com");
    }

    // -- §7.3 normalisation, to_domain -------------------------------------

    #[test]
    fn to_domain_keys_on_the_domain_alone() {
        assert_eq!(domain("bob@example.com"), "example.com");
        assert_eq!(domain("BOB+tag@Example.COM"), "example.com");
        // ...so every recipient at one provider shares a bucket, which is the
        // point of the mode.
        assert_eq!(domain("alice@example.com"), domain("bob@example.com"));
    }

    #[test]
    fn the_two_modes_do_not_share_keys() {
        // A route switched from one mode to the other must not inherit the
        // other's counts: `example.com` and `bob@example.com` are different
        // strings and therefore different hashes.
        assert_ne!(address("bob@example.com"), domain("bob@example.com"));
    }

    // -- the salted hash ---------------------------------------------------

    fn keyer() -> Keyer {
        Keyer::new(b"a fixed salt for tests".to_vec())
    }

    #[test]
    fn the_same_address_under_the_same_salt_gives_the_same_key() {
        assert_eq!(
            keyer().key("bob@example.com"),
            keyer().key("bob@example.com")
        );
    }

    #[test]
    fn different_addresses_give_different_keys() {
        assert_ne!(
            keyer().key("bob@example.com"),
            keyer().key("alice@example.com")
        );
    }

    #[test]
    fn a_different_salt_gives_a_different_key() {
        // Which is why the salt has to survive a restart: minting a new one
        // silently resets every window rather than failing visibly.
        let a = Keyer::new(b"salt one".to_vec());
        let b = Keyer::new(b"salt two".to_vec());
        assert_ne!(a.key("bob@example.com"), b.key("bob@example.com"));
    }

    #[test]
    fn the_key_does_not_contain_the_address() {
        // §7.3's actual requirement: no plaintext record of who was mailed. The
        // check is deliberately crude — any substring of the address appearing in
        // the key bytes would be a catastrophe worth catching.
        let k = keyer().key("bob@example.com");
        let bytes = k.as_bytes();
        for needle in ["bob", "example", "example.com", "bob@example.com"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "'{needle}' appears in the key"
            );
        }
    }

    #[test]
    fn the_debug_form_does_not_leak_the_address_either() {
        // A `Key` reaches a log line only through a test failure, and even there
        // it must not be a record of an address.
        let k = keyer().key("bob@example.com");
        let shown = format!("{k:?}");
        assert!(!shown.contains("bob"), "{shown}");
        assert!(!shown.contains("example"), "{shown}");
    }

    #[test]
    fn a_key_is_sixteen_bytes() {
        assert_eq!(keyer().key("bob@example.com").as_bytes().len(), 16);
    }

    #[test]
    fn key_for_normalises_before_hashing() {
        // The composition the relay uses: the two spellings §7.3 names as one
        // inbox must produce one key.
        let k = keyer();
        assert_eq!(
            k.key_for(
                "Bob.Smith+news@gmail.com",
                FrequencyMode::ToAddress,
                &google()
            ),
            k.key_for("bobsmith@gmail.com", FrequencyMode::ToAddress, &google()),
        );
    }

    #[test]
    fn a_generated_salt_is_thirty_two_bytes_and_not_the_same_twice() {
        let a = generate_salt();
        let b = generate_salt();
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b);
    }

    // -- the rolling window ------------------------------------------------

    fn constraint(unit: WindowUnit, count: u32) -> RecipientFrequency {
        RecipientFrequency {
            mode: FrequencyMode::ToAddress,
            window: Window { unit, count },
            threshold: 3,
        }
    }

    #[test]
    fn the_window_start_is_now_minus_count_times_unit() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            window_start(&constraint(WindowUnit::Hourly, 1), now).to_rfc3339(),
            "2026-08-10T11:00:00+00:00"
        );
        assert_eq!(
            window_start(&constraint(WindowUnit::Daily, 1), now).to_rfc3339(),
            "2026-08-09T12:00:00+00:00"
        );
        assert_eq!(
            window_start(&constraint(WindowUnit::Weekly, 1), now).to_rfc3339(),
            "2026-08-03T12:00:00+00:00"
        );
    }

    #[test]
    fn the_count_multiplies_the_unit() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            window_start(&constraint(WindowUnit::Hourly, 6), now).to_rfc3339(),
            "2026-08-10T06:00:00+00:00"
        );
        assert_eq!(
            window_start(&constraint(WindowUnit::Daily, 3), now).to_rfc3339(),
            "2026-08-07T12:00:00+00:00"
        );
    }

    #[test]
    fn the_window_rolls_with_now_rather_than_snapping_to_a_boundary() {
        // The property that distinguishes a rolling window from a bucket: two
        // instants an hour apart have starts an hour apart, with no shared edge
        // where a bucket would have emptied.
        let c = constraint(WindowUnit::Daily, 1);
        let noon = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let one = "2026-08-10T13:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            window_start(&c, one) - window_start(&c, noon),
            chrono::Duration::hours(1)
        );
    }

    #[test]
    fn a_daily_window_ignores_dst_and_calendar_length() {
        // Same reasoning as §7.2's day index: elapsed duration, never calendar
        // arithmetic. 24 hours before 01:30 on a spring-forward morning is 01:30
        // the previous day in UTC, whatever the local clock did.
        let c = constraint(WindowUnit::Daily, 1);
        let now = "2026-03-29T01:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            window_start(&c, now).to_rfc3339(),
            "2026-03-28T01:30:00+00:00"
        );
    }

    // -- retention ---------------------------------------------------------

    const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 1
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
domain_groups:
  - { name: catchall, domains: ["*"] }
senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
default_chain: [overflow]
routes:
  - name: warming
    downstream:
      host: w.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@newbrand.com" }
    warmup: { started: "2026-08-01T09:00:00Z", schedule: { default: [50] } }
FREQUENCY
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
FREQUENCY2
"#;

    /// A window block, indented to sit under a route.
    fn freq(unit: &str, count: u32) -> String {
        format!(
            "    recipient_frequency:\n      mode: to_address\n      \
             window: {{ unit: {unit}, count: {count} }}\n      threshold: 3"
        )
    }

    fn config(warming: &str, overflow: &str) -> Config {
        let yaml = CFG
            .replace("FREQUENCY2", overflow)
            .replace("FREQUENCY", warming);
        crate::config::from_str(&yaml, "test").expect("fixture is valid")
    }

    #[test]
    fn no_configured_constraint_means_no_retention_and_no_sweeper() {
        let cfg = config("", "");
        assert!(!any_configured(&cfg));
        assert_eq!(retention(&cfg), None);
    }

    #[test]
    fn retention_is_the_configured_window_plus_a_margin() {
        let cfg = config(&freq("daily", 1), "");
        assert!(any_configured(&cfg));
        // 24h + 10%.
        assert_eq!(retention(&cfg), Some(Duration::from_secs(86_400 + 8_640)));
    }

    #[test]
    fn retention_takes_the_longest_window_across_every_route() {
        // A route with a shorter window must not shorten the retention: evicting
        // a row that is still inside somebody's window is the one thing the
        // sweeper must never do.
        let cfg = config(&freq("hourly", 1), &freq("weekly", 2));
        assert_eq!(
            retention(&cfg),
            Some(Duration::from_secs(1_209_600 + 120_960))
        );
    }
}
