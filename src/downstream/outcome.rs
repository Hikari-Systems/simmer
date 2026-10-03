//! §10.1 — mapping a downstream outcome to a client reply, as a pure function.
//!
//! This is the table `SPEC.md` §12.3 asks to be asserted exhaustively, so it is
//! written as data rather than as control flow scattered through the relay.
//!
//! It implements §10.1 **as amended by `DECISIONS.md` D-008**: a downstream `5xx`
//! maps to `550` only at `RCPT TO`. Everywhere else it becomes `451`, because the
//! likeliest `5xx` in this system is the downstream rejecting our *rewritten*
//! envelope sender for want of provider domain authentication — the §6.5
//! provisioning risk — and telling a client that a perfectly deliverable
//! recipient is permanently bad is exactly what §14.1 forbids.

use crate::metrics;
use crate::smtp::reply::{self, Reply};

/// Where in the downstream conversation something happened.
///
/// The stage is what D-008 keys on, so it is carried through every error rather
/// than flattened away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Connect,
    Greeting,
    Ehlo,
    StartTls,
    Auth,
    MailFrom,
    RcptTo,
    Data,
    /// The `.` that ends `DATA`. The only stage whose `2xx` means "delivered".
    FinalDot,
    Quit,
    /// §8.3's own traffic on a pooled connection: the `NOOP` that validates one
    /// idle beyond the threshold, and the `RSET` between messages.
    ///
    /// Never reaches [`failed`]: a failure here means the pool discards the
    /// connection and opens another, which is the whole point of validating. It
    /// exists so that when it is logged it says what it was.
    Keepalive,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Connect => "connect",
            Stage::Greeting => "greeting",
            Stage::Ehlo => "ehlo",
            Stage::StartTls => "starttls",
            Stage::Auth => "auth",
            Stage::MailFrom => "mail_from",
            Stage::RcptTo => "rcpt_to",
            Stage::Data => "data",
            Stage::FinalDot => "final_dot",
            Stage::Quit => "quit",
            Stage::Keepalive => "keepalive",
        }
    }

    /// Stages where a `5xx` means Simmer's own configuration is wrong rather
    /// than the message or the recipient being wrong (D-008).
    ///
    /// `MailFrom` is the §6.5 case. `Ehlo` and `Auth` are the same class — a
    /// downstream refusing our credentials is not a fact about the recipient.
    /// `Data`/`FinalDot` are content or policy rejections that a client cannot
    /// act on usefully as a permanent recipient verdict.
    fn is_config_error_on_5xx(self) -> bool {
        matches!(
            self,
            Stage::Ehlo | Stage::Auth | Stage::MailFrom | Stage::Data | Stage::FinalDot
        )
    }
}

/// Everything that can go wrong on the outbound leg.
#[derive(Debug)]
pub enum RelayError {
    /// TCP connect failed or the greeting never arrived.
    Connect(String),
    /// §8.2 — `STARTTLS` refused, or the handshake failed under `required` /
    /// `required_verify`.
    Tls(String),
    Timeout(Stage),
    /// An unparseable reply, an unexpected continuation, or EOF mid-conversation.
    Protocol(Stage, String),
    /// The downstream said no, in as many words.
    Rejected {
        stage: Stage,
        code: u16,
        text: String,
    },
    /// §10.2 — the connection dropped after the terminating dot was written but
    /// before a reply was read. The message may or may not have been delivered.
    Ambiguous,
    /// D-018 — the downstream does not advertise a capability the client used and
    /// configuration claimed was available.
    MissingCapability(&'static str),
    /// §8.3 — every one of the route's `max_connections` was in use for longer
    /// than the connect budget allowed us to wait.
    ///
    /// Not a downstream failure: the downstream was never reached. It is what
    /// "the pool bounds concurrency against each downstream" costs when the
    /// bound binds, and it is the reason the pool is not just a socket cache.
    PoolExhausted,
}

impl RelayError {
    /// The `class` label `simmer_downstream_errors_total` gives this error, and
    /// the `outcome` §9.6's downstream span records (D-126). A `Rejected`
    /// splits by code family, as [`failed`] does.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Connect(_) => "connect",
            Self::Tls(_) => "tls",
            Self::Timeout(_) => "timeout",
            Self::Protocol(..) => "protocol",
            Self::Ambiguous => "ambiguous",
            Self::MissingCapability(_) => "capability",
            Self::PoolExhausted => "pool_exhausted",
            Self::Rejected { code, .. } if *code >= 500 => "rejected",
            Self::Rejected { .. } => "deferred",
        }
    }

    /// The conversation stage it happened at, where there is one.
    pub fn stage(&self) -> Option<Stage> {
        match self {
            Self::Timeout(s) | Self::Protocol(s, _) | Self::Rejected { stage: s, .. } => Some(*s),
            _ => None,
        }
    }

    /// The downstream's reply code, when it gave one. Never its text, which can
    /// quote the recipient back (§9.5).
    pub fn code(&self) -> Option<u16> {
        match self {
            Self::Rejected { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// A downstream `2xx` on the final dot.
#[derive(Debug, Clone)]
pub struct Delivered {
    pub code: u16,
    pub text: String,
}

/// The full §10.1 outcome: what to tell the client, and what it means for the
/// reservation phase 3 will be holding.
#[derive(Debug)]
pub struct Outcome {
    pub reply: Reply,
    /// §7.4 phase 3. `true` only for a `2xx` on the final dot; every other row of
    /// §10.1 releases. Carried now so the reservation protocol slots in without
    /// revisiting this table.
    pub commit: bool,
    pub result: metrics::MessageResult,
}

/// §10.1 row 1 — the only success.
pub fn delivered(_route: &str, d: &Delivered) -> Outcome {
    debug_assert!((200..300).contains(&d.code));
    Outcome {
        reply: reply::accepted(),
        commit: true,
        result: metrics::MessageResult::Delivered,
    }
}

/// Map a failure to a client reply, emitting the counters and logs each row owes.
///
/// Takes `route` because every counter in §9.1 is labelled by it, and because
/// D-008's `ERROR` log is the only thing that will ever reveal a §6.5
/// misconfiguration.
pub fn failed(ramp: &str, route: &str, err: &RelayError) -> Outcome {
    let reply = match err {
        RelayError::Connect(detail) => {
            tracing::warn!(route, detail, "downstream connect failed");
            metrics::downstream_error(ramp, route, "connect");
            reply::Reply::new(451, "4.4.1 downstream unavailable")
        }

        RelayError::Tls(detail) => {
            tracing::warn!(route, detail, "downstream TLS negotiation failed");
            metrics::downstream_error(ramp, route, "tls");
            reply::Reply::new(451, "4.7.0 downstream TLS failure")
        }

        RelayError::Timeout(stage) => {
            tracing::warn!(route, stage = stage.as_str(), "downstream timeout");
            metrics::downstream_error(ramp, route, "timeout");
            reply::Reply::new(451, "4.4.2 downstream timeout")
        }

        RelayError::Protocol(stage, detail) => {
            tracing::warn!(
                route,
                stage = stage.as_str(),
                detail = %redact_addresses(detail),
                "downstream protocol violation"
            );
            metrics::downstream_error(ramp, route, "protocol");
            reply::Reply::new(451, "4.3.0 downstream protocol error")
        }

        // §10.2. A duplicate on client retry is deliberately preferred to a
        // silent loss, and to a `250` for a delivery Simmer cannot vouch for.
        RelayError::Ambiguous => {
            tracing::warn!(
                route,
                "connection dropped after the terminating dot; delivery is unknown"
            );
            metrics::downstream_error(ramp, route, "ambiguous");
            metrics::ambiguous_delivery();
            reply::Reply::new(451, "4.3.0 downstream reply not received, delivery unknown")
        }

        RelayError::MissingCapability(cap) => {
            // Configuration promised something the downstream does not offer.
            // Same family as D-008: loud, permanent-in-practice, and invisible
            // in the mail flow without this.
            tracing::error!(
                route,
                capability = cap,
                "downstream does not advertise a capability this route's \
                 configuration claims; messages needing it cannot be relayed"
            );
            metrics::downstream_config_error(ramp, route, "capability");
            metrics::downstream_error(ramp, route, "capability");
            reply::Reply::new(451, "4.3.5 downstream capability mismatch")
        }

        // §8.3. Deliberately its own class rather than folded into `connect`:
        // an exhausted pool is Simmer declining to open a fifth connection, not
        // a downstream that would not accept one, and the two call for opposite
        // responses — raise `max_connections`, or go and look at the provider.
        RelayError::PoolExhausted => {
            tracing::warn!(
                route,
                "every pooled connection to this downstream was busy for the whole \
                 connect budget; raise downstream.pool.max_connections if this persists"
            );
            metrics::downstream_error(ramp, route, "pool_exhausted");
            reply::Reply::new(451, "4.4.5 downstream connection pool exhausted")
        }

        // -- the code-bearing rows -------------------------------------
        RelayError::Rejected { stage, code, text } if *code >= 500 => {
            if *stage == Stage::RcptTo {
                // The one place a 5xx is genuinely about the recipient, so the
                // one place §14.1 permits a 550 (D-008).
                tracing::info!(
                    route,
                    stage = stage.as_str(),
                    code,
                    text = %redact_addresses(text),
                    "downstream rejected the recipient"
                );
                metrics::downstream_error(ramp, route, "rejected");
                reply::with_downstream(550, "5.0.0 rejected by downstream", *code, text)
            } else {
                if stage.is_config_error_on_5xx() {
                    // The §6.5 alarm. This is the line that catches a route
                    // ramping flawlessly for weeks while building no reputation.
                    tracing::error!(
                        route,
                        stage = stage.as_str(),
                        code,
                        text = %redact_addresses(text),
                        "downstream returned 5xx at a stage that indicates a \
                         configuration fault, not a recipient fault; returning \
                         451 so the client does not suppress the recipient"
                    );
                    metrics::downstream_config_error(ramp, route, stage.as_str());
                } else {
                    tracing::warn!(
                        route,
                        stage = stage.as_str(),
                        code,
                        text = %redact_addresses(text),
                        "downstream 5xx"
                    );
                }
                metrics::downstream_error(ramp, route, "rejected");
                reply::with_downstream(451, "4.0.0 deferred by downstream", *code, text)
            }
        }

        RelayError::Rejected { stage, code, text } => {
            tracing::info!(
                route,
                stage = stage.as_str(),
                code,
                text = %redact_addresses(text),
                "downstream 4xx"
            );
            metrics::downstream_error(ramp, route, "deferred");
            reply::with_downstream(451, "4.0.0 deferred by downstream", *code, text)
        }
    };

    let result = if reply.code >= 500 {
        metrics::MessageResult::Rejected
    } else {
        metrics::MessageResult::Deferred
    };

    Outcome {
        reply,
        // §10.1: every failure row releases the reservation. No exceptions —
        // including the ambiguous one, per §10.2.
        commit: false,
        result,
    }
}

/// The downstream's reply text as it may be **logged**: every address's local
/// part replaced by `*`, the domain kept.
///
/// §9.5 allows recipient addresses only at `DEBUG`, and a downstream's reply
/// routinely quotes the recipient back — `550 5.1.1 <jane@example.com> unknown
/// user` — so logging its text verbatim at `INFO` put the address in every
/// log shipper and, since D-126, every telemetry backend. The domain stays: it
/// is what an operator needs to tell one provider's refusals from another's,
/// and domains are logged elsewhere at `WARN` already. The client's reply is
/// not this: §10.1 quotes the text back to the client that sent the address,
/// which is not a disclosure.
pub fn redact_addresses(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices('@') {
        // RFC 5322 atext plus the dot, walked backwards from the `@`.
        let start = bytes[..at]
            .iter()
            .rposition(|&b| !(b.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~.-".contains(&b)))
            .map_or(0, |p| p + 1);
        if start < at && start >= copied {
            out.push_str(&text[copied..start]);
            out.push('*');
            copied = at;
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_reply_text_keeps_the_domain_and_loses_the_local_part() {
        assert_eq!(
            redact_addresses("5.1.1 <jane.doe+x@example.com> unknown user"),
            "5.1.1 <*@example.com> unknown user"
        );
        assert_eq!(
            redact_addresses("a@b.com and c@d.com"),
            "*@b.com and *@d.com"
        );
        assert_eq!(redact_addresses("4.2.2 mailbox full"), "4.2.2 mailbox full");
        assert_eq!(redact_addresses("lone @ sign"), "lone @ sign");
        assert_eq!(redact_addresses("@start"), "@start");
    }

    fn rejected(stage: Stage, code: u16) -> RelayError {
        RelayError::Rejected {
            stage,
            code,
            text: "downstream detail".into(),
        }
    }

    // Includes `Keepalive`, which the pool swallows and `failed` should never
    // see. It is here so that if it ever does reach the table, §14.1's assertion
    // below covers it rather than discovering it on somebody's suppression list.
    const ALL_STAGES: [Stage; 11] = [
        Stage::Connect,
        Stage::Greeting,
        Stage::Ehlo,
        Stage::StartTls,
        Stage::Auth,
        Stage::MailFrom,
        Stage::RcptTo,
        Stage::Data,
        Stage::FinalDot,
        Stage::Quit,
        Stage::Keepalive,
    ];

    // -- §10.1 as written -------------------------------------------------

    #[test]
    fn a_2xx_on_the_final_dot_is_the_only_commit() {
        let o = delivered(
            "r",
            &Delivered {
                code: 250,
                text: "ok".into(),
            },
        );
        assert_eq!(o.reply.code, 250);
        assert!(o.commit);
        assert_eq!(o.result, metrics::MessageResult::Delivered);
    }

    #[test]
    fn connect_failure_is_451_4_4_1() {
        let o = failed("main", "r", &RelayError::Connect("refused".into()));
        assert_eq!(o.reply.to_wire(), "451 4.4.1 downstream unavailable\r\n");
        assert!(!o.commit);
    }

    #[test]
    fn tls_failure_is_451_4_7_0() {
        let o = failed("main", "r", &RelayError::Tls("handshake".into()));
        assert_eq!(o.reply.to_wire(), "451 4.7.0 downstream TLS failure\r\n");
    }

    #[test]
    fn a_timeout_at_any_stage_is_451_4_4_2() {
        for stage in ALL_STAGES {
            let o = failed("main", "r", &RelayError::Timeout(stage));
            assert_eq!(
                o.reply.to_wire(),
                "451 4.4.2 downstream timeout\r\n",
                "{stage:?}"
            );
            assert!(!o.commit);
        }
    }

    #[test]
    fn a_protocol_violation_at_any_stage_is_451_4_3_0() {
        for stage in ALL_STAGES {
            let o = failed("main", "r", &RelayError::Protocol(stage, "garbage".into()));
            assert_eq!(
                o.reply.to_wire(),
                "451 4.3.0 downstream protocol error\r\n",
                "{stage:?}"
            );
        }
    }

    #[test]
    fn a_4xx_at_any_stage_is_451_with_the_downstream_text_appended() {
        for stage in ALL_STAGES {
            for code in [421, 450, 451, 452, 454, 471] {
                let o = failed("main", "r", &rejected(stage, code));
                assert_eq!(o.reply.code, 451, "{stage:?} {code}");
                assert!(
                    o.reply
                        .to_wire()
                        .contains(&format!("{code} downstream detail")),
                    "{stage:?} {code}: {}",
                    o.reply
                );
                assert!(!o.commit);
                assert_eq!(o.result, metrics::MessageResult::Deferred);
            }
        }
    }

    // -- D-008: the 5xx split --------------------------------------------

    #[test]
    fn a_5xx_at_rcpt_to_is_550_because_it_is_genuinely_about_the_recipient() {
        for code in [550, 551, 552, 553, 554] {
            let o = failed("main", "r", &rejected(Stage::RcptTo, code));
            assert_eq!(o.reply.code, 550, "{code}");
            assert!(o
                .reply
                .to_wire()
                .contains(&format!("{code} downstream detail")));
            assert_eq!(o.result, metrics::MessageResult::Rejected);
            assert!(!o.commit);
        }
    }

    #[test]
    fn a_5xx_anywhere_else_is_451_so_the_client_cannot_suppress_the_recipient() {
        // The whole point of D-008. The likeliest 5xx in this system is the
        // downstream rejecting our rewritten envelope sender because provider
        // domain authentication is not finished (§6.5) — a 550 for that would
        // permanently suppress a deliverable recipient in systems that outlive
        // Simmer by years (§14.1).
        for stage in ALL_STAGES {
            if stage == Stage::RcptTo {
                continue;
            }
            let o = failed("main", "r", &rejected(stage, 550));
            assert_eq!(o.reply.code, 451, "{stage:?} must not be 550");
            assert!(o.reply.to_wire().contains("550 downstream detail"));
            assert_eq!(o.result, metrics::MessageResult::Deferred);
        }
    }

    #[test]
    fn every_stage_and_class_produces_a_reply_and_never_commits() {
        // The exhaustive assertion §12.3 asks for: no combination falls through.
        for stage in ALL_STAGES {
            for code in [211, 250, 354, 421, 450, 500, 550, 554] {
                let o = failed("main", "r", &rejected(stage, code));
                assert!(
                    o.reply.code == 451 || o.reply.code == 550,
                    "{stage:?} {code} produced {}",
                    o.reply.code
                );
                assert!(!o.commit, "{stage:?} {code} must not commit");
            }
        }
    }

    // -- §10.2 ------------------------------------------------------------

    #[test]
    fn the_ambiguous_final_dot_is_451_and_releases() {
        let o = failed("main", "r", &RelayError::Ambiguous);
        assert_eq!(o.reply.code, 451);
        // Never 250: §10.2 is explicit that Simmer must not vouch for a delivery
        // it cannot confirm, even at the cost of a duplicate on retry.
        assert!(!o.commit);
        assert_eq!(o.result, metrics::MessageResult::Deferred);
    }

    #[test]
    fn an_exhausted_pool_is_451_and_its_own_class() {
        // §8.3. Distinct from `connect` on purpose: one says raise
        // `max_connections`, the other says go and look at the provider, and a
        // dashboard that conflates them sends somebody to the wrong place.
        let o = failed("main", "r", &RelayError::PoolExhausted);
        assert_eq!(o.reply.code, 451);
        assert!(o.reply.to_wire().contains("4.4.5"));
        assert!(!o.commit);
        assert_eq!(o.result, metrics::MessageResult::Deferred);
    }

    #[test]
    fn a_missing_capability_is_451_not_550() {
        let o = failed("main", "r", &RelayError::MissingCapability("SMTPUTF8"));
        assert_eq!(o.reply.code, 451);
        assert!(!o.commit);
    }

    // -- the §14.1 invariant, stated once --------------------------------

    #[test]
    fn the_only_permanent_client_reply_in_the_whole_table_is_a_5xx_at_rcpt_to() {
        let mut permanent = Vec::new();
        for stage in ALL_STAGES {
            for code in [250, 421, 450, 452, 500, 550, 554] {
                let o = failed("main", "r", &rejected(stage, code));
                if o.reply.code >= 500 {
                    permanent.push((stage, code));
                }
            }
            for err in [
                RelayError::Connect("x".into()),
                RelayError::Tls("x".into()),
                RelayError::Timeout(stage),
                RelayError::Protocol(stage, "x".into()),
                RelayError::Ambiguous,
                RelayError::MissingCapability("X"),
                RelayError::PoolExhausted,
            ] {
                assert!(
                    failed("main", "r", &err).reply.code < 500,
                    "{err:?} at {stage:?} must not be permanent"
                );
            }
        }

        assert!(
            permanent
                .iter()
                .all(|(stage, code)| *stage == Stage::RcptTo && *code >= 500),
            "a permanent reply escaped the RCPT TO carve-out: {permanent:?}"
        );
        assert!(!permanent.is_empty(), "RCPT TO 5xx must still be permanent");
    }
}
