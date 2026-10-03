//! §9.4 — `POST /dryrun`.
//!
//! > accepts an envelope sender, a `From:` header value, and a recipient list,
//! > and returns the routing decision — matched sender rule, chain evaluation
//! > with a per-route skip reason, selected route, resolved outbound envelope
//! > and headers after template rendering, and body rewrite matches against a
//! > supplied sample body. It sends nothing and takes no reservation.
//! >
//! > This is the primary tool for validating a configuration before it carries
//! > live traffic, and should be treated as a first-class feature rather than a
//! > debugging afterthought.
//!
//! Treating it as first-class means one thing above all: **it runs the real
//! code**. The sender match is `relay::resolve_chain`, the chain walk is
//! `chain::dry_walk` (which `tests/admin_api.rs` pins against
//! `walk_and_reserve` step for step), and the rewrite is `rewrite::rewrite`
//! itself with the route's compiled templates. A second implementation would
//! answer questions about itself.
//!
//! ## What it must not do
//!
//! Send anything, write anything, or reserve anything. It reads `route_state`,
//! `quota_usage` and `recipient_event` and writes none of them — the one write
//! anywhere near this path is §7.3's salt, which is get-or-insert and would have
//! happened on the first message anyway (D-050).
//!
//! It must also not *count* anything: `dry_walk` deliberately does not increment
//! `simmer_route_skipped_total`, because that series measures messages and an
//! operator testing a configuration has sent none.
//!
//! ## Recipients
//!
//! The request carries plaintext addresses, which is the one place in the
//! control plane that happens. They are supplied by the operator, evaluated, and
//! never stored — §7.3's constraint is on what the *container accumulates*, and
//! this accumulates nothing. They are not logged either: §9.5 puts recipient
//! addresses at `DEBUG` and there is no reason for this path to be louder.
//!
//! A real transaction carries exactly one recipient (D-047). §9.4 asks for a
//! list, so a list is accepted and each address is evaluated independently — one
//! call answers "what happens to these five people", which is what an operator
//! validating a configuration actually wants to know.

use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::auth::Actor;
use super::error::ApiError;
use super::AdminState;
use crate::relay;
use crate::rewrite;
use crate::routing::chain::{self, SkipReason};
use crate::routing::sender_match::{self, Senders};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRunRequest {
    /// `null` or absent is the null sender, `<>` — which §6.2 never rewrites, so
    /// a bounce stays a bounce (D-035). Worth testing deliberately.
    #[serde(default)]
    pub envelope_from: Option<String>,
    /// The `From:` header value, as it would arrive: either a bare address or a
    /// full `Display Name <addr>`.
    #[serde(default)]
    pub from_header: Option<String>,
    pub recipients: Vec<String>,
    /// A complete RFC 5322 message, headers and all. Wins over `body` and
    /// `subject` when supplied, and is the only way to dry-run a MIME structure.
    #[serde(default)]
    pub message: Option<String>,
    /// §9.4's "supplied sample body", for the ordinary case where an operator
    /// has a paragraph of text and a link in it rather than a whole message.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    /// §3.2 step 2a (D-090) — `In-Reply-To:` and `References:` values for the
    /// synthesised message, so an operator can ask "where would a reply to this
    /// go". Ignored when `message` is supplied: its own headers are what the
    /// relay would read.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<String>,
    /// Return the rewritten message in full. Off by default: the useful answer
    /// is almost always the headers and which patterns fired.
    #[serde(default)]
    pub include_message: bool,
    /// §9.4 (D-099) — the ramp to route in. Or leave it out and give what
    /// §5.8 selects from, and the same function the session calls chooses it:
    #[serde(default)]
    pub ramp: Option<String>,
    /// … the listener's address, as written in `server.listeners`,
    #[serde(default)]
    pub listener: Option<String>,
    /// … the user the session authenticated as, whose `grants.ramps` decide
    /// whether the header may be used,
    #[serde(default)]
    pub auth_user: Option<String>,
    /// … and the `X-Simmer-Ramp` value. Ignored when `message` is supplied:
    /// its own headers are what the session would read.
    #[serde(default)]
    pub ramp_header: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DryRunResponse {
    pub evaluated_at: chrono::DateTime<Utc>,
    /// The ramp everything below was evaluated in (D-099).
    pub ramp: String,
    /// Which rule chose it: `given` (the request's `ramp`), or §5.8's
    /// `affinity`, `header` or `default`.
    pub ramp_source: &'static str,
    /// What became of `X-Simmer-Ramp`: `absent`, `used`, `redundant`, or the
    /// reason it was ignored (§5.8's table).
    pub ramp_header: &'static str,
    /// Which sender rule matched, or `null` for the default chain.
    pub matched_rule: Option<MatchedRule>,
    /// Where the chain came from — a sender rule's path, or `default_chain`.
    pub chain_source: String,
    pub chain: Vec<String>,
    /// D-047's note, present only when it applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Set when §3.2 step 1 refuses the sender outright, before any chain
    /// exists to walk. `recipients` is empty when it is — there is nothing to
    /// evaluate them against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refused>,
    /// §3.2 step 2a (D-090). Absent when `thread_affinity` is off or the
    /// message refers to no message ID — the ordinary first message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadView>,
    pub recipients: Vec<RecipientOutcome>,
}

/// What §3.2 step 2a made of the message's threading headers.
#[derive(Debug, Serialize)]
pub struct ThreadView {
    /// `pinned` or `unmatched`.
    pub outcome: &'static str,
    /// The route walked first. `null` when unmatched.
    pub pinned: Option<String>,
    pub explanation: &'static str,
}

/// §3.2 step 1 said no. Not an HTTP error: the request was well formed and this
/// is its answer.
#[derive(Debug, Serialize)]
pub struct Refused {
    /// `strict_senders` or `malformed_from_header`.
    pub reason: &'static str,
    pub explanation: &'static str,
    /// The reply the client would receive.
    pub would_reply: String,
}

#[derive(Debug, Serialize)]
pub struct MatchedRule {
    pub index: usize,
    #[serde(rename = "match")]
    pub pattern: String,
    pub match_on: &'static str,
    pub chain: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RecipientOutcome {
    pub recipient: String,
    pub domain_group: String,
    /// Why the recipient is in that group (D-100): `literal`, `mx:<host>` for
    /// the lowest-preference MX host that matched a group's suffix, `fallback`,
    /// or `fallback:mx-unavailable` when the MX lookup failed or timed out.
    pub domain_group_basis: String,
    /// One entry per route consulted, in walk order — chain order, with a
    /// thread-affinity pin moved to the front (D-090). The walk stops at the
    /// first eligible route, so the links after it are absent — they would not
    /// have been consulted either.
    pub evaluation: Vec<StepView>,
    pub selected: Option<String>,
    /// What the client would be answered. `250` when a route was selected — with
    /// the caveat that the downstream still has to accept it — and §10.3's reply
    /// when nothing was.
    pub would_reply: String,
    /// Absent when no route was selected: there is no identity to render.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound: Option<Outbound>,
    /// §7.7 — present for a `delivery: spool` ramp: what admission would say
    /// and how long the message would wait. `evaluation` is then the first
    /// attempt's walk, made now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spool: Option<SpoolView>,
}

/// D-119 through §9.4: the same `spool::accept::admission` the accept path
/// calls.
#[derive(Debug, Serialize)]
pub struct SpoolView {
    /// `admit`, `full`, `backlog` or `draining`.
    pub admission: &'static str,
    /// What the client would be answered at the final dot.
    pub would_reply: String,
    /// The lane's forecast wait ahead of this message; `null` when nothing in
    /// front would queue it.
    pub expected_wait_seconds: Option<i64>,
    /// The end of the hold (Q4): delivered by then, or dead-lettered.
    pub hold_until: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct StepView {
    pub route: String,
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// A sentence rather than a label, because "quota" and "not_started" call
    /// for opposite responses and the difference is the whole answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<&'static str>,
    /// D-111 — the route's rate step: the slot the message would be given and
    /// how long the client would be held for it, or, on a `rate` skip, the
    /// earliest slot there was. Absent for a route with no `rate`. Computed
    /// from the bucket as it is now, without booking (it reserves nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<RateView>,
}

/// D-111's step, as §9.4 reports it.
#[derive(Debug, Serialize)]
pub struct RateView {
    pub send_at: chrono::DateTime<Utc>,
    pub wait_seconds: f64,
    /// A thread-affinity reply's pinned route, past its limit: it would book
    /// anyway, counted, and go now.
    pub over_limit: bool,
}

#[derive(Debug, Serialize)]
pub struct Outbound {
    pub route: String,
    /// `null` is the null sender, which §6.2 leaves alone (D-035).
    pub envelope_from: Option<String>,
    /// The header block as it would leave, in order.
    pub headers: Vec<Header>,
    /// §6.4 against the sample. See [`Outbound::body_rewrites`]'s note.
    pub body_rewrites: Vec<rewrite::body::RuleMatch>,
    /// §6.4 parts the engine would not touch, by reason (§9.1's
    /// `simmer_body_rewrite_skipped_total`).
    pub body_rewrite_skipped: Vec<&'static str>,
    pub body_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

pub async fn dryrun(
    State(state): State<AdminState>,
    _actor: Actor,
    Json(request): Json<DryRunRequest>,
) -> Result<Json<DryRunResponse>, ApiError> {
    let cfg = state.config();
    let (ramp, ramp_source, ramp_header) = choose_ramp(cfg, &request)?;
    let now = Utc::now();

    if request.recipients.is_empty() {
        return Err(ApiError::bad_request(
            "recipients must contain at least one address",
        ));
    }

    // §5.4 matches on the *address*, and the relay never sees anything else: the
    // session parses the header block with `first_from_address` before building
    // `Senders`. §9.4 accepts "a `From:` header value", which is the display-name
    // form an operator will paste, so it goes through the same function — passing
    // `Jane <jane@oldbrand.com>` through raw makes every domain rule miss, and the
    // dry run then reports a fall-through to `default_chain` that would not
    // happen. Found by driving this endpoint against the shipped configuration.
    let from_address = request.from_header.as_deref().and_then(from_address);
    let senders = Senders::new(request.envelope_from.as_deref(), from_address.as_deref());

    // §3.2 steps 1–2, through the same function the relay uses.
    //
    // A sender-policy refusal is a legitimate *answer* — the request was well
    // formed and this is what would happen to it — so it is a `200` describing
    // the refusal, not an HTTP error. There is simply no chain to walk, so no
    // recipient is evaluated.
    let chain = match relay::resolve_chain(ramp, &senders) {
        Ok(chain) => chain,
        Err(e) => {
            return Ok(Json(DryRunResponse {
                evaluated_at: now,
                ramp: ramp.name.clone(),
                ramp_source,
                ramp_header,
                matched_rule: None,
                chain_source: "none".to_string(),
                chain: Vec::new(),
                note: None,
                thread: None,
                refused: Some(Refused {
                    reason: refusal_reason(&e),
                    explanation: refusal_explanation(&e),
                    would_reply: e.to_reply(ramp).to_wire().trim_end().to_string(),
                }),
                recipients: Vec::new(),
            }));
        }
    };

    let (matched_rule, chain_source) = match sender_match::match_sender(ramp, &senders) {
        sender_match::Match::Rule { rule, index } => (
            Some(MatchedRule {
                index,
                pattern: rule.pattern.clone(),
                match_on: match_on_name(rule.match_on),
                chain: rule.chain.clone(),
            }),
            format!("senders[{index}] (match '{}')", rule.pattern),
        ),
        sender_match::Match::Unmatched => (None, "default_chain".to_string()),
    };

    // §3.2 step 2a, from the bytes the relay would see — the same function over
    // the same message, so a dry run cannot read threading headers differently.
    // The pin depends on the headers and the chain, never on the recipient.
    let pin = crate::routing::thread::pin_for_message(
        ramp,
        chain,
        &synthesise(&request, &request.recipients[0]),
    );
    let thread = thread_view(&pin);

    let mut recipients = Vec::with_capacity(request.recipients.len());
    for recipient in &request.recipients {
        recipients.push(evaluate_one(&state, ramp, chain, &pin, recipient, &request, now).await?);
    }

    Ok(Json(DryRunResponse {
        ramp: ramp.name.clone(),
        ramp_source,
        ramp_header,
        evaluated_at: now,
        matched_rule,
        chain_source,
        chain: chain.to_vec(),
        note: (request.recipients.len() > 1).then(|| {
            "a real transaction carries exactly one recipient (D-047); these were evaluated \
             independently, as separate transactions would be"
                .to_string()
        }),
        refused: None,
        thread,
        recipients,
    }))
}

fn thread_view(pin: &crate::routing::thread::Pin) -> Option<ThreadView> {
    use crate::routing::thread::Pin;
    match pin {
        Pin::None => None,
        Pin::Unmatched => Some(ThreadView {
            outcome: "unmatched",
            pinned: None,
            explanation: "the message refers to message IDs, and no route in this chain emitted \
                          any of them, so the chain is walked in its configured order (D-090)",
        }),
        Pin::Route(route) => Some(ThreadView {
            outcome: "pinned",
            pinned: Some(route.clone()),
            explanation: "a reply into a thread this route started: it is walked first, without \
                          its recipient_frequency threshold, and past its day's cap if it has no \
                          headroom. Pause, strict preflight and a future warmup.started still \
                          skip it (§3.2 step 2a, D-090)",
        }),
    }
}

/// A slug for the two §3.2 step 1 refusals a dry run can produce.
///
/// The other two `SelectError` variants cannot reach here: `resolve_chain` is
/// pure and touches no storage, so it can neither exhaust a chain nor fail
/// against the quota store.
fn refusal_reason(e: &relay::SelectError) -> &'static str {
    match e {
        relay::SelectError::StrictSenderRejected { .. } => "strict_senders",
        relay::SelectError::MalformedFromHeader => "malformed_from_header",
        relay::SelectError::ChainExhausted => "chain_exhausted",
        relay::SelectError::QuotaUnavailable => "quota_unavailable",
    }
}

fn refusal_explanation(e: &relay::SelectError) -> &'static str {
    match e {
        relay::SelectError::StrictSenderRejected { .. } => {
            "this sender matches no rule and strict_senders is true, so §3.2 step 1 rejects \
             it outright. This is the one permitted 550: it is a policy statement about the \
             sender, not about the recipient (§10.3)"
        }
        relay::SelectError::MalformedFromHeader => {
            "a sender rule matches on the From: header and no address could be parsed from \
             the one supplied (§5.4, D-028)"
        }
        relay::SelectError::ChainExhausted => "no eligible route (§3.2 step 4, §10.3)",
        relay::SelectError::QuotaUnavailable => "the quota store is unreachable (§7.5)",
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_one(
    state: &AdminState,
    ramp: &crate::config::Ramp,
    chain: &[String],
    pin: &crate::routing::thread::Pin,
    recipient: &str,
    request: &DryRunRequest,
    now: chrono::DateTime<Utc>,
) -> Result<RecipientOutcome, ApiError> {
    let cfg = state.config();

    // The walk resolves the group again; the second answer comes from the
    // cache this one just filled, so the two cannot disagree in practice.
    let (domain_group, domain_group_basis) =
        match state.engine.groups.resolve(ramp, recipient).await {
            Some(r) => (r.group.name.clone(), r.basis.describe()),
            None => ("catchall".to_string(), "fallback".to_string()),
        };

    let evaluation = chain::dry_walk(
        ramp,
        &state.engine.groups,
        &cfg.dot_insensitive_domains,
        state.store(),
        &state.engine.frequency,
        &state.engine.preflight,
        &crate::routing::thread::order(chain, pin),
        pin.route(),
        recipient,
        now,
    )
    .await?;

    let selected = evaluation
        .iter()
        .find(|s| s.outcome.is_ok())
        .map(|s| s.route.clone());

    let would_reply = match &selected {
        Some(_) => "250 2.0.0 accepted (if the downstream accepts it)".to_string(),
        None => relay::SelectError::ChainExhausted
            .to_reply(ramp)
            .to_wire()
            .trim_end()
            .to_string(),
    };

    // No route, no identity to render — §6.1 step 3 is what tells the engine
    // which one to apply.
    let outbound = selected
        .as_ref()
        .map(|route| render(state, &ramp.name, route, recipient, request, now));

    // §7.7 — a spooling ramp answers at the final dot with admission's
    // verdict, not the walk's; the walk is the first attempt's.
    let spool = match (&state.engine.spool, ramp.delivery) {
        (Some(spool), crate::config::Delivery::Spool) => {
            use crate::spool::accept::{admission, Admit};
            let bytes = i64::try_from(synthesise(request, recipient).len()).unwrap_or(i64::MAX);
            let v = admission(&state.engine, spool, ramp, chain, &domain_group, bytes, now).await?;
            let reply = match v.outcome {
                Admit::Admit => crate::smtp::reply::queued(uuid::Uuid::nil()),
                Admit::Draining => crate::smtp::reply::spool_draining(),
                Admit::Full => crate::smtp::reply::spool_full(),
                Admit::Backlog => crate::smtp::reply::spool_backlog(),
            };
            Some(SpoolView {
                admission: v.outcome.as_str(),
                would_reply: reply.to_wire().trim_end().to_string(),
                expected_wait_seconds: v.expected_wait.map(|w| w.num_seconds()),
                hold_until: v.hold_until,
            })
        }
        _ => None,
    };

    Ok(RecipientOutcome {
        recipient: recipient.to_string(),
        domain_group,
        domain_group_basis,
        evaluation: evaluation.iter().map(step_view).collect(),
        selected,
        would_reply,
        outbound,
        spool,
    })
}

/// §6.1's order of operations, over a synthesised message.
///
/// The clock and the UUID are the real ones rather than the fixed values §6.6's
/// probe uses: an operator asking what `{{uuid}}` renders to wants to see a
/// UUID, and `Received:` carrying the current instant is what makes the preview
/// look like the thing it is previewing.
fn render(
    state: &AdminState,
    ramp: &str,
    route_name: &str,
    recipient: &str,
    request: &DryRunRequest,
    now: chrono::DateTime<Utc>,
) -> Outbound {
    let cfg = state.config();
    let raw = synthesise(request, recipient);
    let recipients = [recipient.to_string()];

    // Unreachable in practice: `Rewriters` is compiled from the same routes the
    // walk selected from.
    let Some(rewriter) = state.engine.rewriters.get(ramp, route_name) else {
        return Outbound {
            route: route_name.to_string(),
            envelope_from: None,
            headers: Vec::new(),
            body_rewrites: Vec::new(),
            body_rewrite_skipped: vec!["no compiled rewrite for this route"],
            body_changed: false,
            message: None,
        };
    };

    let rewritten = rewrite::rewrite(
        rewriter,
        &rewrite::Inbound {
            raw: &raw,
            envelope_from: request.envelope_from.as_deref(),
            recipients: &recipients,
            route_name,
            // Distinguishable in the logs from a real message's, on purpose.
            correlation_id: "dryrun",
            received: rewrite::Received {
                helo: "dryrun.simmer",
                peer: "127.0.0.1",
                by: &cfg.server.hostname,
                authenticated: true,
                tls: false,
            },
            now,
            uuid: &|| uuid::Uuid::new_v4().to_string(),
        },
    );

    let (headers, body) = split_message(&rewritten.raw);
    let original_body = split_message(&raw).1;

    Outbound {
        route: route_name.to_string(),
        envelope_from: rewritten.envelope_from.clone(),
        headers,
        // Against the sample as supplied, which for the `body` form is exactly
        // the text §6.4 would decode a `text/plain` part into. For a MIME
        // `message` it is the raw body including boundaries, so a pattern that
        // fires here and not in `body_changed` is a pattern matching structure
        // rather than content — worth seeing rather than hiding.
        body_rewrites: rewriter
            .body_rewrites
            .match_report(&String::from_utf8_lossy(&original_body)),
        body_rewrite_skipped: rewritten.skipped_parts.iter().map(|r| r.as_str()).collect(),
        body_changed: body != original_body,
        message: request
            .include_message
            .then(|| String::from_utf8_lossy(&rewritten.raw).into_owned()),
    }
}

/// Build a message to rewrite.
///
/// `message` is used verbatim when supplied — the only faithful way to dry-run a
/// MIME structure. Otherwise a minimal `text/plain` message is assembled from
/// the fields §9.4 names, because the common case is an operator with a `From:`
/// value and a paragraph, not a raw RFC 5322 document.
/// §9.4 (D-099): the ramp, which rule chose it, and what became of the header.
/// `ramp` is given outright, or §5.8 selects from the rest — never both, since
/// an answer that silently ignored half the request would mislead exactly the
/// operator who asked.
fn choose_ramp<'a>(
    cfg: &'a crate::config::Config,
    request: &DryRunRequest,
) -> Result<(&'a crate::config::Ramp, &'static str, &'static str), ApiError> {
    use crate::routing::ramp_select;

    let selecting =
        request.listener.is_some() || request.auth_user.is_some() || request.ramp_header.is_some();
    if let Some(name) = &request.ramp {
        if selecting {
            return Err(ApiError::bad_request(
                "give either `ramp`, or what §5.8 selects from (`listener`, `auth_user`, \
                 `ramp_header`), not both",
            ));
        }
        let ramp = cfg
            .ramps
            .get(name)
            .ok_or_else(|| ApiError::not_found("ramp", name))?;
        return Ok((ramp, "given", "absent"));
    }

    let listener = request
        .listener
        .as_ref()
        .map(|address| {
            cfg.server
                .listeners
                .iter()
                .find(|l| &l.address == address)
                .ok_or_else(|| ApiError::not_found("listener", address))
        })
        .transpose()?;
    if let Some(user) = &request.auth_user {
        if !cfg.server.auth.users.iter().any(|u| &u.username == user) {
            return Err(ApiError::not_found("user", user));
        }
    }
    let ingress = ramp_select::ingress(
        cfg,
        listener.and_then(|l| l.ramp.as_deref()),
        listener.is_some_and(|l| l.header_overrides_affinity),
        request.auth_user.as_deref(),
    );
    let headers = match &request.message {
        Some(message) => crate::rewrite::headers::split(&normalise_eol(message))
            .headers
            .get_all(ramp_select::HEADER),
        None => request.ramp_header.iter().cloned().collect(),
    };
    let selection = ramp_select::select(cfg, &ingress, &headers);
    Ok((
        selection.ramp,
        selection.source.as_str(),
        selection.header.as_str(),
    ))
}

fn synthesise(request: &DryRunRequest, recipient: &str) -> Vec<u8> {
    if let Some(message) = &request.message {
        return normalise_eol(message);
    }

    let from = request
        .from_header
        .clone()
        .or_else(|| request.envelope_from.clone())
        .unwrap_or_default();

    let mut out = String::new();
    if !from.is_empty() {
        out.push_str(&format!("From: {from}\r\n"));
    }
    out.push_str(&format!("To: {recipient}\r\n"));
    out.push_str(&format!(
        "Subject: {}\r\n",
        request.subject.as_deref().unwrap_or("simmer dry run")
    ));
    if let Some(v) = &request.in_reply_to {
        out.push_str(&format!("In-Reply-To: {v}\r\n"));
    }
    // So the rendered outbound shows it stripped (§6.5), as it would be.
    if let Some(v) = &request.ramp_header {
        out.push_str(&format!("{}: {v}\r\n", crate::routing::ramp_select::HEADER));
    }
    if let Some(v) = &request.references {
        out.push_str(&format!("References: {v}\r\n"));
    }
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("\r\n");
    out.push_str(request.body.as_deref().unwrap_or(""));

    normalise_eol(&out)
}

/// The address out of a `From:` header value, through the relay's own parser.
///
/// Wrapped back into a header block because that is what `first_from_address`
/// takes — it is fed the head of a real message on the relay path — and reusing
/// it is the whole point. A value that is already a bare address survives
/// unchanged.
fn from_address(value: &str) -> Option<String> {
    let block = format!("From: {}\r\n\r\n", value.trim());
    crate::smtp::session::first_from_address(block.as_bytes())
}

/// A JSON string carries bare `\n`; SMTP carries `\r\n`. Rewriting a message
/// with mixed line endings would report differences that are an artefact of the
/// transport this request arrived over.
fn normalise_eol(s: &str) -> Vec<u8> {
    s.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes()
}

fn split_message(raw: &[u8]) -> (Vec<Header>, Vec<u8>) {
    let block = rewrite::headers::split(raw);
    let headers = block
        .headers
        .fields()
        .map(|(name, value)| Header {
            name: name.to_string(),
            value: value.trim().to_string(),
        })
        .collect();
    (headers, block.body.to_vec())
}

fn step_view(step: &chain::Step) -> StepView {
    StepView {
        rate: step.rate.map(|r| RateView {
            send_at: r.send_at,
            wait_seconds: r.wait.num_milliseconds() as f64 / 1000.0,
            over_limit: r.over_limit,
        }),
        ..step_outcome(step)
    }
}

fn step_outcome(step: &chain::Step) -> StepView {
    match step.outcome {
        Ok(()) if step.over_cap => StepView {
            route: step.route.clone(),
            outcome: "selected",
            reason: Some("over_cap"),
            explanation: Some(
                "a thread-affinity reply on its pinned route, which has no headroom left for \
                 this domain group today: it would be reserved and counted past the cap \
                 (§3.2 step 2a, D-090)",
            ),
            rate: None,
        },
        Ok(()) => StepView {
            route: step.route.clone(),
            outcome: "selected",
            reason: None,
            explanation: None,
            rate: None,
        },
        Err(reason) => StepView {
            route: step.route.clone(),
            outcome: "skipped",
            reason: Some(reason.as_str()),
            explanation: Some(explain(reason)),
            rate: None,
        },
    }
}

fn explain(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::Paused => "an operator paused this route through §9.3's write API",
        SkipReason::NotStarted => {
            "warmup.started is in the future, so the route has not begun (§7.2). \
             This is not a quota problem and more quota will not fix it"
        }
        SkipReason::Quota => {
            "no headroom left for this domain group today (§3.2 step 3c). It returns at \
             the route's next day boundary"
        }
        SkipReason::Frequency => {
            "this recipient is at or over the route's recipient_frequency threshold within \
             the window (§7.3), so the message steers to the next link"
        }
        SkipReason::Preflight => "§6.7's DNS preflight failed with preflight.strict",
        SkipReason::PartialRamp => {
            "the route's share is below 1 right now and this message is not in it (§3.2 \
             step 3c′, D-091). The route has headroom; it is being given less traffic on \
             purpose. Under a share list that ends when the list does; under share: auto \
             the share moves with how full the day's cap is and how far through the day \
             it is, so /routes is where to read what it is now (D-097)"
        }
        SkipReason::Rate => {
            "the route's next sending slot for this domain group is later than its \
             rate.max_wait allows (§3.2 step 3c″, D-111). The route has headroom; it is being \
             paced, and the message steers to the next link. `rate.send_at` is the earliest \
             slot there was"
        }
        SkipReason::Unknown => {
            "the chain names a route that is not defined. §4.2 refuses to start on this, \
             so seeing it means the configuration and the process disagree"
        }
    }
}

fn match_on_name(match_on: crate::config::MatchOn) -> &'static str {
    use crate::config::MatchOn;
    match match_on {
        MatchOn::Envelope => "envelope",
        MatchOn::FromHeader => "from_header",
        MatchOn::Either => "either",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Option<&str>, message: Option<&str>) -> DryRunRequest {
        DryRunRequest {
            envelope_from: Some("app@oldbrand.com".into()),
            from_header: Some("Old Brand <app@oldbrand.com>".into()),
            recipients: vec!["someone@gmail.com".into()],
            message: message.map(str::to_string),
            body: body.map(str::to_string),
            subject: None,
            in_reply_to: None,
            references: None,
            include_message: false,
            ramp: None,
            listener: None,
            auth_user: None,
            ramp_header: None,
        }
    }

    #[test]
    fn threading_headers_are_synthesised_only_when_asked_for() {
        let raw = String::from_utf8(synthesise(&request(Some("hi"), None), "a@b.com")).unwrap();
        assert!(!raw.contains("In-Reply-To") && !raw.contains("References"));

        let mut r = request(Some("hi"), None);
        r.in_reply_to = Some("<r@gmail.com>".into());
        r.references = Some("<1@newbrand.com> <r@gmail.com>".into());
        let raw = String::from_utf8(synthesise(&r, "a@b.com")).unwrap();
        assert!(raw.contains("In-Reply-To: <r@gmail.com>\r\n"), "{raw}");
        assert!(
            raw.contains("References: <1@newbrand.com> <r@gmail.com>\r\n"),
            "{raw}"
        );
        let (head, _) = raw.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("References:"), "a header, not the body");
    }

    #[test]
    fn a_synthesised_message_is_a_well_formed_text_plain() {
        let raw = synthesise(
            &request(Some("hello https://oldbrand.com/x"), None),
            "a@b.com",
        );
        let text = String::from_utf8(raw).unwrap();

        assert!(text.starts_with("From: Old Brand <app@oldbrand.com>\r\n"));
        assert!(text.contains("\r\nTo: a@b.com\r\n"));
        assert!(text.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"));
        assert!(text.ends_with("\r\n\r\nhello https://oldbrand.com/x"));
    }

    #[test]
    fn a_supplied_message_is_used_verbatim() {
        // The only faithful way to dry-run a MIME structure: §6.4 works in byte
        // ranges over the real parts (D-043), so anything reassembled from
        // fields would be a different message.
        let raw = synthesise(
            &request(
                Some("ignored"),
                Some("From: x@y.com\nSubject: real\n\nbody"),
            ),
            "a@b.com",
        );
        let text = String::from_utf8(raw).unwrap();
        assert_eq!(text, "From: x@y.com\r\nSubject: real\r\n\r\nbody");
        assert!(!text.contains("ignored"));
    }

    #[test]
    fn line_endings_are_normalised_to_crlf() {
        // A JSON string carries bare \n. Feeding that to the rewrite engine
        // would report differences that belong to the transport rather than to
        // the configuration.
        assert_eq!(normalise_eol("a\nb"), b"a\r\nb");
        assert_eq!(normalise_eol("a\r\nb"), b"a\r\nb");
        assert_eq!(normalise_eol("a\r\n\nb"), b"a\r\n\r\nb");
    }

    #[test]
    fn an_empty_body_still_produces_a_header_block_and_a_separator() {
        let raw = synthesise(&request(None, None), "a@b.com");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn every_skip_reason_has_an_explanation() {
        // The reason is the product. A label an operator has to look up is a
        // worse answer than a sentence, and a missing one is no answer at all.
        for reason in [
            SkipReason::Paused,
            SkipReason::NotStarted,
            SkipReason::Quota,
            SkipReason::Frequency,
            SkipReason::Preflight,
            SkipReason::PartialRamp,
            SkipReason::Rate,
            SkipReason::Unknown,
        ] {
            assert!(!explain(reason).is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn a_step_view_reports_the_rate_slot_and_wait() {
        // D-111: §9.4 reports the rate step — the slot and the hold.
        let at: chrono::DateTime<Utc> = "2026-10-03T12:00:00Z".parse().unwrap();
        let rate = chain::RateStep {
            send_at: at + chrono::Duration::milliseconds(2500),
            wait: chrono::Duration::milliseconds(2500),
            over_limit: false,
        };
        let held = step_view(&chain::Step {
            route: "warming".into(),
            outcome: Ok(()),
            over_cap: false,
            rate: Some(rate),
        });
        let r = held.rate.expect("a rate step");
        assert_eq!(r.wait_seconds, 2.5);
        assert_eq!(r.send_at, rate.send_at);
        assert_eq!(held.outcome, "selected");

        let steered = step_view(&chain::Step {
            route: "warming".into(),
            outcome: Err(SkipReason::Rate),
            over_cap: false,
            rate: Some(rate),
        });
        assert_eq!(steered.reason, Some("rate"));
        assert!(steered.rate.is_some(), "the earliest slot, for a rate skip");
        let json = serde_json::to_value(&steered).unwrap();
        assert_eq!(json["rate"]["wait_seconds"], 2.5);

        // No rate block: no field at all.
        let plain = step_view(&chain::Step {
            route: "overflow".into(),
            outcome: Ok(()),
            over_cap: false,
            rate: None,
        });
        assert!(serde_json::to_value(&plain).unwrap().get("rate").is_none());
    }

    #[test]
    fn a_step_view_names_the_reason_and_omits_it_when_selected() {
        let selected = step_view(&chain::Step {
            route: "overflow".into(),
            outcome: Ok(()),
            over_cap: false,
            rate: None,
        });
        assert_eq!(selected.outcome, "selected");
        assert_eq!(selected.reason, None);

        let skipped = step_view(&chain::Step {
            route: "warming".into(),
            outcome: Err(SkipReason::Quota),
            over_cap: false,
            rate: None,
        });
        assert_eq!(skipped.outcome, "skipped");
        assert_eq!(skipped.reason, Some("quota"));
        assert!(skipped.explanation.is_some());

        let over = step_view(&chain::Step {
            route: "warming".into(),
            outcome: Ok(()),
            over_cap: true,
            rate: None,
        });
        assert_eq!(over.outcome, "selected", "past the cap is still selected");
        assert_eq!(over.reason, Some("over_cap"));
        assert!(over.explanation.is_some());
    }
}
