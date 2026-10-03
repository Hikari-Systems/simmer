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
use std::path::Path;
use std::time::Duration;

use super::{
    AutoShare, BodyStoreConfig, Config, Delivery, Identity, IngressAuth, ObjectProvider,
    ObjectStoreConfig, OnLimit, Ramp, Route, ShareSchedule,
};
use crate::rewrite::{stability, RouteRewrite};

/// The identity fields of §6.6. A stability violation in one of these is fatal
/// with no override, and naming one in `unstable_headers` is itself a violation.
pub const IDENTITY_HEADERS: [&str; 3] = ["From", "Sender", "Message-ID"];

/// Below this, an admin token gets a startup `WARN` (D-053). 16 characters of
/// base64 is 96 bits, which is well past guessable and short enough that no
/// reasonable secret manager produces less by accident.
const MIN_ADMIN_TOKEN_LEN: usize = 16;

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

/// Keys that a previous version accepted and this one does not.
///
/// Every struct is `deny_unknown_fields`, so a removed key already refuses to
/// start — but it refuses with serde's `unknown field`, which tells an operator
/// what is wrong and nothing about why. A configuration that worked yesterday
/// deserves to be told which decision took the key away, so this runs against the
/// raw document before deserialisation and answers in §4.2's own form.
///
/// Removing an entry from this table is safe once nobody is upgrading across it;
/// the `deny_unknown_fields` refusal remains either way.
pub fn removed_keys(tree: &serde_yaml_ng::Value) -> ViolationList {
    /// D-099: the whole routing block moved, unchanged, under `ramps.<name>`.
    const MOVED_TO_RAMPS: &str =
        "moved under ramps (D-099): every routing key — domain_groups, senders, \
         default_chain, strict_senders, thread_affinity, exhausted_chain_reply and routes — \
         now belongs to a named ramp. Indent the existing block under `ramps: { main: ... }` \
         and add `default_ramp: main` at the top level; nothing inside it changes. See \
         DECISIONS.md D-099 and SPEC.md §3.4";
    const REMOVED: [(&[&str], &str); 10] = [
        (&["domain_groups"], MOVED_TO_RAMPS),
        (&["senders"], MOVED_TO_RAMPS),
        (&["default_chain"], MOVED_TO_RAMPS),
        (&["strict_senders"], MOVED_TO_RAMPS),
        (&["thread_affinity"], MOVED_TO_RAMPS),
        (&["exhausted_chain_reply"], MOVED_TO_RAMPS),
        (&["routes"], MOVED_TO_RAMPS),
        (
            &["server", "single_recipient_only"],
            "removed: a transaction may carry exactly one recipient and a second \
             RCPT TO is always refused, so the switch has nothing to select. Delete \
             the key. See DECISIONS.md D-047 and docs/RECIPIENTS.md",
        ),
        (
            &["server", "listen"],
            "replaced by server.listeners, a list with one entry per port, each with \
             its own tls and auth policy. `listen: \"0.0.0.0:25\"` becomes \
             `listeners: [{ address: \"0.0.0.0:25\" }]`, which keeps the old meaning. \
             See DECISIONS.md D-070",
        ),
        (
            &["server", "auth", "required"],
            "replaced by each listener's `auth: disabled | optional | required`. Port \
             25 defaults to optional and 465/587 to required; to keep `required: true` \
             on port 25, set `auth: required` on that listener. See DECISIONS.md D-070",
        ),
    ];

    let mut v = ViolationList::default();
    for (path, message) in REMOVED {
        let mut node = Some(tree);
        for key in path {
            node = node.and_then(|n| n.get(*key));
        }
        if node.is_some() {
            v.push(path.join("."), message);
        }
    }
    v
}

pub fn validate(cfg: &Config) -> ViolationList {
    let mut v = ViolationList::default();

    check_listeners(cfg, &mut v);
    check_server_tls(cfg, &mut v);
    check_cidrs(cfg, &mut v);
    check_auth(cfg, &mut v);
    check_admin_tokens(cfg, &mut v);
    check_admin_metrics(cfg, &mut v);
    check_ramps(cfg, &mut v);
    check_link_proxy(cfg, &mut v);
    check_capture(cfg, &mut v);
    check_telemetry(cfg, &mut v);
    check_spool(cfg, &mut v);
    check_storage(cfg, &mut v);

    v
}

/// Non-fatal conditions, logged at `WARN` at startup.
pub fn warnings(cfg: &Config) -> Vec<Warning> {
    let mut out = Vec::new();

    // D-093: `/metrics` became opt-in in v0.7.0. A configuration that does not
    // mention it is most likely one from before, whose scrapes and alerts have
    // just stopped — and a clean startup would say nothing about it. `false`
    // is a choice, and silences this.
    if cfg.admin.metrics.is_none() {
        out.push(Warning {
            path: "admin.metrics".to_string(),
            message: "is not set, so /metrics is off and answers 404 — the default since \
                      v0.7.0 (D-093). Set `admin.metrics: true` to serve it, or `false` to \
                      silence this warning"
                .to_string(),
        });
    }

    // §5.5's ceiling can no longer be reached: D-047 refuses the second RCPT TO
    // whatever this says, so any value above 1 describes a limit that will never
    // apply. Left in the schema because §4.1 mandates the key, and warned about
    // rather than rejected because it is a stale expectation, not a mistake —
    // the same reasoning D-040 applies to a stale `unstable_headers`.
    if cfg.server.max_recipients > 1 {
        out.push(Warning {
            path: "server.max_recipients".to_string(),
            message: format!(
                "is {}, but has no effect: a transaction may carry exactly one recipient \
                 and a second RCPT TO is always refused (D-047). Set it to 1",
                cfg.server.max_recipients
            ),
        });
    }

    // D-070 — a certificate nothing presents. Not a violation: it is loaded and
    // checked either way, and a staged rollout may add the TLS listener later.
    if cfg.server.tls.is_some()
        && !cfg
            .server
            .listeners
            .iter()
            .any(|l| l.tls_mode().can_encrypt())
    {
        out.push(Warning {
            path: "server.tls".to_string(),
            message: "names a certificate, but every listener has tls: off, so it is never \
                      presented"
                .to_string(),
        });
    }

    // D-071 — the ACL gates *authenticated* sessions. An `optional` listener lets
    // an unauthenticated client send as anyone `allowed_cidrs` admits, which is
    // the pre-ACL trust model and a legitimate choice on port 25, but one worth
    // saying out loud once there is an ACL that looks like it covers everything.
    if !cfg.server.auth.users.is_empty() {
        for (i, l) in cfg.server.listeners.iter().enumerate() {
            if l.auth_mode() == IngressAuth::Optional {
                out.push(Warning {
                    path: format!("server.listeners[{i}] ({})", l.address),
                    message: "has auth: optional, so a client that does not authenticate may \
                              present any sender identity; the grants in server.auth.users \
                              apply only to sessions that do. allowed_cidrs is the only \
                              control there"
                        .to_string(),
                });
            }
        }
    }

    // D-083 — the load balancer terminates TLS for the click, so an `http://`
    // upstream means the recipient's tracking token crosses the network in the
    // clear on the second leg. Legitimate for an upstream on the same segment.
    if let Some(lp) = &cfg.link_proxy {
        if lp.upstream.to_ascii_lowercase().starts_with("http://") {
            out.push(Warning {
                path: "link_proxy.upstream".to_string(),
                message: "is http://, so forwarded clicks, their cookies and their tracking \
                          tokens reach the upstream unencrypted"
                    .to_string(),
            });
        }
    }

    // D-085 — the one warning in this file that is about what Simmer will write
    // rather than about what it will do. §7.3 hashes recipients precisely so
    // that the container does not accumulate a plaintext record of every address
    // mailed; enabling capture is choosing to accumulate exactly that, plus the
    // bodies. An operator who meant it will read this and move on. An operator
    // who left it on after an afternoon of debugging needs to see it every time
    // the process starts.
    if let Some(c) = &cfg.capture {
        out.push(Warning {
            path: "capture".to_string(),
            message: format!(
                "is enabled: every accepted message's body and its recipient are written to \
                 '{}' in the clear, retained for {}h. This is a debugging mode (D-085) — \
                 §7.3 hashes recipients to avoid exactly this, so do not leave it on in \
                 production",
                c.directory,
                c.retention.as_secs() / 3600
            ),
        });
    }

    // D-126 — vendor API keys travel as gRPC metadata. Over a plain `http://`
    // endpoint off this host they cross the network in the clear: the link
    // proxy's `http://` warning, for the same reason. Loopback is a sidecar
    // collector, which is the usual and safe arrangement.
    if let Some(t) = cfg.telemetry() {
        if !t.headers.is_empty() && plaintext_off_host(&t.endpoint) {
            out.push(Warning {
                path: "telemetry.endpoint".to_string(),
                message: "is http:// to a host other than loopback, so telemetry.headers — \
                          typically a vendor API key — cross the network unencrypted"
                    .to_string(),
            });
        }
    }
    if let Some(t) = &cfg.telemetry {
        if cfg.telemetry().is_none() {
            out.push(Warning {
                path: "telemetry".to_string(),
                message: if t.enabled {
                    "is present but its endpoint is empty, so nothing is exported (D-126)"
                } else {
                    "is present but enabled: false, so nothing is exported (D-126)"
                }
                .to_string(),
            });
        }
    }

    // D-116 — a spool nothing uses. Not a violation: a staged rollout may add
    // the block before flipping a ramp. But it is a body store probed and
    // tables migrated for nothing, and an operator may believe a ramp spools.
    if let Some(sp) = &cfg.spool {
        if !cfg.ramps.iter().any(|r| r.delivery == Delivery::Spool) {
            out.push(Warning {
                path: "spool".to_string(),
                message: "is configured, but no ramp has `delivery: spool`, so nothing is \
                          spooled (D-116)"
                    .to_string(),
            });
        } else if let BodyStoreConfig::Volume { path } = &sp.body_store {
            out.push(Warning {
                path: "spool.body_store.path".to_string(),
                message: format!(
                    "'{path}' passed a write, fsync and read-back probe. On a network \
                     filesystem that proves it is writable, not that an fsync is durable; \
                     a spooled message is only as safe as that fsync (D-117)"
                ),
            });
        }
        if let Some(w) = &sp.dead_letter.webhook {
            if w.include_addresses && w.url.to_ascii_lowercase().starts_with("http://") {
                out.push(Warning {
                    path: "spool.dead_letter.webhook.url".to_string(),
                    message: "is http:// with include_addresses: true, so every dead letter's \
                              sender and recipient cross the network unencrypted (D-120)"
                        .to_string(),
                });
            }
        }
    }

    // §9.3's write API can pause a route or set an allowance of zero, and either
    // one turns into `451` on every message that steers there. A short token is
    // the difference between "an operator did that" and "anyone who could reach
    // the admin port did that". Not a violation, because a length threshold is a
    // judgement rather than a rule and §4.1 sets none.
    for (name, token) in cfg.admin.credentials() {
        if !token.is_empty() && token.len() < MIN_ADMIN_TOKEN_LEN {
            out.push(Warning {
                path: if name == super::Admin::DEFAULT_TOKEN_NAME {
                    "admin.auth_token".to_string()
                } else {
                    format!("admin.tokens ('{name}')")
                },
                message: format!(
                    "is {} characters; §9.3 can pause a route or zero an allowance, either of \
                     which answers every affected message 451. Use at least {} random characters",
                    token.len(),
                    MIN_ADMIN_TOKEN_LEN
                ),
            });
        }
    }

    // Everything else is about one ramp's routing, and is reported under it.
    for ramp in cfg.ramps.iter() {
        for w in ramp_warnings(ramp) {
            out.push(Warning {
                path: format!("ramps.{}.{}", ramp.name, w.path),
                message: w.message,
            });
        }
    }

    out
}

/// The per-ramp half of [`warnings`]: the pre-D-099 routing warnings, with
/// paths relative to the ramp.
fn ramp_warnings(ramp: &Ramp) -> Vec<Warning> {
    let mut out = Vec::new();

    // D-111: a rate that cannot carry the day's cap. Not a violation — a slow
    // rate under a high cap may be exactly what is wanted — but the cap is then
    // decorative, and an operator reading the schedule would not know it.
    for route in &ramp.routes {
        out.extend(rate_cap_warnings(ramp, route));
    }

    // D-118: `max_wait` holds a synchronous client for a slot. A spooling ramp
    // has already answered its client, so the key describes nothing.
    if ramp.delivery == Delivery::Spool {
        for route in &ramp.routes {
            if route.rate.as_ref().is_some_and(|r| !r.max_wait().is_zero()) {
                out.push(Warning {
                    path: format!("routes.{}.rate.max_wait", route.name),
                    message: "has no effect on a `delivery: spool` ramp, whose client is \
                              answered before delivery; use on_limit: wait to defer to a \
                              slot (D-118)"
                        .to_string(),
                });
            }
        }
    }

    // §7.3 is a *steering* rule: over threshold means "try the next link". A
    // constraint on the last link of a chain has no next link to steer to, so it
    // stops steering and starts refusing — the message gets §10.3's `451` instead
    // of going out by another route. That is a legitimate configuration (it is
    // how "never mail this person more than twice a day, full stop" is spelled),
    // but it is much more often a mistake, and it is invisible until the day a
    // recipient reaches the threshold.
    for chain in ramp.chains() {
        let Some(last) = chain.routes.last() else {
            continue;
        };
        if ramp
            .route(last)
            .is_some_and(|r| r.recipient_frequency.is_some())
        {
            out.push(Warning {
                path: format!("{}: {last}", chain.path),
                message: "is the last route in this chain and carries a recipient_frequency \
                          constraint, so a recipient over threshold has nothing to fall through \
                          to and the message is answered 451 rather than steered (§7.3, §10.3)"
                    .to_string(),
            });
        }
    }

    // §6.7's `strict` on the last link, for D-052's reason exactly: with no next
    // link the rule stops steering and starts refusing, so a DNS problem answers
    // 451 rather than routing around itself. That is the one outcome §6.7's
    // non-blocking default exists to avoid, and it is invisible in the config.
    for chain in ramp.chains() {
        let Some(last) = chain.routes.last() else {
            continue;
        };
        if ramp
            .route(last)
            .is_some_and(|r| r.preflight.as_ref().is_some_and(|p| p.enabled && p.strict))
        {
            out.push(Warning {
                path: format!("{}: {last}", chain.path),
                message: "is the last route in this chain and sets preflight.strict, so a \
                          failing DNS check has nothing to fall through to and the message is \
                          answered 451 rather than steered (§6.7, §10.3)"
                    .to_string(),
            });
        }
    }

    for route in &ramp.routes {
        // §6.7 — the route whose identity domain is not a constant, so there is
        // no single domain to check on a timer (D-064).
        //
        // The second sentence is the point of the warning. Silence here would let
        // an operator set `strict: true`, see no error, and believe a gate is
        // protecting them that is not running at all.
        //
        // **Unreachable through `load()` since D-069**, which makes the same
        // condition a §4.2 violation — the route is refused before this runs.
        // Kept rather than deleted, because it is `warnings()` that would have to
        // notice if that rule were ever relaxed, and rediscovering this reasoning
        // from an empty function is not something a future reader should have to
        // do. `preflight::plan` skips such a route for the same reason.
        if route.preflight_enabled()
            && crate::preflight::literal_domain(&route.identity.envelope_from).is_none()
        {
            let strict = route.preflight.as_ref().is_some_and(|p| p.strict);
            out.push(Warning {
                path: format!("routes.{}.preflight", route.name),
                message: format!(
                    "is enabled, but identity.envelope_from ('{}') has no constant domain, so \
                     there is nothing to check on an interval and preflight will not run for \
                     this route (§6.7).{}",
                    route.identity.envelope_from,
                    if strict {
                        " strict: true therefore has NO EFFECT here: the route is never made \
                         ineligible, because no check ever produces a verdict"
                    } else {
                        ""
                    }
                ),
            });
        }

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

        // §6.5 (D-099): `X-Simmer-Ramp` is stripped before `set_headers` runs,
        // so a route can put it back. Legal, and almost certainly a mistake: it
        // would hand a Simmer instruction to the downstream.
        for (header, _) in route.identity.set_headers.iter() {
            if crate::rewrite::CONTROL_HEADERS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(header))
            {
                out.push(Warning {
                    path: format!("routes.{}.identity.set_headers", route.name),
                    message: format!(
                        "sets '{header}', which §6.5 strips as Simmer's own control header; \
                         setting it hands a Simmer instruction to the downstream (D-099)"
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

        // §6.3's "a template referencing recipient.* is a startup validation
        // warning, since it forces per-recipient splitting" used to live here.
        // It fired only when `single_recipient_only` was false, and D-047 removed
        // that case: every transaction has exactly one recipient, so `recipient.*`
        // always renders a real value and forces nothing.

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

    // D-089 — a `header_rewrites` entry that can never take effect. Not a
    // violation: §6.2's own idiom is naming a header in two lists, and the
    // order makes each case well defined. But the operator wrote a rule
    // expecting it to fire, and it will not.
    for route in &ramp.routes {
        let identity = &route.identity;
        for (i, rule) in identity.header_rewrites.iter().enumerate() {
            let path = format!("routes.{}.identity.header_rewrites[{i}]", route.name);
            let header = &rule.header;
            let shadowed_by = if identity.set_headers.contains_key(header) {
                Some("set_headers, which runs after it and replaces the value")
            } else if identity
                .remove_headers
                .iter()
                .any(|h| h.eq_ignore_ascii_case(header))
            {
                Some("remove_headers, which runs before it")
            } else if crate::rewrite::AUTH_ARTEFACTS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(header))
            {
                Some("§6.5's unconditional strip, which runs before it")
            } else {
                None
            };
            if let Some(by) = shadowed_by {
                out.push(Warning {
                    path,
                    message: format!(
                        "rewrites '{header}', which is also handled by {by}: this rule never \
                         takes effect (D-089)"
                    ),
                });
            }
        }
    }

    // §14.2: an unmatched sender goes to the overflow route at full volume. If
    // that is not what the operator wants, strict_senders exists.
    if !ramp.strict_senders {
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
    if cfg.server.listeners.is_empty() {
        v.push(
            "server.listeners",
            "is empty, so Simmer would accept no mail at all",
        );
    }
    let mut seen: BTreeMap<SocketAddr, usize> = BTreeMap::new();
    for (i, l) in cfg.server.listeners.iter().enumerate() {
        let path = format!("server.listeners[{i}]");
        match l.address.parse::<SocketAddr>() {
            Err(_) => v.push(
                format!("{path}.address"),
                format!("'{}' is not a valid host:port address", l.address),
            ),
            // Port 0 is "any free port", which tests bind several of; two of
            // them never collide.
            Ok(addr) if addr.port() != 0 => {
                if let Some(prev) = seen.insert(addr, i) {
                    v.push(
                        format!("{path}.address"),
                        format!("'{addr}' duplicates server.listeners[{prev}]"),
                    );
                }
            }
            Ok(_) => {}
        }

        // §4.2 (D-099): an affinity must name a ramp, and an override needs an
        // affinity to override.
        if let Some(ramp) = &l.ramp {
            if cfg.ramps.get(ramp).is_none() {
                v.push(
                    format!("{path}.ramp"),
                    format!("'{ramp}' names no ramp in `ramps`"),
                );
            }
        } else if l.header_overrides_affinity {
            v.push(
                format!("{path}.header_overrides_affinity"),
                "is true on a listener with no `ramp`, where there is no affinity for a \
                 header to override; a listener without one already routes by the header",
            );
        }

        let tls = l.tls_mode();
        if tls.can_encrypt() && cfg.server.tls.is_none() {
            v.push(
                format!("{path}.tls"),
                format!(
                    "is {} (the {} for this port) but server.tls names no certificate",
                    tls.as_str(),
                    if l.tls.is_some() {
                        "configured value"
                    } else {
                        "default"
                    },
                ),
            );
        }

        let auth = &cfg.server.auth;
        if l.auth_mode() == IngressAuth::Required {
            // §4.2: "auth.required: true with an empty user list", per listener.
            if auth.users.is_empty() {
                v.push(
                    format!("{path}.auth"),
                    "is required but server.auth.users is empty, so no client could ever \
                     authenticate and this listener would accept nothing",
                );
            } else if auth.mechanisms.is_empty() {
                v.push(
                    format!("{path}.auth"),
                    "is required but server.auth.mechanisms is empty",
                );
            }
            // The inversion of the old §4.2 rule. With TLS impossible here and
            // plaintext AUTH refused, AUTH can never succeed on this listener,
            // so `required` would mean "refuse everything".
            if !tls.can_encrypt() && !auth.allow_insecure_auth {
                v.push(
                    format!("{path}.auth"),
                    "is required on a listener with tls: off, but \
                     server.auth.allow_insecure_auth is false, so AUTH could never be \
                     used and every message would be refused. Enable TLS on this \
                     listener, or set allow_insecure_auth: true to accept plaintext \
                     credentials on a trusted segment",
                );
            }
        }
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

/// D-083. Everything `link_proxy::Listener::bind` and the crate's
/// `ReverseProxy::new_with_client` rely on — the latter *panics* on a target it
/// cannot parse, so a URI that reaches it has to have passed here first.
fn check_link_proxy(cfg: &Config, v: &mut ViolationList) {
    let Some(lp) = &cfg.link_proxy else {
        return;
    };

    match lp.listen.parse::<SocketAddr>() {
        Err(_) => v.push(
            "link_proxy.listen",
            format!("'{}' is not a valid host:port address", lp.listen),
        ),
        Ok(addr) if addr.port() != 0 => {
            let smtp = cfg
                .server
                .listeners
                .iter()
                .position(|l| l.address.parse::<SocketAddr>().ok() == Some(addr));
            if let Some(i) = smtp {
                v.push(
                    "link_proxy.listen",
                    format!("'{addr}' duplicates server.listeners[{i}]"),
                );
            }
            if cfg.admin.listen.parse::<SocketAddr>().ok() == Some(addr) {
                v.push(
                    "link_proxy.listen",
                    format!("'{addr}' duplicates admin.listen"),
                );
            }
        }
        Ok(_) => {}
    }

    if let Some(problem) = upstream_problem(&lp.upstream) {
        v.push(
            "link_proxy.upstream",
            format!("'{}' {problem}", lp.upstream),
        );
    }

    if lp.allowed_cidrs.is_empty() {
        v.push(
            "link_proxy.allowed_cidrs",
            "is empty, so every connection would be refused. List the load balancer's subnets",
        );
    }
    for (i, cidr) in lp.allowed_cidrs.iter().enumerate() {
        if cidr.parse::<ipnet::IpNet>().is_err() {
            v.push(
                format!("link_proxy.allowed_cidrs[{i}]"),
                format!("'{cidr}' is not a valid CIDR block"),
            );
        }
    }

    if lp.max_connections == 0 {
        v.push("link_proxy.max_connections", "must be at least 1");
    }
    if lp.max_request_bytes == 0 {
        v.push("link_proxy.max_request_bytes", "must be greater than zero");
    }
    let t = &lp.timeouts;
    for (name, d) in [
        ("header_read", t.header_read),
        ("upstream_connect", t.upstream_connect),
        ("upstream_response", t.upstream_response),
        ("idle", t.idle),
    ] {
        if d.is_zero() {
            v.push(
                format!("link_proxy.timeouts.{name}"),
                "must be greater than zero",
            );
        }
    }
}

/// D-085 — everything `capture::Capture::start` relies on.
///
/// The directory is checked here rather than on the first message because a
/// capture that silently writes nothing is the worst outcome available: the
/// operator enabled it for a reason, and would find out at the moment they went
/// looking for the records. §4.2's posture — refuse to start, report everything
/// — is what makes "it is on" and "it is working" the same statement.
fn check_capture(cfg: &Config, v: &mut ViolationList) {
    let Some(c) = &cfg.capture else {
        return;
    };

    let dir = Path::new(&c.directory);
    if c.directory.trim().is_empty() {
        v.push("capture.directory", "is empty");
    } else if !dir.is_absolute() {
        // A relative path resolves against the working directory, which is
        // `/app` in the container and wherever the operator stood in a test.
        // The same config would then capture to two different places.
        v.push(
            "capture.directory",
            format!("'{}' is not an absolute path", c.directory),
        );
    } else if dir.exists() {
        if !dir.is_dir() {
            v.push(
                "capture.directory",
                format!("'{}' exists and is not a directory", c.directory),
            );
        } else if let Err(e) = writable(dir) {
            v.push(
                "capture.directory",
                format!("'{}' is not writable: {e}", c.directory),
            );
        }
    } else {
        // It will be created, so what matters is the parent. The container's
        // root filesystem is read-only (docker-compose.yml), which is the
        // failure this catches in practice.
        match dir.parent() {
            None => v.push(
                "capture.directory",
                format!("'{}' has no parent directory", c.directory),
            ),
            Some(parent) if !parent.is_dir() => v.push(
                "capture.directory",
                format!(
                    "'{}' does not exist and neither does its parent '{}'",
                    c.directory,
                    parent.display()
                ),
            ),
            Some(parent) => {
                if let Err(e) = writable(parent) {
                    v.push(
                        "capture.directory",
                        format!(
                            "'{}' would have to be created in '{}', which is not writable: {e}",
                            c.directory,
                            parent.display()
                        ),
                    );
                }
            }
        }
    }

    if c.max_body_bytes == 0 {
        v.push("capture.max_body_bytes", "must be greater than zero");
    }

    // A retention shorter than one bucket would make the sweeper delete the file
    // the writer is appending to.
    let bucket = Duration::from_secs(
        u64::try_from(crate::capture::bucket::BUCKET_SECS).expect("BUCKET_SECS is positive"),
    );
    if c.retention < bucket {
        v.push(
            "capture.retention",
            format!(
                "must be at least {}s, one bucket; a shorter retention would sweep the file \
                 being written",
                bucket.as_secs()
            ),
        );
    }

    if c.queue_depth == 0 {
        v.push("capture.queue_depth", "must be at least 1");
    }
    if c.max_queue_bytes == 0 {
        v.push("capture.max_queue_bytes", "must be greater than zero");
    } else if c.max_queue_bytes < cfg.server.max_message_bytes.saturating_mul(2) {
        // One maximum-size message is a ~1.34x base64 line, and the writer needs
        // room for the one it is writing plus the next. Below that, the largest
        // messages — the ones a capture is usually chasing — are the only ones
        // that never make it into the file.
        v.push(
            "capture.max_queue_bytes",
            format!(
                "is {}, below twice server.max_message_bytes ({}); the largest messages would \
                 always be dropped by the capture queue",
                c.max_queue_bytes,
                cfg.server.max_message_bytes.saturating_mul(2)
            ),
        );
    }
}

/// Whether `dir` can be written to, reported as the OS reports it.
///
/// `std::fs::Permissions` answers "what do the mode bits say", which is not the
/// question — the container runs as UID 1000 against a mount whose ownership
/// nobody checked, and a read-only filesystem has perfectly permissive modes.
/// Creating and removing a file is the only answer that is not a guess.
fn writable(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(".simmer-capture-probe-{}", std::process::id()));
    std::fs::File::create(&probe)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// §4.2 for §7.7's spool (D-116). The probe of a volume path writes, `fsync`s,
/// reads back and deletes a file — the whole of what the body store will do —
/// so a mount that accepts a create but not a write is refused here, not on the
/// first `250 queued`.
fn check_spool(cfg: &Config, v: &mut ViolationList) {
    // D-116: a ramp must not claim to spool without spooling. Before this rule
    // `delivery: spool` loaded and was ignored, which is the worst outcome — the
    // operator believes the client is answered at once and it is held instead.
    if cfg.spool.is_none() {
        for ramp in cfg.ramps.iter() {
            if ramp.delivery == Delivery::Spool {
                v.push(
                    format!("ramps.{}.delivery", ramp.name),
                    "is `spool`, but there is no top-level `spool:` block saying where \
                     messages are kept (§7.7, D-116)",
                );
            }
        }
    }

    let Some(sp) = &cfg.spool else {
        return;
    };

    match &sp.body_store {
        BodyStoreConfig::Volume { path } => check_spool_volume(path, v),
        BodyStoreConfig::Object(o) => check_object_store(o, v),
    }

    if sp.max_hold.is_zero() {
        v.push("spool.max_hold", "must be greater than zero");
    }

    let r = &sp.retry;
    if r.initial.is_zero() {
        v.push("spool.retry.initial", "must be greater than zero");
    }
    if r.max.is_zero() {
        v.push("spool.retry.max", "must be greater than zero");
    }
    if !r.factor.is_finite() || r.factor < 1.0 {
        v.push(
            "spool.retry.factor",
            format!(
                "is {}; must be a finite number of at least 1, or the backoff shrinks",
                r.factor
            ),
        );
    }
    if r.initial > r.max {
        v.push(
            "spool.retry.initial",
            format!(
                "is {}s, above spool.retry.max ({}s)",
                r.initial.as_secs(),
                r.max.as_secs()
            ),
        );
    }

    if sp.max_messages == 0 {
        v.push("spool.max_messages", "must be at least 1");
    }
    if sp.max_bytes < cfg.server.max_message_bytes {
        v.push(
            "spool.max_bytes",
            format!(
                "is {}, below server.max_message_bytes ({}); the largest messages could never \
                 be admitted (D-119)",
                sp.max_bytes, cfg.server.max_message_bytes
            ),
        );
    }

    if sp.dispatch.poll_interval.is_zero() {
        v.push("spool.dispatch.poll_interval", "must be greater than zero");
    }
    if sp.dispatch.batch == 0 {
        v.push("spool.dispatch.batch", "must be at least 1");
    }

    let dl = &sp.dead_letter;
    if dl.retention.is_zero() {
        v.push("spool.dead_letter.retention", "must be greater than zero");
    }
    // A body kept past its row would be an orphan the sweeper deletes anyway,
    // and in the meantime a plaintext message nothing points at (Q6).
    if dl.keep_body > dl.retention {
        v.push(
            "spool.dead_letter.keep_body",
            format!(
                "is {}s, longer than spool.dead_letter.retention ({}s); a body cannot outlive \
                 the entry that names it (D-121)",
                dl.keep_body.as_secs(),
                dl.retention.as_secs()
            ),
        );
    }
    if let Some(w) = &dl.webhook {
        if w.timeout.is_zero() {
            v.push(
                "spool.dead_letter.webhook.timeout",
                "must be greater than zero",
            );
        }
        if let Some(problem) = http_url_problem(&w.url, true) {
            // The URL itself is not echoed: a webhook URL often carries a token.
            v.push("spool.dead_letter.webhook.url", problem);
        }
    }
}

fn check_spool_volume(path: &str, v: &mut ViolationList) {
    const AT: &str = "spool.body_store.path";
    let dir = Path::new(path);
    if path.trim().is_empty() {
        v.push(AT, "is empty");
        return;
    }
    if !dir.is_absolute() {
        v.push(AT, format!("'{path}' is not an absolute path"));
        return;
    }
    // The body store creates a missing directory itself (0700), so what must be
    // writable then is the parent; the round trip is checked in whichever
    // exists.
    let target = if dir.exists() {
        if !dir.is_dir() {
            v.push(AT, format!("'{path}' exists and is not a directory"));
            return;
        }
        dir
    } else {
        match dir.parent() {
            Some(parent) if parent.is_dir() => parent,
            Some(parent) => {
                v.push(
                    AT,
                    format!(
                        "'{path}' does not exist and neither does its parent '{}'",
                        parent.display()
                    ),
                );
                return;
            }
            None => {
                v.push(AT, format!("'{path}' has no parent directory"));
                return;
            }
        }
    };
    if let Err(e) = round_trip(target) {
        v.push(
            AT,
            format!(
                "'{}' failed a write, fsync and read-back probe: {e}",
                target.display()
            ),
        );
    }
}

/// Write, `fsync`, read back and delete a probe file in `dir`.
fn round_trip(dir: &Path) -> std::io::Result<()> {
    use std::io::{Read, Write};
    const PROBE: &[u8] = b"simmer spool probe\n";
    let probe = dir.join(format!(".simmer-spool-probe-{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&probe)?;
        f.write_all(PROBE)?;
        f.sync_all()?;
        drop(f);
        let mut back = Vec::new();
        std::fs::File::open(&probe)?.read_to_end(&mut back)?;
        if back != PROBE {
            return Err(std::io::Error::other("read back different bytes"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&probe);
    result
}

fn check_object_store(o: &ObjectStoreConfig, v: &mut ViolationList) {
    let at = |field: &str| format!("spool.body_store.{field}");
    let set = |f: &Option<String>| f.as_deref().is_some_and(|s| !s.trim().is_empty());
    let named = |f: &Option<String>| f.is_some();

    match o.provider {
        ObjectProvider::S3 => {
            if !set(&o.bucket) {
                v.push(at("bucket"), "is required for provider: s3");
            }
            for (field, value) in [
                ("container", &o.container),
                ("account", &o.account),
                ("access_key", &o.access_key),
            ] {
                if named(value) {
                    v.push(at(field), "is an Azure field; provider is s3");
                }
            }
            if set(&o.access_key_id) != set(&o.secret_access_key) {
                v.push(
                    at("access_key_id"),
                    "and secret_access_key must be given together, or neither",
                );
            }
        }
        ObjectProvider::Azure => {
            if !set(&o.container) {
                v.push(at("container"), "is required for provider: azure");
            }
            if !set(&o.account) {
                v.push(at("account"), "is required for provider: azure");
            }
            for (field, value) in [
                ("bucket", &o.bucket),
                ("region", &o.region),
                ("access_key_id", &o.access_key_id),
                ("secret_access_key", &o.secret_access_key),
            ] {
                if named(value) {
                    v.push(at(field), "is an S3 field; provider is azure");
                }
            }
        }
    }

    if let Some(prefix) = &o.prefix {
        if prefix.starts_with('/') {
            v.push(at("prefix"), "must not start with '/'");
        }
    }
    if let Some(endpoint) = &o.endpoint {
        if let Some(problem) = http_url_problem(endpoint, o.allow_http) {
            v.push(at("endpoint"), problem);
        }
    }
    if o.timeout.is_zero() {
        v.push(at("timeout"), "must be greater than zero");
    }
}

/// Why `url` is not an absolute `https://` URL with a host (or `http://`, when
/// `allow_http`), or `None` if it is. Never echoes the URL.
fn http_url_problem(url: &str, allow_http: bool) -> Option<&'static str> {
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return Some("is not a valid URL");
    };
    if uri.host().is_none_or(str::is_empty) {
        return Some("has no host");
    }
    match uri.scheme_str().map(str::to_ascii_lowercase).as_deref() {
        Some("https") => None,
        Some("http") if allow_http => None,
        Some("http") => {
            Some("is http://; bodies and addresses are mail, so set allow_http: true to permit it")
        }
        _ => Some("must be an http:// or https:// URL"),
    }
}

/// Why `upstream` is not `scheme://host[:port][/prefix]`, or `None` if it is.
fn upstream_problem(upstream: &str) -> Option<&'static str> {
    // `http::Uri` discards a fragment rather than refusing it.
    if upstream.contains('#') {
        return Some("must not carry a fragment");
    }
    let Ok(uri) = upstream.parse::<axum::http::Uri>() else {
        return Some("is not a valid URI");
    };
    match uri.scheme_str() {
        Some("http" | "https") => {}
        Some(_) => return Some("must use the http or https scheme"),
        None => return Some("must be an absolute URI such as https://link.example.com"),
    }
    let Some(authority) = uri.authority() else {
        return Some("has no host");
    };
    if authority.as_str().contains('@') {
        return Some("must not carry credentials");
    }
    if authority.host().is_empty() {
        return Some("has no host");
    }
    // A path is a prefix for every forwarded request's path. A query has no
    // equivalent: the proxy would silently drop it.
    if uri.query().is_some() {
        return Some("must not carry a query; a path prefix is allowed, a query is not");
    }
    None
}

/// §5.1's certificate, loaded exactly as the listener will load it, so a file
/// that is missing, unreadable, unparseable or paired with the wrong key is a
/// startup violation rather than a listener that fails its first handshake.
/// §4.2 for §9.6's `telemetry` (D-126). Checked whenever the block is present,
/// enabled or not, so switching it on later surfaces nothing new — the
/// `admin.metrics` precedent. Only the endpoint is skipped when empty, since
/// empty is how it is switched off.
fn check_telemetry(cfg: &Config, v: &mut ViolationList) {
    let Some(t) = &cfg.telemetry else { return };

    if !t.endpoint.trim().is_empty() {
        if let Some(problem) = telemetry_endpoint_problem(&t.endpoint) {
            v.push("telemetry.endpoint", problem);
        }
    }

    for (name, value) in &t.headers {
        let path = format!("telemetry.headers.{name}");
        match http::HeaderName::from_bytes(name.as_bytes()) {
            Err(_) => v.push(&path, "is not a valid gRPC metadata key".to_string()),
            Ok(h) => {
                let n = h.as_str();
                if n == "content-type" || n == "te" || n.starts_with("grpc-") {
                    v.push(
                        &path,
                        "is reserved by gRPC and cannot be set as metadata".to_string(),
                    );
                } else if n.ends_with("-bin") {
                    v.push(
                        &path,
                        "names a binary metadata key; only ASCII values are supported".to_string(),
                    );
                }
            }
        }
        if http::HeaderValue::from_str(value).is_err() {
            // The value is very likely a credential: say what is wrong with
            // it, never what it is.
            v.push(
                &path,
                "has a value that is not visible ASCII, so it cannot be sent as gRPC metadata"
                    .to_string(),
            );
        }
    }

    if t.service_name.trim().is_empty() {
        v.push("telemetry.service_name", "must not be empty".to_string());
    }
    for key in t.resource.keys() {
        if key.trim().is_empty() {
            v.push(
                "telemetry.resource",
                "has an empty attribute name".to_string(),
            );
        }
    }

    if let Err(e) = tracing_subscriber::EnvFilter::try_new(&t.level) {
        v.push(
            "telemetry.level",
            format!("is not a valid filter directive: {e}"),
        );
    }

    if !(0.0..=1.0).contains(&t.sample_ratio) {
        v.push(
            "telemetry.sample_ratio",
            format!("is {}; it must be between 0 and 1", t.sample_ratio),
        );
    }
    if t.metrics_interval < Duration::from_secs(1) {
        v.push(
            "telemetry.metrics_interval",
            format!("is {:?}; it must be at least 1s", t.metrics_interval),
        );
    }
    if t.timeout.is_zero() {
        v.push("telemetry.timeout", "must be greater than zero".to_string());
    }
    if !(t.traces || t.metrics || t.logs) {
        v.push(
            "telemetry",
            "turns off traces, metrics and logs alike, so it would export nothing; use \
             enabled: false to keep the block and switch it off"
                .to_string(),
        );
    }
}

/// Why `endpoint` cannot be a gRPC collector address, if it cannot.
fn telemetry_endpoint_problem(endpoint: &str) -> Option<String> {
    let uri: http::Uri = match endpoint.parse() {
        Ok(u) => u,
        Err(e) => return Some(format!("'{endpoint}' is not a valid URL: {e}")),
    };
    match uri.scheme_str() {
        Some("http" | "https") => {}
        _ => return Some(format!("'{endpoint}' must start with http:// or https://")),
    }
    if uri.host().is_none_or(str::is_empty) {
        return Some(format!("'{endpoint}' names no host"));
    }
    if uri.port_u16().is_none() {
        return Some(format!(
            "'{endpoint}' names no port; OTLP/gRPC is conventionally 4317"
        ));
    }
    if uri
        .path_and_query()
        .is_some_and(|p| p.as_str() != "/" && !p.as_str().is_empty())
    {
        return Some(format!(
            "'{endpoint}' has a path; a gRPC endpoint is a scheme, host and port only"
        ));
    }
    None
}

/// `http://` to anything but loopback. An endpoint that does not parse is
/// §4.2's business, not this warning's.
fn plaintext_off_host(endpoint: &str) -> bool {
    let Ok(uri) = endpoint.parse::<http::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("http") {
        return false;
    }
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    !loopback
}

fn check_server_tls(cfg: &Config, v: &mut ViolationList) {
    let Some(tls) = &cfg.server.tls else {
        return;
    };
    if let Err(errors) = crate::smtp::tls::load(tls) {
        for e in errors {
            v.push(e.path, e.message);
        }
    }
}

fn check_auth(cfg: &Config, v: &mut ViolationList) {
    let auth = &cfg.server.auth;

    // The listener-level rules — `auth: required` with nobody to authenticate,
    // or with AUTH unusable — are in `check_listeners`, because since D-070
    // "required" is a property of a port rather than of the server.

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

        // D-071. An empty grant list is default deny taken to its conclusion —
        // a user who can log in and send nothing — and is never what was meant.
        if user.grants.send_as.is_empty() {
            v.push(
                format!("server.auth.users[{i}].grants.send_as"),
                "is empty, so this user could authenticate and then send nothing. List \
                 the sender identities it may present (§5.4's patterns)",
            );
        }
        for (j, pattern) in user.grants.send_as.iter().enumerate() {
            if let Some(problem) = grant_pattern_problem(pattern) {
                v.push(
                    format!("server.auth.users[{i}].grants.send_as[{j}]"),
                    format!("'{pattern}' {problem}"),
                );
            }
        }

        // §4.2 (D-099): each entry a real ramp, named once, and never `*` — a
        // wildcard would grant ramps added later without anyone deciding to.
        let mut named: BTreeMap<&str, usize> = BTreeMap::new();
        for (j, ramp) in user.grants.ramps.iter().enumerate() {
            let path = format!("server.auth.users[{i}].grants.ramps[{j}]");
            if ramp.contains('*') {
                v.push(
                    path,
                    format!("'{ramp}' is a wildcard; name each ramp this user may select"),
                );
            } else if cfg.ramps.get(ramp).is_none() {
                v.push(path, format!("'{ramp}' names no ramp in `ramps`"));
            } else if let Some(prev) = named.insert(ramp, j) {
                v.push(path, format!("'{ramp}' repeats grants.ramps[{prev}]"));
            }
        }
    }
}

/// Why a `send_as` entry cannot be what its author meant, if it cannot.
///
/// §5.4's `Pattern::parse` is total — every string is *some* pattern — which is
/// right for routing, where a sender rule that never matches is visible in
/// `simmer_unmatched_sender_total`. A grant that never matches is visible only as
/// refusals, so the shapes that can never match anything are refused here.
fn grant_pattern_problem(pattern: &str) -> Option<&'static str> {
    let p = pattern.trim();
    if p.is_empty() {
        return Some("is empty");
    }
    if p.chars().any(char::is_whitespace) {
        return Some("contains whitespace, so it can never match an address");
    }
    if p == "*" || p == "*." {
        return Some(
            "would grant every sender identity, which is the absence of an ACL rather \
             than one. List the domains instead",
        );
    }
    if let Some(rest) = p.strip_prefix("*.") {
        if rest.contains('@') || rest.contains('*') {
            return Some("is not a pattern §5.4 recognises (`*.domain` takes a bare domain)");
        }
    } else if p.contains('*') {
        return Some("uses '*' somewhere other than a leading `*.`, which §5.4 does not support");
    }
    // The *last* `@`, as §5.4's matcher splits it: a quoted local part may carry
    // one of its own.
    if let Some((local, domain)) = p.rsplit_once('@') {
        if local.is_empty() || domain.is_empty() {
            return Some("is not a full address (`local@domain`)");
        }
    }
    None
}

/// §9.3's credentials (D-053).
///
/// Not one of §4.2's enumerated rules, because §4.1 has a single scalar and a
/// scalar cannot be inconsistent with itself. Named tokens can be, and every way
/// they can be wrong here ends with the write API either unusable or logging an
/// identifier that does not identify anything.
/// §4.2 for D-093's `admin.metrics`. Checked whether or not it is enabled, so
/// switching it on later surfaces nothing new.
fn check_admin_metrics(cfg: &Config, v: &mut ViolationList) {
    let idle = cfg.admin.metrics().idle_timeout;
    if idle < std::time::Duration::from_secs(60) {
        v.push(
            "admin.metrics.idle_timeout",
            format!(
                "is {idle:?}; it must be at least 1m. Shorter than a scrape interval, a \
                 counter on a quiet route would vanish and reappear from zero between scrapes"
            ),
        );
    }
}

fn check_admin_tokens(cfg: &Config, v: &mut ViolationList) {
    let credentials = cfg.admin.credentials();

    if credentials.is_empty() {
        v.push(
            "admin.auth_token",
            "no admin credential is configured; §9.3's write API would be \
             permanently unusable. Set admin.auth_token, or list one or more \
             admin.tokens",
        );
        return;
    }

    let mut names: BTreeMap<&str, String> = BTreeMap::new();
    let mut secrets: BTreeMap<&str, String> = BTreeMap::new();

    for (i, (name, token)) in credentials.iter().enumerate() {
        // `auth_token` is credentials[0] whenever it is set, and it has no index
        // in the document.
        let path = if *name == super::Admin::DEFAULT_TOKEN_NAME && i == 0 {
            "admin.auth_token".to_string()
        } else {
            let idx = if cfg.admin.auth_token.is_some() {
                i - 1
            } else {
                i
            };
            format!("admin.tokens[{idx}]")
        };

        if name.trim().is_empty() {
            v.push(
                &path,
                "name must not be empty; it is what §9.3's audit log records as the actor",
            );
        }
        if token.is_empty() {
            v.push(&path, "token must not be empty");
            continue;
        }

        if let Some(previous) = names.insert(name, path.clone()) {
            v.push(
                &path,
                format!("name '{name}' duplicates {previous}; an audit line naming it would be ambiguous"),
            );
        }
        if let Some(previous) = secrets.insert(token, path.clone()) {
            v.push(
                &path,
                format!(
                    "shares its token with {previous}; §9.3 identifies the actor by the \
                     token presented, so two names behind one secret make the audit log a \
                     coin flip"
                ),
            );
        }
    }
}

/// The longest ramp name §4.2 accepts (D-099). It appears in log lines, metric
/// labels and admin paths, so it is kept short and URL-safe.
pub const MAX_RAMP_NAME_CHARS: usize = 64;

/// §3.4's shape, then every per-ramp rule once per ramp.
///
/// The per-ramp checks are the pre-D-099 top-level checks, unchanged: each
/// writes paths relative to its ramp, and they are prefixed `ramps.<name>.`
/// here, so a violation always says which ramp it is in.
fn check_ramps(cfg: &Config, v: &mut ViolationList) {
    if cfg.ramps.is_empty() {
        v.push("ramps", "must declare at least one ramp");
    } else if cfg.ramp(&cfg.default_ramp).is_none() {
        v.push(
            "default_ramp",
            format!(
                "names ramp '{}', which is not declared in ramps (declared: {})",
                cfg.default_ramp,
                cfg.ramps
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    for ramp in cfg.ramps.iter() {
        if let Some(problem) = ramp_name_problem(&ramp.name) {
            v.push(format!("ramps.{}", ramp.name), problem);
        }

        let mut inner = ViolationList::default();
        check_domain_groups(ramp, &mut inner);
        check_route_uniqueness(ramp, &mut inner);
        check_routes(ramp, cfg.server.timeouts.data, &mut inner);
        check_chains(ramp, &mut inner);
        check_default_chain(ramp, &mut inner);
        check_thread_affinity(ramp, &mut inner);
        for violation in inner.0 {
            v.push(
                format!("ramps.{}.{}", ramp.name, violation.path),
                violation.message,
            );
        }
    }
}

/// §4.2 (D-099): 1–64 characters of `[A-Za-z0-9][A-Za-z0-9._-]*`.
fn ramp_name_problem(name: &str) -> Option<String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Some("a ramp name must not be empty".to_string());
    };
    if name.chars().count() > MAX_RAMP_NAME_CHARS {
        return Some(format!(
            "a ramp name must be at most {MAX_RAMP_NAME_CHARS} characters"
        ));
    }
    let valid = first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Some(
            "a ramp name must start with a letter or digit and contain only letters, \
             digits, '.', '_' and '-'"
                .to_string(),
        );
    }
    None
}

fn check_domain_groups(ramp: &Ramp, v: &mut ViolationList) {
    if ramp.domain_groups.is_empty() {
        v.push("domain_groups", "must contain at least the catch-all group");
        return;
    }

    // §4.2: "No domain group contains `*`, or more than one does."
    let catchalls: Vec<&str> = ramp
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
    for (i, g) in ramp.domain_groups.iter().enumerate() {
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
    for g in &ramp.domain_groups {
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

    // D-100: MX suffixes. Same uniqueness rule as domains — first group in
    // configuration order would win, and a config that relies on that is one
    // an operator will misread.
    let mut seen_mx: BTreeMap<String, &str> = BTreeMap::new();
    for g in &ramp.domain_groups {
        if g.is_catchall() && !g.mx.is_empty() {
            v.push(
                format!("domain_groups.{}.mx", g.name),
                "the catch-all group must not list MX suffixes; it is where every \
                 recipient no other group claims already goes",
            );
        }
        for suffix in &g.mx {
            if let Some(problem) = mx_suffix_problem(suffix) {
                v.push(
                    format!("domain_groups.{}.mx", g.name),
                    format!("'{suffix}': {problem}"),
                );
                continue;
            }
            let key = suffix.to_ascii_lowercase();
            match seen_mx.get(&key) {
                Some(first) if *first != g.name.as_str() => v.push(
                    format!("domain_groups.{}.mx", g.name),
                    format!("'{suffix}' also appears in group '{first}'"),
                ),
                Some(_) => v.push(
                    format!("domain_groups.{}.mx", g.name),
                    format!("'{suffix}' is listed twice in the same group"),
                ),
                None => {
                    seen_mx.insert(key, &g.name);
                }
            }
        }
    }
}

/// D-100: an MX suffix is a host name — at least two labels, no wildcard, no
/// leading or trailing dot. A single label (`com`) would claim half the
/// internet's mail for one group.
fn mx_suffix_problem(suffix: &str) -> Option<&'static str> {
    if suffix.is_empty() {
        return Some("must not be empty");
    }
    if suffix.contains('*') {
        return Some("is a suffix, not a pattern; write 'google.com', not '*.google.com'");
    }
    if suffix.starts_with('.') || suffix.ends_with('.') {
        return Some("must not start or end with '.'");
    }
    let labels: Vec<&str> = suffix.split('.').collect();
    if labels.len() < 2 {
        return Some("must have at least two labels, e.g. 'google.com'");
    }
    let label_ok = |l: &&str| {
        !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if !labels.iter().all(label_ok) {
        return Some("must be a host name: letters, digits and '-', separated by '.'");
    }
    None
}

fn check_route_uniqueness(ramp: &Ramp, v: &mut ViolationList) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, r) in ramp.routes.iter().enumerate() {
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

/// D-084: what the build's storage backend needs of the configuration.
///
/// Only rules about the rest of the config live here. Whether `database.url`
/// is for the right backend is checked where the pool is built
/// (`db::postgres::build_pool`, `db::mssql::parse_url`), which still refuses to
/// start and names the image to use instead.
#[cfg(feature = "postgres")]
fn check_storage(_cfg: &Config, _v: &mut ViolationList) {}

#[cfg(feature = "mssql")]
fn check_storage(cfg: &Config, v: &mut ViolationList) {
    // The key columns are NVARCHAR(128): SQL Server caps a clustered key at 900
    // bytes, and (ramp, route, domain_group, day_index) must fit (D-099; it was
    // 200 before the ramp joined the key). A ramp name is already capped at 64
    // for both builds. Postgres has no such limit, so the rule is this build's
    // alone.
    const MAX: usize = crate::db::mssql::MAX_NAME_CHARS;
    for ramp in cfg.ramps.iter() {
        for (i, r) in ramp.routes.iter().enumerate() {
            if r.name.chars().count() > MAX {
                v.push(
                    format!("ramps.{}.routes[{i}].name", ramp.name),
                    format!("is longer than {MAX} characters, the SQL Server build's limit"),
                );
            }
        }
        for (i, g) in ramp.domain_groups.iter().enumerate() {
            if g.name.chars().count() > MAX {
                v.push(
                    format!("ramps.{}.domain_groups[{i}].name", ramp.name),
                    format!("is longer than {MAX} characters, the SQL Server build's limit"),
                );
            }
        }
    }
}

fn check_routes(ramp: &Ramp, client_data: Duration, v: &mut ViolationList) {
    for route in &ramp.routes {
        check_rate(ramp, route, client_data, v);

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
            check_schedule(ramp, route, warmup, v);
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

/// D-111 — every day index at which `per_hour × 24 + burst` is below the cap
/// for a group, one warning per (group, day). Days are checked through the
/// longer of the two schedules; past both ends nothing changes.
fn rate_cap_warnings(ramp: &Ramp, route: &Route) -> Vec<Warning> {
    let (Some(rate), Some(warmup)) = (&route.rate, &route.warmup) else {
        return Vec::new();
    };
    let longest = |default: usize, overrides: &mut dyn Iterator<Item = usize>| {
        overrides.fold(default, usize::max)
    };
    let cap_days = longest(
        warmup.schedule.default.len(),
        &mut warmup.schedule.overrides.values().map(Vec::len),
    );
    let rate_days = rate.schedule.as_ref().map_or(1, |s| {
        longest(s.default.len(), &mut s.overrides.values().map(Vec::len))
    });

    let mut out = Vec::new();
    for group in &ramp.domain_groups {
        for day in 0..cap_days.max(rate_days) as u64 {
            let (Some(cap), Some(per_hour)) = (
                warmup.schedule.allowance_for(&group.name, day),
                rate.per_hour_for(&group.name, day),
            ) else {
                continue;
            };
            let capacity = crate::quota::rate::Rate::new(per_hour, rate.burst).daily_capacity();
            if per_hour >= 1 && rate.burst >= 1 && (capacity as u64) < cap {
                out.push(Warning {
                    path: format!("routes.{}.rate", route.name),
                    message: format!(
                        "in ramp '{}', route '{}', domain group '{}', day {day}: {per_hour}/h × \
                         24 + burst {} = {capacity}, below the day's cap of {cap}, so the cap \
                         cannot be reached that day (D-111)",
                        ramp.name, route.name, group.name, rate.burst
                    ),
                });
            }
        }
    }
    out
}

/// D-111's longest permitted `rate.max_wait`. A held client is a held
/// connection and a held session slot; a minute is already generous.
pub const MAX_RATE_WAIT: Duration = Duration::from_secs(60);

/// §4.2 for D-111's `rate` block. Where a rate-limited route may sit in a chain
/// is [`check_chain`]'s business; the unreachable-cap warning is
/// [`ramp_warnings`]'s.
fn check_rate(ramp: &Ramp, route: &Route, client_data: Duration, v: &mut ViolationList) {
    let Some(rate) = &route.rate else {
        return;
    };
    let at = |suffix: &str| format!("routes.{}.rate.{suffix}", route.name);

    match (&rate.schedule, rate.per_hour) {
        (Some(_), Some(_)) => v.push(
            at("per_hour"),
            "is set alongside rate.schedule; give exactly one — a schedule indexed by the \
             route's day, or a fixed per_hour (D-111)",
        ),
        (None, None) => v.push(
            format!("routes.{}.rate", route.name),
            "needs exactly one of schedule or per_hour (D-111)",
        ),
        _ => {}
    }

    if let Some(n) = rate.per_hour {
        if n < 1 {
            v.push(
                at("per_hour"),
                format!("is {n}; a rate must be at least 1 message per hour. To send nothing, pause the route (§9.3)"),
            );
        }
    }

    if let Some(schedule) = &rate.schedule {
        // D-024: an overflow route has no warm-up and so no day of its own to
        // index a schedule by. A fixed per_hour means the same thing every day.
        if route.overflow {
            v.push(
                at("schedule"),
                "is not allowed on an overflow route, which has no warm-up day to index it \
                 by (D-024); use a fixed rate.per_hour (D-111)",
            );
        }
        check_rate_series(&schedule.default, &at("schedule.default"), v);
        for (group, series) in &schedule.overrides {
            if ramp.domain_group(group).is_none() {
                v.push(
                    at(&format!("schedule.overrides.{group}")),
                    format!("names domain group '{group}', which is not defined"),
                );
            }
            check_rate_series(series, &at(&format!("schedule.overrides.{group}")), v);
        }
    }

    if rate.burst < 1 {
        v.push(
            at("burst"),
            format!("is {}; burst must be at least 1 (D-111)", rate.burst),
        );
    }

    let spooling = ramp.delivery == Delivery::Spool;
    if rate.on_limit == OnLimit::Wait {
        if !spooling {
            v.push(
                at("on_limit"),
                format!(
                    "is `wait`, which needs `delivery: spool` on ramp '{}' — a synchronous \
                     ramp has no later to defer a message to, so its rate limit can only \
                     steer (or hold a client for max_wait); use on_limit: steer (D-118)",
                    ramp.name
                ),
            );
        }
        // D-097's `auto` share exists only because Simmer could not wait, and a
        // fixed share turns away messages a waiting route would have queued.
        // Both at once means two rules disagreeing about the same message.
        if route
            .warmup
            .as_ref()
            .is_some_and(|w| w.schedule.has_partial_ramp())
        {
            v.push(
                at("on_limit"),
                "is `wait` on a route with a warmup.schedule.share; a share turns messages \
                 away that the wait would have deferred. Use one or the other (D-118)",
            );
        }
    }

    let max_wait = rate.max_wait();
    if max_wait > MAX_RATE_WAIT {
        v.push(
            at("max_wait"),
            format!(
                "is {}s; at most {}s. A held client holds a connection and a session slot \
                 (D-111)",
                max_wait.as_secs(),
                MAX_RATE_WAIT.as_secs()
            ),
        );
    }
    // The README's "Timeout budget": the client is held for the wait and then
    // for the whole downstream conversation, and has to hear Simmer's reply
    // before its own data timeout gives up on it.
    //
    // Only a nonzero wait is judged. With max_wait 0 the rate adds no hold at
    // all, and the downstream budget alone is not this rule's business: by this
    // formula the shipped defaults (10 + 30×8 + 120 = 370s) already exceed the
    // default 300s data timeout, which O-21 asks about (D-115).
    let budget = crate::quota::downstream_budget(route, 1);
    // A spooling ramp never holds its client for the downstream, so the budget
    // rule is not its business; `max_wait` there is warned about instead.
    if !spooling && !max_wait.is_zero() && max_wait + budget >= client_data {
        v.push(
            at("max_wait"),
            format!(
                "is {}s, and with the route's downstream budget of {}s the client could be \
                 held {}s — not below server.timeouts.data ({}s), so a client could give up \
                 before Simmer answers it. Lower max_wait or the downstream timeouts (D-111)",
                max_wait.as_secs(),
                budget.as_secs(),
                (max_wait + budget).as_secs(),
                client_data.as_secs()
            ),
        );
    }
}

fn check_rate_series(series: &[i64], path: &str, v: &mut ViolationList) {
    if series.is_empty() {
        v.push(path, "must not be empty");
        return;
    }
    for (i, value) in series.iter().enumerate() {
        if *value < 1 {
            v.push(
                format!("{path}[{i}]"),
                format!(
                    "is {value}; a rate must be at least 1 message per hour. To send nothing, \
                     pause the route (§9.3)"
                ),
            );
        }
    }
}

fn check_schedule(ramp: &Ramp, route: &Route, warmup: &super::Warmup, v: &mut ViolationList) {
    let at = |suffix: &str| format!("routes.{}.warmup.schedule.{suffix}", route.name);

    // §4.2: "A warmup.schedule array is empty, or contains a negative value."
    check_series(&warmup.schedule.default, &at("default"), v);

    for (group, series) in &warmup.schedule.overrides {
        // §4.2: "An overrides key names a nonexistent domain group."
        if ramp.domain_group(group).is_none() {
            v.push(
                at(&format!("overrides.{group}")),
                format!("names domain group '{group}', which is not defined"),
            );
        }
        check_series(series, &at(&format!("overrides.{group}")), v);
    }

    check_share(&warmup.schedule.share, &at("share"), v);
}

/// §4.2 for D-091's `schedule.share` and D-097's `auto`. Where a route carrying
/// one may sit in a chain is [`check_chain`]'s business.
fn check_share(share: &ShareSchedule, path: &str, v: &mut ViolationList) {
    match share {
        ShareSchedule::Days(days) => {
            for (i, value) in days.iter().enumerate() {
                check_fraction(*value, &format!("{path}[{i}]"), v);
            }
        }
        ShareSchedule::Auto(auto) => check_auto_share(auto, path, v),
    }
}

/// Every D-097 parameter, reported together as §4.2 requires.
fn check_auto_share(auto: &AutoShare, path: &str, v: &mut ViolationList) {
    let at = |suffix: &str| format!("{path}.{suffix}");

    check_fraction(auto.floor, &at("floor"), v);
    check_fraction(auto.ceiling, &at("ceiling"), v);
    check_fraction(auto.fill_by, &at("fill_by"), v);
    check_fraction(auto.tail.ceiling, &at("tail.ceiling"), v);

    // The release threshold alone may be zero, which disables it. It may not be
    // 1: releasing an empty cap is no ramp at all, and §4.2 would rather say so
    // than let a route look ramped while offering everything.
    if !(auto.tail.below >= 0.0 && auto.tail.below < 1.0) {
        v.push(
            at("tail.below"),
            format!(
                "is {}; the tail release threshold must be at least 0 and below 1. Use 0 to \
                 disable the release (§4.2, D-097)",
                auto.tail.below
            ),
        );
    }

    if !(auto.gain > 0.0 && auto.gain.is_finite()) {
        v.push(
            at("gain"),
            format!(
                "is {}; the gain must be above 0 and finite. 1 is proportional; higher \
                 throttles harder when the route runs ahead of pace (§4.2, D-097)",
                auto.gain
            ),
        );
    }

    // Both ends have already been reported as fractions if either is unusable;
    // this is the one rule about the pair, and NaN fails it silently there.
    if auto.floor > auto.ceiling {
        v.push(
            at("floor"),
            format!(
                "is {}, above the ceiling of {}; the share would have no value it could \
                 take (§4.2, D-097)",
                auto.floor, auto.ceiling
            ),
        );
    }
}

/// A share-shaped number: above 0 and at most 1. Written so that NaN fails too.
fn check_fraction(value: f64, path: &str, v: &mut ViolationList) {
    if !(value > 0.0 && value <= 1.0) {
        v.push(
            path,
            format!(
                "is {value}; a share must be above 0 and at most 1. To offer the route \
                 nothing, pause it (§9.3)"
            ),
        );
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
    } else if crate::preflight::literal_domain(&identity.envelope_from).is_none() {
        // §4.2, added after implementation — D-069.
        //
        // Stronger than the §6.6 stability rule below and separate from it:
        // `bounce@{{original.envelope_from.domain}}` is perfectly stable, since
        // applying it twice gives the same answer, and is nonetheless incoherent.
        // The ramp, the daily allowance and reputation accrual all exist to build
        // reputation for ONE domain. A route whose domain varies per message warms
        // nothing, and its `quota_usage` row counts a mixture of domains under a
        // single label — the ledger reads as a healthy ramp while no domain is
        // actually being warmed.
        //
        // Until this rule existed, §6.7 was the only thing that noticed: it
        // declined to preflight such a route and warned (D-064). That warning is
        // now unreachable through a valid configuration, and the check has been
        // promoted from "we cannot check this" to "this is not a route".
        v.push(
            at("envelope_from"),
            format!(
                "has a domain that is not a literal: '{}'. A warming route builds reputation \
                 for one domain, so its outbound domain cannot vary per message — the ramp, \
                 the daily allowance and the quota row are all keyed on the assumption that \
                 it does not. Template the local part if you need to; the domain must be a \
                 constant (§4.2, D-069)",
                identity.envelope_from
            ),
        );
    }

    // §4.2's "a body_rewrites.pattern fails to compile" is reported by
    // `RouteRewrite::compile` below, alongside the template parse errors — one
    // compile, one source of truth for what an `identity` block has to satisfy
    // before the stability checks can mean anything.

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
    // D-089: an identity field is not rewritable by `header_rewrites`, with no
    // override — the treatment `envelope_from` gets. §6.6 routes the override
    // for every other header through `unstable_headers`, and naming an identity
    // field there is refused just above, so there is no declaration that could
    // admit one. A regex over `From:` is a relative transformation of the
    // reputation-bearing field — the output is a function of the value it
    // overwrites — which is §1.1 constraint 1 by name. `set_headers` is how an
    // identity field is assigned.
    for (i, rule) in identity.header_rewrites.iter().enumerate() {
        if is_identity_header(&rule.header) {
            v.push(
                at(&format!("header_rewrites[{i}].header")),
                format!(
                    "names identity field '{}'. Identity fields determine which domain accrues \
                     reputation and must be absolute assignments: set it with set_headers. This \
                     is not overridable, and unstable_headers cannot name an identity field \
                     (§1.1, §6.6, D-089)",
                    rule.header
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
            // §6.6 in the body (D-046). A `body_rewrites` entry whose pattern
            // matches its own replacement grows the message on every pass, which
            // is §1.1 constraint 1 — "append `.new` to the sending domain is not
            // permitted, because applying it to already-migrated traffic
            // corrupts it" — in a different place. There is no `unstable_headers`
            // for it because it is not a header, and there is no migration-only
            // reading of it the way there is for `Reply-To`, so it is fatal.
            if let Some((once, twice)) = compiled.body_rewrites.fixed_point_violation() {
                v.push(
                    at("body_rewrites"),
                    format!(
                        "are not stable: applying them to their own output changes it again \
                         ({once:?} then {twice:?}). A rule that matches what it just wrote \
                         corrupts a message every time it passes through, and would corrupt \
                         traffic from an application that has already been cut over (§1.1, §6.6)"
                    ),
                );
            }

            // D-089 — the same question of `header_rewrites`, one header at a
            // time, and fatal for D-046's reason: nobody writes a regex meaning
            // it to apply twice, so there is nothing for unstable_headers to
            // acknowledge.
            let mut unstable_rewrites: Vec<String> = Vec::new();
            for (header, once, twice) in compiled.header_rewrites.fixed_point_violations() {
                v.push(
                    at("header_rewrites"),
                    format!(
                        "for '{header}' are not stable: applying them to their own output \
                         changes it again ({once:?} then {twice:?}). A rule that matches what it \
                         just wrote corrupts the header every time it passes through, and would \
                         corrupt traffic from an application that has already been cut over. \
                         Not overridable (§1.1, §6.6, D-089)"
                    ),
                );
                unstable_rewrites.push(header.to_ascii_lowercase());
            }

            let report = stability::probe(&compiled);

            // D-089 — "a rewrite producing a non-RFC-5322-conformant value must
            // fail validation, not emit a malformed header". The replacement's
            // literal text is checked at compile; this is the engine declining
            // to emit its result for the probe.
            for (header, reason) in &report.skipped_headers {
                v.push(
                    at("header_rewrites"),
                    format!(
                        "for '{header}' cannot be applied to the startup probe: {} (D-089)",
                        reason.describe()
                    ),
                );
            }

            for u in report.unstable {
                let from_rewrite = compiled.header_rewrites.names(&u.field)
                    && !identity.set_headers.contains_key(&u.field);
                if from_rewrite {
                    if !unstable_rewrites.contains(&u.field.to_ascii_lowercase()) {
                        v.push(
                            at("header_rewrites"),
                            format!(
                                "for '{}' are not stable: rewriting twice gives '{}' then '{}'. \
                                 Not overridable (§1.1, §6.6, D-089)",
                                u.field, u.first, u.second
                            ),
                        );
                    }
                } else if u.identity {
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

fn check_chains(ramp: &Ramp, v: &mut ViolationList) {
    for (i, rule) in ramp.senders.iter().enumerate() {
        let path = format!("senders[{i}] (match '{}')", rule.pattern);
        if rule.pattern.trim().is_empty() {
            v.push(&path, "match must not be empty");
        }
        check_chain(ramp, &rule.chain, &path, v);
    }
}

/// §3.1: "Zero or more warming routes, followed by **at most one** overflow
/// route. It must be last in any chain containing it."
fn check_chain(ramp: &Ramp, chain: &[String], path: &str, v: &mut ViolationList) {
    if chain.is_empty() {
        v.push(path, "chain must not be empty");
        return;
    }

    let mut overflow_positions = Vec::new();

    for (i, name) in chain.iter().enumerate() {
        // §4.2: "A referenced route name does not exist."
        let Some(route) = ramp.route(name) else {
            v.push(
                path,
                format!("chain[{i}] references route '{name}', which is not defined"),
            );
            continue;
        };
        if route.overflow {
            overflow_positions.push(i);
        }

        // D-091: a partially-ramped route turns messages away while it still
        // has headroom. Last in a chain, each of those is §10.3's `451` by
        // design, which is an outage dressed up as a ramp. D-097's `auto` is
        // the same rule for the same reason — more so, since it has no end.
        let partial = route
            .warmup
            .as_ref()
            .is_some_and(|w| w.schedule.has_partial_ramp());
        // D-111, for D-091's reason: a rate-limited route turns messages away
        // while it still has headroom, and last in a chain each is a `451`.
        // D-118: on a spooling ramp a waiting route does not turn messages away
        // inside their hold; it defers them. Past the hold the message expires
        // into the dead-letter list rather than becoming a 451, so last in the
        // chain is exactly where "pace this, nothing else" puts it.
        let defers = ramp.delivery == Delivery::Spool
            && route
                .rate
                .as_ref()
                .is_some_and(|r| r.on_limit == OnLimit::Wait);
        if route.rate.is_some() && !defers && i == chain.len() - 1 {
            v.push(
                path,
                format!(
                    "route '{name}' has a rate limit and is last in the chain, so every \
                     message it turns away for its rate would be answered 451. Put another \
                     route after it (§4.2, D-111)"
                ),
            );
        }
        if partial && i == chain.len() - 1 {
            v.push(
                path,
                format!(
                    "route '{name}' has a warmup.schedule.share and is last in the chain, so \
                     the messages it is not offered would have nowhere to go. Put another \
                     route after it (§4.2, D-091)"
                ),
            );
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

/// §4.2 for §3.2 step 2a (D-090): with `thread_affinity` on, an emitted
/// `Message-ID:` has to name the route that emitted it, in every chain a
/// message can walk.
///
/// Both failures are refusals rather than warnings because both are silent
/// otherwise. A route whose ID names no route is never pinned, and a reply to
/// its mail takes the ordinary walk with nothing to say it did; two routes in
/// one chain sharing a domain make the pin pick whichever is listed first,
/// which is right for one of them. Memory of which it was is exactly the state
/// D-090 chose not to keep.
fn check_thread_affinity(ramp: &Ramp, v: &mut ViolationList) {
    if !ramp.thread_affinity {
        return;
    }

    let mut chains: Vec<(String, &[String])> = ramp
        .senders
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            (
                format!("senders[{i}] (match '{}')", rule.pattern),
                rule.chain.as_slice(),
            )
        })
        .collect();
    if let Some(chain) = &ramp.default_chain {
        chains.push(("default_chain".to_string(), chain.as_slice()));
    }

    let mut reported = std::collections::BTreeSet::new();
    for (path, chain) in chains {
        let mut seen: Vec<(String, &str)> = Vec::new();
        for name in chain {
            // A dangling name is check_chain's to report.
            let Some(route) = ramp.route(name) else {
                continue;
            };
            match crate::routing::thread::route_domain(&route.identity) {
                None => {
                    if reported.insert(name.as_str()) {
                        v.push(
                            format!("routes.{name}.identity.set_headers.Message-ID"),
                            "must be set, with a literal domain, when thread_affinity is on: \
                             a reply is recognised by the domain of the Message-ID this route \
                             emitted, and a route whose IDs carry no domain of its own can \
                             never be pinned (§4.2, D-090)",
                        );
                    }
                }
                Some(domain) => match seen.iter().find(|(d, _)| *d == domain) {
                    Some((_, other)) if *other != name.as_str() => v.push(
                        &path,
                        format!(
                            "routes '{other}' and '{name}' both emit Message-IDs at '{domain}', \
                             so with thread_affinity on a reply to either would be pinned to \
                             '{other}'. Give each route in a chain its own Message-ID domain \
                             (§4.2, D-090)"
                        ),
                    ),
                    Some(_) => {}
                    None => seen.push((domain, name.as_str())),
                },
            }
        }
    }
}

fn check_default_chain(ramp: &Ramp, v: &mut ViolationList) {
    // §4.2: "strict_senders: false and default_chain is absent or its final
    // route is not an overflow route."
    if ramp.strict_senders {
        // A default_chain is harmless but pointless under strict_senders; still
        // validate it if present, so turning strict_senders off later does not
        // surface new errors.
        if let Some(chain) = &ramp.default_chain {
            check_chain(ramp, chain, "default_chain", v);
        }
        return;
    }

    let Some(chain) = &ramp.default_chain else {
        v.push(
            "default_chain",
            "is required when strict_senders is false: an unmatched sender has nowhere to go",
        );
        return;
    };

    check_chain(ramp, chain, "default_chain", v);

    match chain.last().and_then(|name| ramp.route(name)) {
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
