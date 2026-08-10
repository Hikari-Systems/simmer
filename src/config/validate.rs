//! `SPEC.md` §4.2 startup validation.
//!
//! > The process must refuse to start if any of the following hold. Report
//! > **all** violations, not just the first.
//!
//! That last sentence is the design constraint: nothing in here short-circuits.
//! A misconfigured chain and a bad regex and three unresolvable secrets should
//! all be visible from one failed start, because the alternative is a
//! fix-one-restart-discover-the-next loop against a service that takes minutes
//! to build.
//!
//! The §6.6 rewrite-stability rules are checked by running the real engine:
//! `check_identity` compiles the route's templates and hands them to
//! `rewrite::stability::probe`, which composes `rewrite()` with itself. Deriving
//! the property from a second, validation-only implementation would mean the
//! startup check and the relay could disagree, and the check exists precisely
//! because that disagreement is invisible in production.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;

use super::{Config, Identity, Route};
use crate::rewrite::{stability, RouteRewrite};

/// The identity fields of §6.6. A stability violation in one of these is fatal
/// with no override, and naming one in `unstable_headers` is itself a violation.
pub const IDENTITY_HEADERS: [&str; 3] = ["From", "Sender", "Message-ID"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Where in the document, in a form the reader can search for.
    pub path: String,
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViolationList(pub Vec<Violation>);

impl ViolationList {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(Violation {
            path: path.into(),
            message: message.into(),
        });
    }

    /// Test helper: does any violation mention this substring?
    pub fn mentions(&self, needle: &str) -> bool {
        self.0
            .iter()
            .any(|v| v.message.contains(needle) || v.path.contains(needle))
    }
}

impl fmt::Display for ViolationList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} configuration violation{}:",
            self.0.len(),
            if self.0.len() == 1 { "" } else { "s" }
        )?;
        for v in &self.0 {
            write!(f, "\n  - {v}")?;
        }
        Ok(())
    }
}

/// A condition worth surfacing that is not fatal (§6.6, §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub path: String,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

// ---------------------------------------------------------------------------

pub fn validate(cfg: &Config) -> ViolationList {
    let mut v = ViolationList::default();

    check_listeners(cfg, &mut v);
    check_cidrs(cfg, &mut v);
    check_auth(cfg, &mut v);
    check_domain_groups(cfg, &mut v);
    check_route_uniqueness(cfg, &mut v);
    check_routes(cfg, &mut v);
    check_chains(cfg, &mut v);
    check_default_chain(cfg, &mut v);

    v
}

/// Non-fatal conditions, logged at `WARN` at startup.
pub fn warnings(cfg: &Config) -> Vec<Warning> {
    let mut out = Vec::new();

    for route in &cfg.routes {
        // §6.6: "Startup logs a WARN naming each declared header and the route."
        for header in &route.identity.unstable_headers {
            if !is_identity_header(header) {
                out.push(Warning {
                    path: format!("routes.{}.identity.unstable_headers", route.name),
                    message: format!(
                        "'{header}' is declared migration-only: its value will change when the \
                         application is reconfigured, and it is not part of the target state"
                    ),
                });
            }
        }

        // §7.2: "warmup.started in the future makes the route ineligible until
        // it arrives; log at startup."
        if let Some(w) = &route.warmup {
            if w.started > chrono::Utc::now() {
                out.push(Warning {
                    path: format!("routes.{}.warmup.started", route.name),
                    message: format!(
                        "is in the future ({}); the route is ineligible until then",
                        w.started.to_rfc3339()
                    ),
                });
            }
        }

        // Everything below needs the templates parsed. A route whose templates do
        // not compile has already failed `validate()`, so there is nothing here
        // worth reporting about it.
        let Ok(compiled) = RouteRewrite::compile(&route.identity) else {
            continue;
        };

        // §6.3: "A template referencing recipient.* in a configuration where
        // single_recipient_only: false is a startup validation warning, since it
        // forces per-recipient splitting."
        if !cfg.server.single_recipient_only {
            for (name, template) in &compiled.set_headers {
                if template.references_recipient() {
                    out.push(Warning {
                        path: format!("routes.{}.identity.set_headers.{name}", route.name),
                        message: "references recipient.*, which forces per-recipient splitting \
                                  when single_recipient_only is false"
                            .to_string(),
                    });
                }
            }
            if compiled.envelope_from.references_recipient() {
                out.push(Warning {
                    path: format!("routes.{}.identity.envelope_from", route.name),
                    message: "references recipient.*, which forces per-recipient splitting \
                              when single_recipient_only is false"
                        .to_string(),
                });
            }
        }

        // §6.6: "Naming a header that is in fact stable is also a startup WARN —
        // it means either the declaration is stale or the intent was
        // misunderstood, and both are worth surfacing."
        for header in stability::probe(&compiled).declared_but_stable {
            out.push(Warning {
                path: format!("routes.{}.identity.unstable_headers", route.name),
                message: format!(
                    "declares '{header}' migration-only, but nothing this route does makes it \
                     unstable. Either the declaration is stale or the intent was misunderstood \
                     (§6.6)"
                ),
            });
        }
    }

    // §14.2: an unmatched sender goes to the overflow route at full volume. If
    // that is not what the operator wants, strict_senders exists.
    if !cfg.strict_senders {
        out.push(Warning {
            path: "strict_senders".to_string(),
            message: "is false, so an unmatched sender routes to the default chain's overflow \
                      route at full volume. Alert on simmer_unmatched_sender_total or set \
                      strict_senders: true"
                .to_string(),
        });
    }

    out
}

fn is_identity_header(name: &str) -> bool {
    IDENTITY_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
        // envelope_from is an identity *field* but not a header; naming it in a
        // header list is a category error either way.
        || name.eq_ignore_ascii_case("envelope_from")
}

// ---------------------------------------------------------------------------
// individual rules
// ---------------------------------------------------------------------------

fn check_listeners(cfg: &Config, v: &mut ViolationList) {
    if cfg.server.listen.parse::<SocketAddr>().is_err() {
        v.push(
            "server.listen",
            format!("'{}' is not a valid host:port address", cfg.server.listen),
        );
    }
    if cfg.admin.listen.parse::<SocketAddr>().is_err() {
        v.push(
            "admin.listen",
            format!("'{}' is not a valid host:port address", cfg.admin.listen),
        );
    }
    if cfg.server.max_concurrent_sessions == 0 {
        v.push("server.max_concurrent_sessions", "must be at least 1");
    }
    if cfg.server.max_recipients == 0 {
        v.push("server.max_recipients", "must be at least 1");
    }
    if cfg.server.max_message_bytes == 0 {
        v.push("server.max_message_bytes", "must be greater than zero");
    }
    if cfg.server.hostname.trim().is_empty() {
        v.push(
            "server.hostname",
            "must not be empty; it appears in the EHLO banner and Received headers",
        );
    }
}

fn check_cidrs(cfg: &Config, v: &mut ViolationList) {
    // §5.1. An empty list would refuse every connection, which is a
    // configuration mistake rather than a deliberate lockout.
    if cfg.server.allowed_cidrs.is_empty() {
        v.push(
            "server.allowed_cidrs",
            "is empty, so every connection would be refused",
        );
    }
    for (i, cidr) in cfg.server.allowed_cidrs.iter().enumerate() {
        if cidr.parse::<ipnet::IpNet>().is_err() {
            v.push(
                format!("server.allowed_cidrs[{i}]"),
                format!("'{cidr}' is not a valid CIDR block"),
            );
        }
    }
}

fn check_auth(cfg: &Config, v: &mut ViolationList) {
    let auth = &cfg.server.auth;

    // §4.2: "auth.required: true with an empty user list."
    if auth.required && auth.users.is_empty() {
        v.push(
            "server.auth.users",
            "is empty but auth.required is true, so no client could ever authenticate",
        );
    }

    // §4.2: "allow_insecure_auth is false (there is no inbound TLS, so AUTH
    // would be unusable)."
    if auth.required && !auth.allow_insecure_auth {
        v.push(
            "server.auth.allow_insecure_auth",
            "must be explicitly true: there is no inbound TLS (§5.1), so AUTH is only \
             usable over plaintext and this has to be an acknowledged choice",
        );
    }

    if auth.required && auth.mechanisms.is_empty() {
        v.push(
            "server.auth.mechanisms",
            "is empty but auth.required is true",
        );
    }

    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, user) in auth.users.iter().enumerate() {
        if user.username.is_empty() {
            v.push(
                format!("server.auth.users[{i}].username"),
                "must not be empty",
            );
        }
        if user.password_hash.is_empty() {
            v.push(
                format!("server.auth.users[{i}].password_hash"),
                "must not be empty",
            );
        } else if !user.password_hash.starts_with("$argon2") {
            // §5.3 says argon2id. Catching this at startup beats discovering it
            // when the first client fails to authenticate.
            v.push(
                format!("server.auth.users[{i}].password_hash"),
                "is not an argon2 PHC string (expected it to begin '$argon2')",
            );
        }
        if let Some(prev) = seen.insert(&user.username, i) {
            v.push(
                format!("server.auth.users[{i}].username"),
                format!("'{}' duplicates users[{prev}]", user.username),
            );
        }
    }
}

fn check_domain_groups(cfg: &Config, v: &mut ViolationList) {
    if cfg.domain_groups.is_empty() {
        v.push("domain_groups", "must contain at least the catch-all group");
        return;
    }

    // §4.2: "No domain group contains `*`, or more than one does."
    let catchalls: Vec<&str> = cfg
        .domain_groups
        .iter()
        .filter(|g| g.is_catchall())
        .map(|g| g.name.as_str())
        .collect();
    match catchalls.len() {
        0 => v.push(
            "domain_groups",
            "no group contains '*'; exactly one catch-all group is required",
        ),
        1 => {}
        _ => v.push(
            "domain_groups",
            format!(
                "{} groups contain '*' ({}); exactly one catch-all group is required",
                catchalls.len(),
                catchalls.join(", ")
            ),
        ),
    }

    // Group names must be unique — the quota key and the schedule overrides both
    // address groups by name.
    let mut seen_names: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, g) in cfg.domain_groups.iter().enumerate() {
        if g.name.is_empty() {
            v.push(format!("domain_groups[{i}].name"), "must not be empty");
        }
        if let Some(prev) = seen_names.insert(&g.name, i) {
            v.push(
                format!("domain_groups[{i}].name"),
                format!("'{}' duplicates domain_groups[{prev}]", g.name),
            );
        }
        if g.domains.is_empty() {
            v.push(format!("domain_groups[{i}].domains"), "must not be empty");
        }
    }

    // §4.2: "A domain appears in more than one group." Case-insensitive, since
    // §3.2 step 2 matches case-insensitively.
    let mut seen_domains: BTreeMap<String, &str> = BTreeMap::new();
    for g in &cfg.domain_groups {
        for domain in &g.domains {
            let key = domain.to_ascii_lowercase();
            match seen_domains.get(&key) {
                Some(first) if *first != g.name.as_str() => v.push(
                    format!("domain_groups.{}.domains", g.name),
                    format!("'{domain}' also appears in group '{first}'"),
                ),
                Some(_) => v.push(
                    format!("domain_groups.{}.domains", g.name),
                    format!("'{domain}' is listed twice in the same group"),
                ),
                None => {
                    seen_domains.insert(key, &g.name);
                }
            }
        }
    }
}

fn check_route_uniqueness(cfg: &Config, v: &mut ViolationList) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, r) in cfg.routes.iter().enumerate() {
        if r.name.is_empty() {
            v.push(format!("routes[{i}].name"), "must not be empty");
        }
        if let Some(prev) = seen.insert(&r.name, i) {
            v.push(
                format!("routes[{i}].name"),
                format!("'{}' duplicates routes[{prev}]", r.name),
            );
        }
    }
}

fn check_routes(cfg: &Config, v: &mut ViolationList) {
    for route in &cfg.routes {
        let at = |suffix: &str| format!("routes.{}.{suffix}", route.name);

        // §4.2: "An overflow route carries a warmup block, or a non-overflow
        // route omits one."
        match (route.overflow, &route.warmup) {
            (true, Some(_)) => v.push(
                at("warmup"),
                "an overflow route must not carry a warm-up schedule: it is never quota-limited",
            ),
            (false, None) => v.push(
                at("warmup"),
                "a non-overflow route must carry a warm-up schedule",
            ),
            _ => {}
        }

        if let Some(warmup) = &route.warmup {
            check_schedule(cfg, route, warmup, v);
        }

        check_identity(&route.identity, &route.name, v);

        if route.downstream.host.trim().is_empty() {
            v.push(at("downstream.host"), "must not be empty");
        }
        if route.downstream.port == 0 {
            v.push(at("downstream.port"), "must not be zero");
        }
        if route.downstream.pool.max_connections == 0 {
            v.push(
                at("downstream.pool.max_connections"),
                "must be at least 1, or the route can never send",
            );
        }
        if route.downstream.pool.max_messages_per_connection == 0 {
            v.push(
                at("downstream.pool.max_messages_per_connection"),
                "must be at least 1",
            );
        }

        // §6.7: a DKIM preflight check needs a selector to look up.
        if let Some(p) = &route.preflight {
            if p.enabled && p.dkim_selector.as_ref().is_none_or(|s| s.trim().is_empty()) {
                v.push(
                    at("preflight.dkim_selector"),
                    "is required when preflight is enabled: the DKIM check resolves \
                     <selector>._domainkey.<domain> and has nothing to query without it",
                );
            }
            if p.enabled && p.spf_include.as_ref().is_none_or(|s| s.trim().is_empty()) {
                v.push(
                    at("preflight.spf_include"),
                    "is required when preflight is enabled: the SPF check asserts that the \
                     published record contains it",
                );
            }
        }

        if let Some(rf) = &route.recipient_frequency {
            if rf.threshold == 0 {
                v.push(
                    at("recipient_frequency.threshold"),
                    "must be at least 1; zero would make the route permanently ineligible",
                );
            }
            if rf.window.count == 0 {
                v.push(at("recipient_frequency.window.count"), "must be at least 1");
            }
        }
    }
}

fn check_schedule(cfg: &Config, route: &Route, warmup: &super::Warmup, v: &mut ViolationList) {
    let at = |suffix: &str| format!("routes.{}.warmup.schedule.{suffix}", route.name);

    // §4.2: "A warmup.schedule array is empty, or contains a negative value."
    check_series(&warmup.schedule.default, &at("default"), v);

    for (group, series) in &warmup.schedule.overrides {
        // §4.2: "An overrides key names a nonexistent domain group."
        if cfg.domain_group(group).is_none() {
            v.push(
                at(&format!("overrides.{group}")),
                format!("names domain group '{group}', which is not defined"),
            );
        }
        check_series(series, &at(&format!("overrides.{group}")), v);
    }
}

fn check_series(series: &[i64], path: &str, v: &mut ViolationList) {
    if series.is_empty() {
        v.push(path, "must not be empty");
        return;
    }
    for (i, value) in series.iter().enumerate() {
        if *value < 0 {
            v.push(
                format!("{path}[{i}]"),
                format!("is negative ({value}); a daily allowance cannot be below zero"),
            );
        }
    }
}

fn check_identity(identity: &Identity, route_name: &str, v: &mut ViolationList) {
    let at = |suffix: &str| format!("routes.{route_name}.identity.{suffix}");

    if identity.envelope_from.trim().is_empty() {
        v.push(at("envelope_from"), "must not be empty");
    }

    // §4.2: "A body_rewrites.pattern fails to compile."
    for (i, rewrite) in identity.body_rewrites.iter().enumerate() {
        if let Err(e) = regex::Regex::new(&rewrite.pattern) {
            // A regex parse error renders over several lines: a "regex parse
            // error:" banner, the offending pattern, a caret, then the actual
            // diagnosis. The banner alone tells the reader nothing, so take the
            // last non-empty line — that is the one that names the problem.
            let rendered = e.to_string();
            let reason = rendered
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("invalid");
            v.push(
                at(&format!("body_rewrites[{i}].pattern")),
                format!("does not compile: {reason}"),
            );
        }
    }

    // §6.6 / §4.2: "An identity field is named in unstable_headers." This half of
    // the stability rules is purely syntactic, so it is enforced now rather than
    // waiting for the rewrite engine in phase 4.
    for header in &identity.unstable_headers {
        if is_identity_header(header) {
            v.push(
                at("unstable_headers"),
                format!(
                    "names identity field '{header}'. Identity fields determine which domain \
                     accrues reputation; their stability is not overridable (§6.6)"
                ),
            );
        }
    }
    // A header the route never sets was a *violation* in phase 1, when there was
    // no way to tell a stale declaration from a live one. §6.6 says otherwise —
    // "Naming a header that is in fact stable is also a startup WARN" — and a
    // header nothing writes is stable by definition. The stability probe below
    // now reports it, and `warnings()` emits it at the severity the spec asks
    // for.

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (i, (name, _)) in identity.set_headers.0.iter().enumerate() {
        if name.trim().is_empty() {
            v.push(at(&format!("set_headers[{i}]")), "has an empty header name");
        }
        // §6.2: "Setting a header that already exists replaces all instances."
        // Two entries for one name in the config is ambiguous rather than
        // additive, so it is rejected.
        if let Some(prev) = seen.insert(name.to_ascii_lowercase(), i) {
            v.push(
                at(&format!("set_headers.{name}")),
                format!("is set twice (also at entry {prev})"),
            );
        }
    }

    // §6.2 applies remove_headers before set_headers so a header may appear in
    // both; that is a documented idiom, not a violation. But removing an
    // identity field without setting it leaves the message with no From:, which
    // is not a legal message.
    for header in &identity.remove_headers {
        if header.eq_ignore_ascii_case("From") && !identity.set_headers.contains_key("From") {
            v.push(
                at("remove_headers"),
                "removes 'From' without setting it; the result would not be a legal message",
            );
        }
    }

    // §6.6, the property itself. Everything above this point is syntactic; this
    // is the rule the whole component turns on, and it is checked by running the
    // real rewrite engine against a synthetic probe — the same `rewrite()` the
    // relay calls, never a second implementation of it.
    match RouteRewrite::compile(identity) {
        // D-034 — an unknown template variable is a configuration error. Nothing
        // downstream of a failed compile can be checked, so the stability probe
        // is skipped and the parse errors stand on their own.
        Err(errors) => {
            for e in errors {
                v.push(at(&e.field), e.error.to_string());
            }
        }
        Ok(compiled) => {
            let report = stability::probe(&compiled);
            for u in report.unstable {
                if u.identity {
                    // "A stability violation here is a fatal startup error with
                    // no override."
                    v.push(
                        at(&stability_path(&u.field)),
                        format!(
                            "is not stable: rewriting twice gives '{}' then '{}'. {} is an \
                             identity field, so this is not overridable — arrangement A and \
                             arrangement B of §1.1 would produce materially different mail (§6.6)",
                            u.first, u.second, u.field
                        ),
                    );
                } else if !compiled.is_declared_unstable(&u.field) {
                    v.push(
                        at(&stability_path(&u.field)),
                        format!(
                            "is not stable: rewriting twice gives '{}' then '{}'. It reads a \
                             field this route also writes, so reconfiguring the application \
                             would silently change what the recipient sees. Declare it in \
                             unstable_headers if that is intended and migration-only (§6.6)",
                            u.first, u.second
                        ),
                    );
                }
                // Declared: downgraded to the WARN `warnings()` emits.
            }
        }
    }
}

/// Point at the key the operator would edit, which for a header is its
/// `set_headers` entry.
fn stability_path(field: &str) -> String {
    if field == "envelope_from" {
        field.to_string()
    } else {
        format!("set_headers.{field}")
    }
}

fn check_chains(cfg: &Config, v: &mut ViolationList) {
    for (i, rule) in cfg.senders.iter().enumerate() {
        let path = format!("senders[{i}] (match '{}')", rule.pattern);
        if rule.pattern.trim().is_empty() {
            v.push(&path, "match must not be empty");
        }
        check_chain(cfg, &rule.chain, &path, v);
    }
}

/// §3.1: "Zero or more warming routes, followed by **at most one** overflow
/// route. It must be last in any chain containing it."
fn check_chain(cfg: &Config, chain: &[String], path: &str, v: &mut ViolationList) {
    if chain.is_empty() {
        v.push(path, "chain must not be empty");
        return;
    }

    let mut overflow_positions = Vec::new();

    for (i, name) in chain.iter().enumerate() {
        // §4.2: "A referenced route name does not exist."
        let Some(route) = cfg.route(name) else {
            v.push(
                path,
                format!("chain[{i}] references route '{name}', which is not defined"),
            );
            continue;
        };
        if route.overflow {
            overflow_positions.push(i);
        }
    }

    // §4.2: "A chain contains more than one overflow route, or an overflow route
    // is not last."
    if overflow_positions.len() > 1 {
        v.push(
            path,
            format!(
                "chain contains {} overflow routes ({}); at most one is permitted",
                overflow_positions.len(),
                overflow_positions
                    .iter()
                    .map(|i| chain[*i].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    if let Some(first) = overflow_positions.first() {
        if *first != chain.len() - 1 {
            v.push(
                path,
                format!(
                    "overflow route '{}' is at position {} but must be last",
                    chain[*first], first
                ),
            );
        }
    }
}

fn check_default_chain(cfg: &Config, v: &mut ViolationList) {
    // §4.2: "strict_senders: false and default_chain is absent or its final
    // route is not an overflow route."
    if cfg.strict_senders {
        // A default_chain is harmless but pointless under strict_senders; still
        // validate it if present, so turning strict_senders off later does not
        // surface new errors.
        if let Some(chain) = &cfg.default_chain {
            check_chain(cfg, chain, "default_chain", v);
        }
        return;
    }

    let Some(chain) = &cfg.default_chain else {
        v.push(
            "default_chain",
            "is required when strict_senders is false: an unmatched sender has nowhere to go",
        );
        return;
    };

    check_chain(cfg, chain, "default_chain", v);

    match chain.last().and_then(|name| cfg.route(name)) {
        Some(route) if route.overflow => {}
        Some(route) => v.push(
            "default_chain",
            format!(
                "final route '{}' is not an overflow route; unmatched senders would be \
                 subject to a warm-up quota with no fallback",
                route.name
            ),
        ),
        // A dangling name was already reported by check_chain.
        None => {}
    }
}
