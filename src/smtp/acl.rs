//! §5.3's sender ACL (D-071): which sender identities an authenticated user may
//! present.
//!
//! Modelled on Slater's ACL — users keyed by name, per-resource capability
//! lists, **default deny** — with one capability, `send_as`, whose patterns are
//! §5.4's grammar reused verbatim through [`Pattern`]. Operators already know it,
//! it is already thoroughly tested, and a second grammar for the same kind of
//! thing would be a second grammar to get wrong.
//!
//! ## Acceptance, never routing
//!
//! §5.3 says the authenticated username "plays no part in route selection", and
//! this module keeps that true. It answers one question — *may this user present
//! this identity?* — and the answer only ever refuses a message. It never feeds
//! the router: a message the ACL admits is routed by §5.4 exactly as it would be
//! without an ACL, so two users permitted the same identity produce
//! byte-identical output. Letting a grant pick a chain would make the outbound
//! identity depend on *who authenticated*, which application-side configuration
//! cannot express, and that breaks §1.1 outright rather than merely §5.3.

use std::collections::BTreeMap;

use crate::config::Auth;
use crate::routing::sender_match::Pattern;

/// The compiled ACL. Patterns are parsed once, at startup.
#[derive(Debug, Clone, Default)]
pub struct Acl {
    send_as: BTreeMap<String, Vec<Pattern>>,
}

impl Acl {
    pub fn new(auth: &Auth) -> Self {
        Self {
            send_as: auth
                .users
                .iter()
                .map(|u| {
                    (
                        u.username.clone(),
                        u.grants.send_as.iter().map(|p| Pattern::parse(p)).collect(),
                    )
                })
                .collect(),
        }
    }

    /// May `user` present `address` as a sender?
    ///
    /// Default deny: an unknown user, or an address matching none of the user's
    /// patterns, is refused. `address` is a bare `local@domain`, as §5.4's
    /// matcher expects.
    pub fn permits(&self, user: &str, address: &str) -> bool {
        self.send_as
            .get(user)
            .is_some_and(|patterns| patterns.iter().any(|p| p.matches(address)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Grants, User};

    fn acl(users: &[(&str, &[&str])]) -> Acl {
        Acl::new(&Auth {
            allow_insecure_auth: false,
            mechanisms: vec![],
            users: users
                .iter()
                .map(|(name, grants)| User {
                    username: name.to_string(),
                    password_hash: "$argon2id$unused".to_string(),
                    grants: Grants {
                        send_as: grants.iter().map(|s| s.to_string()).collect(),
                    },
                })
                .collect(),
        })
    }

    #[test]
    fn each_of_the_three_pattern_forms_grants_what_it_says() {
        let a = acl(&[(
            "cfapp",
            &["oldbrand.com", "*.oldbrand.com", "marketing@newbrand.com"],
        )]);
        assert!(a.permits("cfapp", "jane@oldbrand.com"));
        assert!(a.permits("cfapp", "x@mail.oldbrand.com"));
        assert!(a.permits("cfapp", "marketing@newbrand.com"));

        assert!(!a.permits("cfapp", "sales@newbrand.com"));
        assert!(!a.permits("cfapp", "jane@notoldbrand.com"));
        assert!(!a.permits("cfapp", "jane@oldbrand.com.evil"));
    }

    #[test]
    fn matching_is_case_insensitive_as_section_5_4_says() {
        let a = acl(&[("cfapp", &["OldBrand.COM"])]);
        assert!(a.permits("cfapp", "Jane@OLDBRAND.com"));
    }

    #[test]
    fn default_deny_for_an_unknown_user() {
        // An authenticated session always has a configured user, so this is the
        // belt: a username the ACL does not know grants nothing.
        let a = acl(&[("cfapp", &["oldbrand.com"])]);
        assert!(!a.permits("nobody", "jane@oldbrand.com"));
    }

    #[test]
    fn grants_do_not_leak_between_users() {
        let a = acl(&[("cfapp", &["oldbrand.com"]), ("crm", &["newbrand.com"])]);
        assert!(a.permits("cfapp", "a@oldbrand.com"));
        assert!(!a.permits("cfapp", "a@newbrand.com"));
        assert!(a.permits("crm", "a@newbrand.com"));
        assert!(!a.permits("crm", "a@oldbrand.com"));
    }

    #[test]
    fn a_string_that_is_not_an_address_is_refused() {
        let a = acl(&[("cfapp", &["oldbrand.com"])]);
        assert!(!a.permits("cfapp", "oldbrand.com"));
        assert!(!a.permits("cfapp", ""));
    }
}
