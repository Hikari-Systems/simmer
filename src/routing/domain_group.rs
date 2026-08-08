//! §3.2 step 2 — resolve a recipient to a domain group.
//!
//! "Exact, case-insensitive match of the recipient domain against each group's
//! domain list; fall back to the catch-all group."
//!
//! No MX lookup and no heuristics, per §3.1 and §14.3. The consequence is stated
//! in §14.3 and is worth repeating where someone will read it: a Google Workspace
//! custom domain lands in the catch-all rather than being counted against
//! `google`, so a B2B recipient list gets less useful per-provider throttling
//! than a consumer one. The config shape permits adding MX grouping later
//! without a schema change.

use crate::config::{Config, DomainGroup};

/// The group a recipient belongs to.
///
/// Never fails: §4.2 guarantees exactly one group contains `*`, so there is
/// always a fallback. The `Option` is only for a configuration that bypassed
/// validation, which `config::load` makes impossible.
pub fn resolve<'a>(cfg: &'a Config, recipient: &str) -> Option<&'a DomainGroup> {
    let domain = domain_of(recipient);

    if let Some(d) = domain {
        for group in &cfg.domain_groups {
            if group
                .domains
                .iter()
                .any(|configured| configured != "*" && configured.eq_ignore_ascii_case(d))
            {
                return Some(group);
            }
        }
    }

    // No domain (a bare local part like `postmaster`), or no group claims it.
    cfg.catchall_group()
}

/// The domain part of `local@domain`, lowercased by the caller's comparison.
///
/// Uses the **last** `@`: a quoted local part may legally contain one, and
/// `"a@b"@example.com` has domain `example.com`. Same rule as §5.4's sender
/// matching, deliberately.
fn domain_of(address: &str) -> Option<&str> {
    let at = address.rfind('@')?;
    let domain = &address[at + 1..];
    (!domain.is_empty() && at > 0).then_some(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
server:
  listen: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { required: false, allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
domain_groups:
  - { name: google, domains: ["gmail.com", "googlemail.com"] }
  - { name: microsoft, domains: ["outlook.com", "hotmail.co.uk", "live.com"] }
  - { name: catchall, domains: ["*"] }
senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [overflow] }
default_chain: [overflow]
routes:
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    fn config() -> Config {
        crate::config::from_str(CFG, "test").expect("fixture is valid")
    }

    fn group_of(cfg: &Config, recipient: &str) -> String {
        resolve(cfg, recipient)
            .expect("catch-all always exists")
            .name
            .clone()
    }

    #[test]
    fn resolves_a_configured_domain_to_its_group() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@gmail.com"), "google");
        assert_eq!(group_of(&cfg, "bob@googlemail.com"), "google");
        assert_eq!(group_of(&cfg, "bob@outlook.com"), "microsoft");
        assert_eq!(group_of(&cfg, "bob@live.com"), "microsoft");
    }

    #[test]
    fn matching_is_case_insensitive_on_both_sides() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "Bob@GMAIL.COM"), "google");
        assert_eq!(group_of(&cfg, "bob@Hotmail.CO.UK"), "microsoft");
    }

    #[test]
    fn an_unlisted_domain_falls_to_the_catch_all() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@example.com"), "catchall");
        assert_eq!(group_of(&cfg, "bob@yahoo.com"), "catchall");
    }

    #[test]
    fn matching_is_exact_not_by_suffix() {
        // The bug this guards: a suffix match would put `notgmail.com` and
        // `gmail.com.evil.net` in the google bucket, and a warm-up would be
        // steered by an attacker's choice of recipient domain.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@notgmail.com"), "catchall");
        assert_eq!(group_of(&cfg, "bob@gmail.com.evil.net"), "catchall");
        assert_eq!(group_of(&cfg, "bob@mail.gmail.com"), "catchall");
    }

    #[test]
    fn a_literal_star_in_a_recipient_does_not_match_the_catch_all_entry() {
        // `*` is the catch-all marker, not a domain. An address at the literal
        // domain "*" must land in the catch-all by fallback, not by matching its
        // own list entry — otherwise a group whose list is `["*"]` would appear
        // to have a real domain in it.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@*"), "catchall");
    }

    #[test]
    fn first_matching_group_wins() {
        // Config order decides, and §4.2 already rejects a domain listed in more
        // than one group — so this only fixes the behaviour, it does not invite
        // the ambiguity.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@gmail.com"), "google");
    }

    #[test]
    fn an_address_with_no_domain_falls_to_the_catch_all() {
        // RFC 5321 §4.5.1 requires bare `postmaster` to be accepted, and §5.2's
        // parser passes it through. It has to be counted somewhere.
        let cfg = config();
        assert_eq!(group_of(&cfg, "postmaster"), "catchall");
        assert_eq!(group_of(&cfg, "bob@"), "catchall");
        assert_eq!(group_of(&cfg, "@gmail.com"), "catchall");
        assert_eq!(group_of(&cfg, ""), "catchall");
    }

    #[test]
    fn a_quoted_local_part_containing_an_at_uses_the_last_one() {
        let cfg = config();
        assert_eq!(group_of(&cfg, r#""a@b"@gmail.com"#), "google");
    }
}
