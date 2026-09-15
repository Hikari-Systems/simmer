# Decisions

Divergences from `docs/SPEC.md`, and calls the spec left open, recorded as they
are made. `SPEC.md` is authoritative and is **not** amended silently — anything
here that should become spec is for the spec's author to fold in.

Each entry: what the spec says, what we do, and why.

---

## Phase 1

### D-001 — §6.6 rewrite stability supersedes "idempotent" in the kickoff prompt

**Spec:** §6.6 (as revised) splits the property by field class. Identity fields
(`identity.envelope_from`, `From:`, `Sender:`, `Message-ID:`) must satisfy
`rewrite(rewrite(m)) == rewrite(m)` with **no override**. Every other header must
too, unless named in the route's `unstable_headers`.

**Prompt:** non-negotiable constraint 2 still states the property unconditionally
for every route, with no notion of a declared exception.

**Decision:** follow `SPEC.md`, per the prompt's own "where this prompt and
`SPEC.md` disagree, `SPEC.md` wins". Constraint 2 is read as unconditional for
identity fields only.

**Why it matters:** the §4.1 example configuration sets
`Reply-To: {{original.from.address}}`, which reads a field the same pass
overwrites. Under the prompt's unconditional reading that configuration is
rejected at startup; under §6.6 it is accepted with `unstable_headers:
["Reply-To"]`. The revised spec is the better answer because it names the real
distinction: *migration-only* instability, which is intentional and disappears
along with Simmer, versus *accidental* instability, which is a bug.

*Suggested prompt edit: one line in constraint 2 pointing at §6.6's field
classes, so the next reader does not have to reconcile the two.*

### D-002 — §12.3's byte-equivalence assertion must exclude declared unstable headers

**Spec:** §12.3 acceptance requires "sending the same logical message under both
arrangements of §1.1 and asserting byte-equivalent downstream output". §6.6
permits declared headers to differ between exactly those two arrangements.

**Decision:** the §12.3 comparison excludes `unstable_headers` alongside the
volatile template variables (`uuid`, `now.*`, `correlation_id`) that §6.6 already
excludes.

**Why:** a declared unstable header is *by definition* not byte-equivalent across
the two arrangements — that is what declaring it means. Without the exclusion the
acceptance test contradicts §6.6 and no valid configuration could pass both.

Also excluded, for the same class of reason: the `Received:` header Simmer
prepends (§6.1 step 8) and the `X-Simmer-*` headers, which are present only
because Simmer is in the path. These are in mild tension with §1.1's "neither
ordering may change what the recipient sees" and are kept because they are
operationally essential; the exception is noted rather than hidden.

### D-003 — Admin listener uses `axum`, not the house `actix-web`

**Spec:** §12.1 suggests `axum` for the admin listener ("suggested, not
mandated"). The hikari-systems data-service pattern is uniformly actix-web 4
behind `hs_utils::server::run`.

**Decision:** `axum`.

**Why:** the prompt scopes "use the house pattern" to "Postgres connection
management, migrations, and query organisation" — none of which is the HTTP
layer. `hs_utils::server::run` sits behind the `web` feature, which also brings
the actix middleware and JSON-config machinery this service does not use (D-004),
and its main value is uniform behaviour across fleet-deployed services, which
Simmer is not (D-007). What the house pattern *does* govern here is kept: runtime
`sqlx` queries over compile-time macros, `models/<entity>.rs` free functions over
`&PgPool`, plain-SQL migrations applied by `sqlx::migrate!` at startup, and a
concrete state struct rather than a trait object.

### D-004 — Configuration is YAML per §4, not the house `config.json` layering

**Spec:** §4 — "Format is YAML. Secrets are supplied by `${ENV_VAR}`
interpolation, resolved at startup; an unresolvable reference is a fatal startup
error." The house pattern is `config.json` + a `/sandbox` overlay +
`segment__segment` env overrides + `[SECRET]:/path` file indirection, via
`hs_utils::config::load_layered_value()`.

**Decision:** follow §4 literally. One YAML file, `${ENV_VAR}` only.
`hs_utils::config` is not used.

**Why:** the two mechanisms exist to serve the spot-fleet deployment, which
Simmer does not use (D-007). Adopting the layering would be complexity in service
of a deployment path not being taken. Confirmed with the spec's author.

**Implementation note:** interpolation runs over the *parsed* YAML tree, not the
raw text. Textual substitution would let a secret containing a newline and a
colon restructure the document — a password ending `\n  strict_senders: false`
would be a config injection. There is a test for this.

### D-005 — The pool is built directly, not via `hs_utils::db::build_pool`

**Spec:** §4.1 specifies `database.url`, a single connection URL. §11 says to
follow the house pattern for connection management, and `build_pool` takes a
`DbConfig` of discrete host / port / database / username / password fields.

**Decision:** build the pool with `PgConnectOptions::from_str(&url)` and
`PgPoolOptions`.

**Why:** reconstructing a `DbConfig` from a URL means parsing it only to
re-serialise it, and the two disagree about SSL configuration (`DbConfig` has an
`ssl` block; a URL carries `sslmode`). Forced by §4.1's schema, not chosen.
`§4.1`'s `connect_timeout` is mapped to the pool's *acquire* timeout, which is
what actually bounds how long a message waits for a usable connection — relevant
to keeping the §8.4 timeout budget honest.

### D-006 — Logging is JSON via `tracing_subscriber` directly

**Spec:** §9.5 requires structured JSON with a `correlation_id` on every line.
`hs_utils::logging::init` emits human-readable text with no JSON option.

**Decision:** call `tracing_subscriber` directly, honouring §4.1's
`logging.format` (`json` default, `text` for local terminals). `RUST_LOG` still
overrides, for debugging a container without editing its config.

*The clean upstream fix is a `format` argument on `hs_utils::logging::init`; a
small PR to `hs-utils-rs` would let this revert.*

### D-007 — Single instance confirmed; not deployed on the spot fleet

**Spec:** §2.2 — "No multi-instance clustering. One Simmer instance owns its
quota state."

**Context:** the house `hs-deploy` method is an AWS spot fleet that requires
target capacity ≥2 and rolls one instance at a time, so a standard deploy would
run two Simmers against one database for the minutes a replacement takes to
build. ~~That window is precisely when quota overshoot would occur.~~

> **Corrected 2026-08-11 by D-061.** The struck sentence is wrong and was wrong
> from phase 3 onward. Quota overshoot through the §7.4 path is not reachable
> across instances: the headroom check and the write share one transaction holding
> a row lock, and Postgres serialises contenders for that row across processes as
> readily as across tasks. `tests/quota_multi_instance.rs` is the evidence.
>
> **The decision below is unchanged** — simmer still does not go on the spot fleet
> — but it now rests on the two things that are actually true: config skew during
> a roll under D-026, and D-049's recipient-frequency race. See D-061 and
> `docs/MULTI_INSTANCE.md`.

**Decision:** Simmer is not deployed on the spot fleet. §2.2 stands as written.
No `instance_id` column, no sweeper advisory locks, no instance-scoped shutdown
release.

**Kept anyway, because they cost nothing now and are expensive to retrofit** —
and because §11 asks that the storage layer "should not preclude" horizontal
scaling later: §10.4 shutdown releases only the reservations belonging to its own
in-flight sessions rather than truncating the table, and the sweeper's expiry
query is written in a form that is safe under concurrency.

### D-008 — §10.1's `5xx` mapping is split by downstream stage

**Spec:** §10.1 maps a downstream `5xx` at *any* stage to `550` toward the client.

**Decision:**

| Stage of downstream `5xx` | Client reply |
|---|---|
| `RCPT TO` | `550` + sanitised downstream text |
| `MAIL FROM` | `451` + sanitised downstream text |
| `DATA` / final dot | `451` + sanitised downstream text |
| connect / TLS / timeout / protocol | `451` (unchanged) |

`MAIL FROM` and `DATA` failures additionally emit an `ERROR` log and increment
`simmer_downstream_config_error_total{route,stage}`. `4xx` handling is unchanged.

**Why:** the likeliest `5xx` in this system is the downstream rejecting our
*rewritten* envelope sender because provider domain authentication is not
finished — the §6.5 provisioning risk, which §6.5 itself calls the single most
important operational caveat in the document. Returning `550` for that makes the
client permanently suppress a perfectly deliverable recipient, which is exactly
what §14.1's generalisable principle forbids: *Simmer must not emit a reply that
causes a client to record permanent state about a message or recipient.* A `5xx`
at `RCPT TO` is genuinely about the recipient and keeps its `550`.

The new counter is the signal that catches the §6.5 failure, which is otherwise
invisible in the mail flow. It should be alerted on.

Agreed with the spec's author. Implemented in phase 2.

### D-009 — Preflight is disabled when the block is absent

**Spec:** §6.7 says preflight applies to "each route with `preflight.enabled:
true` (default true)". The §4.1 example's overflow route carries no `preflight`
block at all, and no `spf_include` or `dkim_selector`.

**Decision:** an absent `preflight` block means disabled. When the block is
present, `enabled` defaults to true, and `spf_include` and `dkim_selector` are
then *required* — a DKIM check with no selector has nothing to query, and an SPF
check with no include has nothing to assert.

**Why:** the alternative reading makes the spec's own example configuration
invalid. Startup validation enforces the required fields, so a half-configured
preflight fails loudly rather than silently passing.

### D-010 — `dot_insensitive_domains` is explicit config

**Spec:** §7.3 normalisation removes dots from the local part "when the domain is
a known dot-insensitive provider (configurable list, defaulting to the `google`
group's domains)".

**Decision:** a top-level `dot_insensitive_domains` key, defaulting to
`["gmail.com", "googlemail.com"]`.

**Why:** the spec's default couples behaviour to a configuration-defined group
*name*. A deployment that renames the group, or does not have one, silently
changes address normalisation — which changes which messages the §7.3 frequency
cap catches. §4.2 has no rule against this, so the coupling is removed rather
than validated around. Implemented in phase 6.

### D-011 — `exhausted_chain_reply` is a top-level key

**Spec:** §3.2 and §10.3 both reference `exhausted_chain_reply`, but it does not
appear in the §4.1 example schema, so its location is unstated.

**Decision:** top level, alongside `strict_senders`. It is a policy about the
whole engine rather than about one route or one server setting. Default `451`
per §10.3.

### D-012 — Schedule values are typed `i64`, not `u64`

**Spec:** §4.2 makes "a `warmup.schedule` array ... contains a negative value" a
*validation* violation, and requires that all violations be reported together.

**Decision:** the schedule arrays deserialise as `i64` and validation rejects
negatives.

**Why:** typing them `u64` would turn a negative value into a *parse* error,
which aborts on the first bad element and therefore cannot satisfy §4.2's "report
all violations, not just the first". Validation guarantees non-negativity
thereafter.

### D-013 — Unknown configuration keys are rejected

**Spec:** silent on unknown keys.

**Decision:** every config struct is `deny_unknown_fields`.

**Why:** §2.2 says there is no hot reload, so a typo'd key would sit in the file
until someone wondered why a setting had no effect. For a component whose entire
purpose is to not exceed a limit, "the limit you configured was silently ignored"
is the worst available failure mode.

### D-014 — Phase 1's baseline migration creates only `instance_config`

**Spec:** §11 lists five indicative tables.

**Decision:** the phase 1 baseline creates `instance_config` only. `quota_usage`,
`quota_reservation`, `recipient_event` and `route_state` land in phase 3.

**Why:** phase 3 is where the reservation protocol is designed, and two questions
about those tables are still open — whether overflow routes take part in quota
accounting at all (§3.1 says "never quota-limited", §1.1 says a pass-through
route "still counts against quota"), and how §9.3's *per-domain-group* allowance
override is stored given §11's `route_state` is keyed on route alone. Creating
the tables now would bake in answers that have not been given.

### D-015 — Additional startup validation beyond §4.2's list

§4.2 enumerates the rules that must cause a refusal to start; it does not forbid
others. These are also enforced, on the grounds that each is a configuration
mistake with no plausible intent behind it and each is cheaper to catch at
startup than in production:

- `server.listen` / `admin.listen` parse as socket addresses; `allowed_cidrs`
  entries parse as CIDR blocks, and the list is non-empty (an empty list refuses
  every connection).
- Route names and domain-group names are unique; a chain is non-empty.
- `password_hash` begins `$argon2` (§5.3 specifies argon2id).
- Pool sizes and `max_concurrent_sessions` / `max_recipients` are at least 1; a
  zero would make the route or the listener permanently useless.
- `recipient_frequency.threshold` is at least 1 — zero would make the route
  permanently ineligible, which is a way of writing "disabled" that the config
  already expresses by omitting the block.
- A header set twice in one `set_headers` block (§6.2 says setting a header
  replaces all instances, so two entries are ambiguous rather than additive).
- `remove_headers` naming `From` without `set_headers` restoring it — the result
  would not be a legal message.
- A header named in `unstable_headers` that the route never sets, which means the
  declaration is stale.

### D-016 — `serde_yaml_ng` instead of `serde_yaml`

§12.1 names `serde_yaml`, which is unmaintained and published as
`0.9.34+deprecated`. `serde_yaml_ng` is the maintained continuation under the
same `MIT` terms. Recorded in `LICENSES.md`.

### D-017 — `hs-utils` is taken for one function

Pinned at `tag = "v0.31.2"` with **no features**, for
`healthcheck::check_subcommand` alone — which is what makes
`HEALTHCHECK CMD ["/app/server", "healthcheck"]` work in a runtime image with no
`curl`, and keeps the container shape identical to every other house service.
Given D-004, D-005 and D-006, nothing else in the crate applies. It is a small
dependency for a small thing and is trivially droppable.

*Noted while checking licences: `hs-utils` declares no `license` field, so an
automated check cannot tell "in-house, fine" from "unlicensed, not fine". Every
hikari-systems Rust service has the same gap. Worth fixing upstream; allow-listed
by name in `deny.toml` until then.*

---

## Phase 2

### D-018 — `SMTPUTF8` is advertised only when every reachable route declares it (settles O-10)

**Spec:** §5.2 — "`EHLO` advertises exactly: `PIPELINING`, `8BITMIME`,
`SMTPUTF8`, `SIZE <max_message_bytes>`, and `AUTH PLAIN LOGIN` when auth is
enabled."

**Problem:** Simmer cannot honour that promise for a downstream that does not
implement RFC 6531, and it cannot know which downstream a message will use at
`EHLO` time — with `match_on: from_header` the route is not chosen until the
final dot. O-1's working assumption ("reject `550 5.6.7` at `MAIL FROM` if the
selected route's downstream does not advertise it") therefore has nothing to
decide against at the moment it needs to decide.

**Decision:** a new per-route key `downstream.smtputf8: bool`, default `false`.
`EHLO` advertises `SMTPUTF8` only when **every reachable route** — the union of
all sender-rule chains and `default_chain` — declares `true`. A route defined in
`routes` but named by no chain does not get a vote, because §2.2 rules out hot
reload so nothing can bring it into service.

Three failure points follow, and each is answered where it can be answered
cheaply:

| Condition | Reply | Where |
|---|---|---|
| UTF-8 address or `SMTPUTF8` parameter, capability not advertised | `550 5.6.7` | `MAIL FROM` / `RCPT TO` |
| Configuration claims `smtputf8: true`, downstream's `EHLO` disagrees | `451 4.3.5` + `ERROR` + `simmer_downstream_config_error_total{route,stage="capability"}` | before the downstream envelope |
| Same, for `8BITMIME` | as above | as above |

**Why `550` is right for the first row, despite §14.1.** Apply §10.3's own test:
were Simmer removed, the client would talk to the same downstream, which does not
implement RFC 6531 either, and would get the same permanent refusal. The reply is
not an artefact of Simmer's presence, so it does not distort the client's view of
the world. The second row is the opposite case — a configuration fault of ours —
and takes `451` and the D-008 counter accordingly.

**Why not advertise unconditionally and `451` on shortfall.** That is purer
against §14.1 but converts a permanent misconfiguration into a message that
retries forever and never lands. Being honest at `EHLO` costs one config key.

`8BITMIME` gets only the second half of this treatment: advertised
unconditionally per §5.2, because it is near-universal and does not justify a
second key.

*Divergence from §5.2's "advertises exactly" is confined to making one entry
conditional. Nothing is added to the list.*

### D-019 — Route selection and reservation are separated; no connection pool in phase 2 (settles O-1)

**Spec:** §6.1 orders reservation after `DATA`; §5.4 says decide at `RCPT TO`
when all rules are envelope-only. §7.4's reservation expiry does not budget for a
body transfer.

**Decision:** confirmed as proposed. `src/relay.rs` has three steps —
**decide**, *(phase 3)* **reserve**, **relay** — and the reservation goes
immediately before the downstream conversation, so it never spans `DATA`.
Eligibility is evaluated at `RCPT TO` when `can_decide_at_rcpt()` holds, and at
the final dot otherwise.

Phase 2 has no quota, so "first eligible route in the chain" degenerates to
`chain[0]`; phase 3 replaces that one expression with the §3.2 step 3 walk and
inserts `reserve()` at the marked seam. Both decision points are already wired
and tested.

**Also decided:** §8.3's connection pool is **not** in phase 2. One connection
per message: connect, `EHLO`, `STARTTLS`, `AUTH`, envelope, `DATA`, `QUIT`.
§13.10 calls pooling a *refinement*, and the conversation is written against an
owned `Stream` so the pool wraps `client::relay` in phase 10 without changing it.

### D-020 — Line-length limits, and closing the connection on an oversized body

**Spec:** silent on line length. §5.5 gives `552 5.3.4` for exceeding
`max_message_bytes` but not what to do with the rest of the transfer.

**Decision:**

- Command lines are capped at 4096 octets (RFC 5321 §4.5.3.1 requires 512), and
  `DATA` lines at 65536 (RFC 5321 requires 1000). Exceeding the command cap is
  `500 5.5.2 line too long` and the session continues.
- Bare `LF` is normalised to `CRLF`, and the buffer stores the **unstuffed**
  canonical message. Re-stuffing happens on transmit.
- `MAIL FROM ... SIZE=n` is honoured against `max_message_bytes` and refused
  `552` before the body is transferred.
- A body that exceeds `max_message_bytes` mid-transfer gets `552` and the
  connection is **closed**.

**Why close.** Staying in step would mean reading and discarding an unbounded
remainder — the exact denial of service the limit exists to prevent. RFC 5321
permits termination, and it is what a real MTA does. This is the one place phase
2 does not honour RFC 2920's "keep answering".

**Why the caps at all** (same grounds as D-015): `max_message_bytes` governs only
`DATA`, so without a command-line cap a client can exhaust memory before `SIZE`
has anything to say.

### D-021 — Counters land now, the Prometheus exporter lands in phase 7

**Spec:** §9.1 lists the metrics; §13.7 puts the admin API and metrics in phase 7.

**Decision:** take the `metrics` facade crate now and call it from the code that
creates the conditions §9.1 counts — §10.2's ambiguous delivery, D-008's config
errors, §14.2's unmatched senders. With no recorder installed every call compiles
to a branch on a null pointer. Phase 7 adds `metrics-exporter-prometheus` and no
call site moves.

**Why not log-only now.** The alternative is a sweep through the relay path in
phase 7 to add counters to code written months earlier, which is where a missing
series comes from. `src/metrics.rs` also gives each metric exactly one spelling,
so a typo cannot silently create a second time series.

### D-022 — `mail-parser` arrives in phase 2, for header extraction only

**Spec:** §13.4 puts the rewriting engine in phase 4, and `LICENSES.md` §1
planned `mail-parser` for it.

**Decision:** adopt it in phase 2, used only to pull the first `From:` address
out of the buffered header block.

**Why:** the shipped configuration matches on `from_header`, so phase 2 cannot
relay end to end without parsing `From:`, and §5.4 owes a `550 5.6.0 malformed
From header` for the cases where it cannot be parsed. Hand-rolling an RFC 5322
address parser for a few weeks — one that has to survive folding, display names,
group syntax and a comma-separated list — is where subtle bugs live. Licence was
already cleared (`Apache-2.0 OR MIT`, all 36 published versions).

Only the header block is parsed, capped at 256 KiB and read from the head of the
buffer, so a spilled 25 MiB message is never read back into memory to answer a
routing question.

### D-023 — A downstream `5xx` at `EHLO` or `AUTH` joins D-008's config-error class

**Spec:** D-008 splits §10.1's `5xx` mapping by stage, naming `MAIL FROM`,
`DATA` and the final dot as config errors and `RCPT TO` as the recipient case.

**Decision:** `EHLO` and `AUTH` are added to the config-error class — `451`, an
`ERROR` log, and `simmer_downstream_config_error_total`.

**Why:** a downstream refusing *our* credentials, or refusing to talk to us at
all, is as much a statement about Simmer's configuration as a rejected envelope
sender is, and as little a statement about the recipient. D-008 did not enumerate
them because phase 1 had no downstream conversation to reach them from.

---

## Phase 3

### D-024 — Overflow routes account fully, and measure their day from the epoch (settles O-2)

**Spec:** §3.1 — an overflow route "carries no warm-up schedule and is never
quota-limited". §1.1 — "A route that happens to emit the identity it received
still counts against quota."

**These do not conflict.** §1.1 is about a *warming* route whose output identity
happens to equal its input: do not skip accounting merely because no bytes
changed. It says nothing about overflow routes.

**Decision:** an overflow route runs the full §7.4 reserve/commit protocol with
`allowance IS NULL`. "Never quota-limited" is honoured by the headroom check
never failing.

**Why account at all:** *how much traffic is spilling to overflow* is the single
most important number during a warm-up, and this is what makes it answerable.
The alternative — a special case that skips the protocol — also means §9.2's
`/routes` and every §9.1 series need a branch. `simmer_quota_allowance` reports
`+Inf` for these routes, which is honest and plots alongside the warming ones.

**Consequence:** an overflow route has no `warmup.started`, so no day boundary.
It is given a synthetic start of the **Unix epoch**, so `day_index = floor((now −
started) / 24h)` is one formula for every route and overflow buckets on UTC
midnight. Warming routes bucket on their own anniversary. The two clocks differ
and are never compared — they only key rows — but a reader of `quota_usage` needs
to know which is which, and a test that reads the wrong one finds an empty row.

### D-025 — The allowance override is a column on `quota_usage`, not a `route_state` field (settles O-3)

**Spec:** §11 lists `route_state(route, paused, graduated, allowance_override,
override_expires_at, …)`. §9.3's override is **per domain group**, which a table
keyed on route alone cannot represent.

**Decision:** `quota_usage` gains `allowance_override`, alongside the scheduled
`allowance`. `route_state` keeps only `paused` and `graduated`. No
`route_group_override` table — O-3's working assumption is dropped as unnecessary.

**Why:** the row is already keyed `(route, domain_group, day_index)`, which is
exactly the scope §9.3 describes, so **the override expires at the day boundary
by construction** — tomorrow is a different row. That removes
`override_expires_at`, removes an expiry sweeper, and removes the question O-3
raised about what "the next day boundary" means for a route with no warm-up.
Keeping both columns rather than overwriting `allowance` leaves the mutation
auditable, which §9.3 wants when it logs who did it.

### D-026 — `quota_usage.allowance` is authoritative once written (settles O-4)

**Decision:** the ceiling is written when the row is created and never updated
from configuration thereafter. A schedule edit plus a restart applies from the
**next** day boundary, not retroactively. Mid-day changes go through §9.3's
admin endpoint, which writes `allowance_override`.

**Why:** the alternative lets a restart authorise a burst on a day that was
already half spent — which is the one thing this component exists to prevent.
`INSERT … ON CONFLICT DO UPDATE` deliberately does not use `EXCLUDED.allowance`.

*Phase 7 should log at startup where a stored allowance differs from what the
config would now compute, so the deferral is visible rather than mysterious.*

### D-027 — §3.2 step 3d's retry is dropped (settles O-5)

**Spec:** "If reservation fails due to a concurrent claim, re-evaluate this route
once, then skip."

**Decision:** no retry. `INSERT … ON CONFLICT DO UPDATE` takes a row lock, so
contenders for one `(route, domain_group, day_index)` are serialised: by the time
a session reads the row it is reading the truth, and there is no lost claim to
re-evaluate. A lock-wait timeout is a database failure per §7.5.

`DO UPDATE` rather than `DO NOTHING` is load-bearing — `DO NOTHING` returns no row
on conflict and takes no lock, which would leave contenders unserialised and
reintroduce the exact race §7.4 exists to close.

### D-028 — `550 5.6.0` fires when *any* rule needs the `From:` header

*Implemented in phase 2, recorded here — it only became load-bearing once the
chain walk gave `resolve_chain` a second caller.*

**Spec:** §5.4 — a malformed `From:` "causes `550 5.6.0` **when `match_on`
requires it**".

**Decision:** the check is "does any configured rule use `from_header` or
`either`", not "does the rule that would have matched".

**Why:** a rule cannot be known to match until the header it tests has been
parsed, so the narrower reading is unimplementable. Consequence worth stating:
with a mixed rule set, a message with no `From:` is rejected even if an
envelope-only rule further down the list would have matched it. That is the
conservative reading; the other silently changes which rule applies based on a
header being malformed.

### D-029 — `recipient_event` is deferred to phase 6

**Spec:** §11 lists it; D-014 said the quota tables land in phase 3.

**Decision:** phase 3 creates `quota_usage`, `quota_reservation` and
`route_state` only. `recipient_event` lands in phase 6, where §7.3's hashing,
normalisation and sweeper are designed.

**Why:** the same reasoning D-014 used to defer these tables out of phase 1 — its
row shape depends on decisions that have not been made, and creating it now would
bake in an answer.

### D-030 — A fifth skip reason, `not_started`

**Spec:** §9.1 enumerates `simmer_route_skipped_total{route,reason}` with reason
`quota`, `frequency`, `paused`, `preflight`.

**Decision:** add `not_started`, for §7.2's "`warmup.started` in the future makes
the route ineligible". (`unknown_route` also exists, for a chain naming a route
§4.2 should already have rejected.)

**Why:** folding it into `quota` would be a lie an operator wastes an afternoon
on. "Out of quota" and "has not begun" call for opposite responses — wait, versus
check the configured start date.

### D-031 — Database tests use `#[sqlx::test]`, so `cargo test` needs a Postgres

**Decision:** the phase 3 storage tests run against real Postgres via
`#[sqlx::test]`, which creates a fresh database per test and applies
`migrations/`. `docker compose up -d simmer-db` plus `DATABASE_URL` is now part of
the documented development loop, and the compose file publishes the database on a
fixed `127.0.0.1:5433`. CI supplies the same thing as a `postgres:18` service on
the same host port, so `DATABASE_URL` has one spelling everywhere.

There is no fixture step and there should not be one: `#[sqlx::test]` applies
`migrations/` to each test database, so the migrations *are* the fixtures and every
test builds the state it needs. Shared seed data would give the tests a hidden
dependency on each other, which is the first thing that goes wrong in a suite whose
assertions are counters.

**Why:** §12.3's concurrency requirement — "N concurrent sessions against a route
with N−1 remaining allowance; assert exactly N−1 delivered and no overshoot" — is
the test the whole three-phase protocol exists for, and it is a claim about what
two transactions do to one row simultaneously. Against a fake it proves nothing.
A post-hoc increment passes every other test in the suite and fails this one.

**Cost:** `cargo test` is no longer dependency-free. The ingress and reply-mapping
suites are unaffected: they use an in-memory `QuotaStore` (§11's trait), which is
test scaffolding rather than the "alternative backend" §11 says is not implemented
in v1.

*This uses `sqlx::test`'s runtime database creation, not `query_as!`'s
compile-time verification. `CLAUDE.md`'s ban is on the latter, which would need
`DATABASE_URL` during the Docker build; nothing here affects the image.*

---

## Phase 4

### D-034 — An unknown template variable is a fatal startup error

**Spec:** §6.3 lists the available variables. §4.2's list of startup violations
does not mention templates at all, so on a literal reading `{{original.frm.address}}`
is neither valid nor invalid — it is simply not covered.

**Decision:** `rewrite::template::Template::parse` rejects any name outside §6.3's
table, and `check_identity` reports the failure as a §4.2 violation naming the
YAML key it came from. Reported alongside every other violation, per §4.2's
"report all violations, not just the first".

**Why:** the alternative — rendering an unknown variable as the empty string,
which is what §6.3 does for *absent values* — makes a typo invisible. A misspelt
variable in `set_headers.From` would emit `From: <sales@newbrand.com>` with no
display name, on every message, for as long as nobody looked at a delivered
message closely. Simmer's whole job is to build reputation on a domain, and doing
it under a subtly malformed identity for a fortnight is the expensive failure.
Startup is the cheap place to find it.

The empty-string treatment stays for the cases §6.3 actually specifies —
`original.header["X-Absent"]`, `original.message_id` on a message with none.
Those are properties of the *message*, which arrives long after startup and must
not fail a message already accepted from a client.

### D-035 — The null sender is never rewritten

**Spec:** §6.2 makes the envelope `MAIL FROM` an absolute assignment from
`identity.envelope_from`, with no exceptions stated.

**Decision:** an incoming `MAIL FROM:<>` is forwarded as `<>`. The template is
not rendered.

**Why:** RFC 5321 §6.1 requires the null reverse-path on delivery status
notifications, and it is what stops a bounce from being bounceable. Assigning
`bounce@newbrand.com` to a bounce makes a mail loop a live possibility, and the
loop would be between two systems neither of which is Simmer.

It is also the §1.1-correct answer, which is why it is a decision rather than a
carve-out: an application sending its own notifications directly to the provider
would use `<>` too, so passing it through is exactly what "output identity must be
exactly expressible as application-side configuration" requires. Rewriting it
would be Simmer emitting mail the application could not.

### D-036 — `SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2

> **Settled 2026-08-11 by the spec's author: the rule is right and the example was
> wrong.** `SPEC.md` §4.1's example now reads `bounce@newbrand.com`, and §4.2
> carries a note recording what it used to say and why that was prohibited. The
> stronger rule this finding led to is D-069.

**Not a decision — a finding, and one for the spec's author.**

§4.1's example sets, on the warming route:

```yaml
envelope_from: "bounce+{{original.envelope_from.local}}@newbrand.com"
```

That is a **relative transformation**, which §1.1 constraint 1 prohibits by name:
"Append `.new` to the sending domain is not permitted, because applying it to
already-migrated traffic corrupts it." Composed with itself it gives
`bounce+bounce+jane@newbrand.com`, and again on the next pass. §6.6 classes
`envelope_from` as an identity field, so §4.2 makes it "a fatal startup error with
no override".

The §4.2 stability check, implemented in this phase, therefore refuses to start on
the spec's own example. `tests/rewrite_stability.rs::a_relative_transformation_is_caught`
demonstrates the composition directly.

**What we did:** changed the shipped `simmer.yaml` to `bounce@newbrand.com` with a
comment explaining the removal, and left `SPEC.md` untouched. The engine is right
and the example is wrong; which of the two the author wants to change is not ours
to decide.

**Worth knowing if the intent was VERP:** per-sender return paths cannot be
expressed absolutely from a field the route rewrites. They *can* be expressed from
one it does not — `bounce+{{recipient.local}}@newbrand.com` is stable — which when
this was written cost per-recipient splitting (§6.3, O-9). **Since D-047 it costs
nothing:** every transaction has exactly one recipient, so `recipient.*` always
renders a real value and there is no splitting left to force.

### D-037 — `Received:` is the only header Simmer adds unbidden

**Spec:** §6.1 step 8 requires a prepended `Received:`. D-002 additionally
excludes "the `X-Simmer-*` headers" from §12.3's byte-equivalence comparison,
which implies they exist.

**Decision:** the engine emits exactly one header of its own, the `Received:`.
It emits no `X-Simmer-*` header. An operator who wants them writes them in
`set_headers`, where they are ordinary configuration — the shipped `simmer.yaml`
does exactly that.

**Why:** §1.1 says "neither ordering may change what the recipient sees", and
every header Simmer adds on its own initiative is a header that vanishes the day
Simmer is unplugged. D-002 already admits the `Received:` as an operationally
essential exception; making it the *only* one keeps the exception as small as it
can be. D-002's exclusion of `X-Simmer-*` still stands and still matters, because
a configuration that sets them is entitled to expect the comparison to allow it.

### D-038 — §6.3 conformance is applied at substitution boundaries, not to the finished value

**Spec:** §6.3: "Rendered header values must be RFC 5322-conformant; non-ASCII in
display names is RFC 2047-encoded automatically."

**Decision:** quoting and encoding are applied to each substituted variable as it
is placed, with the surrounding literal text used to decide what position it
lands in — display name, `addr-spec`, or inside operator-supplied quotes. The
finished string is then conformed as a whole.

**Why:** the property test found it. A display name of `Smith, Jane` rendered into
`{{original.from.display_name}} <sales@newbrand.com>` produces
`Smith, Jane <sales@newbrand.com>`, which is not one mailbox with a comma in its
name — it is two mailboxes, and a second pass reads the display name as `Smith`.
By the time the value is a flat string that comma is indistinguishable from one
the operator wrote deliberately to separate two addresses, so no amount of
cleverness afterwards can recover the distinction. Escaping at the boundary is the
standard injection fix and the only correct one here: operator text stays grammar,
message-derived text stays a value.

The position analysis is what keeps it safe. `{{original.from.address}}` alone in
`Reply-To:` is an address, not a phrase, and quoting it would break it; a variable
inside `<…>` is part of the `addr-spec`. The rule is that a value is a display
name when an `<` follows it before any top-level comma, which is precisely RFC
5322's `name-addr` production.

### D-039 — The header block is edited in place; the body is opaque

**Spec:** §6.1 step 2 says "parse into headers and MIME structure".

**Decision:** phase 4 splits the message at the header/body separator, edits the
header block as a list of fields that carry their *original bytes*, and
concatenates the body back untouched. Only headers a route actually names are
re-serialised. No MIME parsing happens, and `mail-parser` is used to *read*
values — `{{original.subject}}` is specified as the decoded subject — never to
write them.

**Why:** §12.3 compares raw downstream output byte for byte. A message that
arrived with `Subject:  two  spaces`, a lowercase `message-id:`, or a
tab-indented continuation must leave with all three intact, because none of them
is something Simmer was configured to change. Round-tripping through a parsed
representation normalises exactly those things, invisibly, on every message.

Carrying original bytes also makes "the body arrives unaltered" structurally true
rather than a property that has to be tested for every message shape. §6.4 body
rewriting (phase 5) is where the body stops being opaque, and it should open it
deliberately rather than inherit an already-reserialised message.

### D-040 — A stale `unstable_headers` declaration is a `WARN`, not a violation

**Spec:** §6.6: "Naming a header that is in fact stable is also a startup `WARN` —
it means either the declaration is stale or the intent was misunderstood, and both
are worth surfacing."

**Decision:** naming a header the route never sets is now a startup warning.
**This supersedes phase 1**, which made it a fatal violation.

**Why:** phase 1 had no stability engine, so "declared but stable" was not
computable and "declared but never set" was the only approximation available —
and it was made fatal, which the spec does not ask for. A header nothing writes is
stable by definition, so it is exactly the case §6.6 assigns a `WARN`. Now that
the probe exists, both halves of the sentence are implementable and the spec's
severity applies to both.

Worth noting because it *loosens* validation: a configuration that phase 1 refused
to start on will now start, with a warning.

### D-041 — Header lines are folded only at RFC 5322's hard limit, never at 78

**Spec:** silent. RFC 5322 §2.1.1 makes 998 octets a MUST and 78 a SHOULD.

**Decision:** a header Simmer writes is emitted on one line unless that line would
exceed 998 octets, in which case it is folded at spaces.

**Why:** folding is the one transformation here that a downstream, a filter or a
spam scorer can observe *and* that has no single right answer — where to break is
a choice, and any choice becomes part of the bytes §12.3 compares. Folding only at
the hard limit means the common case emits exactly what the operator's template
describes and the uncommon case stays legal. The fold point is always a space and
unfolding restores that space, so §6.6's property survives it.

---

## Phase 5

### D-043 — The MIME structure is walked as byte ranges, and only matched spans are rebuilt

**Spec:** §6.4 describes a per-part decode/rewrite/re-encode and says nothing about
what happens to the parts that do not match.

**Decision:** the body is located as **ranges** — `src/rewrite/mime.rs` returns the
byte span of each `text/*` part rather than its content — and `body.rs` rebuilds
the body by copying every span it did not rewrite verbatim. A part no rule matches
is never decoded-and-re-encoded, and a body in which no part matched is returned as
`None`, so the caller splices the original slice straight back.

**Why:** D-039 made "nothing Simmer was not configured to change is changed"
structurally true for the body by never touching it. §6.4 ends that, and the
obvious replacement is a test. Ranges give something better: the guarantee narrows
rather than disappearing. It still holds structurally for every part that did not
match, and only the spans the operator's configuration actually hit are rebuilt.

That distinction is not cosmetic. **Re-encoding is not the identity.** A
quoted-printable part re-wrapped by our encoder rather than the client's is
equivalent but not equal, and §12.3 compares bytes. Confining re-encoding to the
matched parts means the difference exists exactly where the operator asked for one
and nowhere else.

`mail-parser` does expose `offset_body`/`offset_end` per part and was the first
approach tried. It was dropped for two reasons: it decodes every part as it parses,
which doubles the peak memory of a large message to obtain structure a scan can
give; and it has to be handed the whole message, which would mean re-serialising
the header block §6.1 steps 4–6 have just edited before step 7 could read a
`Content-Type` those steps might have set. Walking the edited `HeaderBlock` and the
untouched body avoids both.

**Consequences:** `message/rfc822` and any part carrying
`Content-Disposition: attachment` are not descended into and produce no counter —
§6.4's first sentence puts attachments outside the scope, so they are not "skipped"
from a scope they were never in. `multipart/signed`, `multipart/encrypted` and
`application/pkcs7-*` *are* counted, because those are §6.4 protecting a signature
and an operator whose rewrite silently stops applying to signed mail should see it.

### D-044 — Four charsets are supported; every other charset is §6.4's "unknown charset"

**Spec:** §6.4 says "decode the charset to UTF-8", and also says "if a part cannot
be decoded (unknown charset, malformed encoding), leave it untouched, log at
`WARN`, and increment `simmer_body_rewrite_skipped_total`".

**Decision:** UTF-8, US-ASCII, ISO-8859-1 and Windows-1252 are decoded and encoded
exactly, by hand, in `src/rewrite/charset.rs`. Everything else takes the second
sentence: the part is left untouched, warned about, and counted with
`reason="unsupported_charset"`. **No new dependency was taken.**

**Why:** the line has to be drawn somewhere, and the spec supplies the behaviour
for whatever falls outside it, so this is a scope choice rather than a gap. It is
drawn at what an application generating its own mail actually emits.

`encoding_rs` — reachable through `mail-parser`'s `full_encoding` feature — would
decode far more. It was rejected on its *encoder*, not its licence (which is
clean; see `LICENSES.md` §4): for a character the target charset cannot represent
it emits a numeric character reference, which is right for HTML form submission and
puts a literal `&#8212;` in the middle of an email body. Round-tripping a part
faithfully matters more here than covering Shift-JIS, and adding it later is a
change to one file.

US-ASCII is deliberately decoded *as* UTF-8. The two agree on every byte a
conforming us-ascii part can contain, and mail that labels UTF-8 content as
us-ascii is common enough that refusing it would skip rewrites that are perfectly
safe. A part whose label is us-ascii and whose bytes are neither ASCII nor valid
UTF-8 still fails to decode, which is the honest answer.

### D-045 — A part's transfer encoding and charset are never changed

**Spec:** §6.4 says to "re-encode, and fix up `Content-Transfer-Encoding` and any
length-bearing headers", which anticipates the encoding changing.

**Decision:** a rewritten part is always written back in the charset and the
transfer encoding it arrived with. If the rewritten text cannot be — a
`body_rewrites` replacement containing a character the part's charset has no room
for, or a non-ASCII character entering a part that genuinely was `7bit` clean — the
part is left untouched, warned about, and counted with
`reason="unrepresentable"`. So the `Content-Transfer-Encoding` fix-up §6.4 asks for
is never needed. Length-bearing headers (`Content-Length`, `Lines`) **are** fixed
up when the part carries them, because those are statements about the part that a
rewrite can falsify.

**Why:** §1.1. Simmer's output has to stay "exactly expressible as application-side
configuration", and re-framing a part from `7bit` to `quoted-printable` — or
relabelling its charset — is not something the operator could hand back to the
application as a setting. Not needing the fix-up is a better answer than performing
it well.

The branch is close to dead in practice: a replacement string is operator-written
and is essentially always ASCII, which every supported charset can carry. It exists
so that the one case where it is not has a defined, observable outcome instead of
mojibake.

One softening: the 7bit check applies only to a part that was **itself** 7-bit
clean. A part carrying high bytes under a `7bit` label was already breaking its own
declaration before Simmer saw it, and §6.4's exclusions are about what Simmer would
damage, not about policing the sender.

### D-046 — Unstable `body_rewrites` are a fatal startup error, with no override

**Spec:** §6.6 classifies stability violations into identity fields (fatal, no
override) and "all other headers" (fatal, downgradable via `unstable_headers`). The
body is neither.

**Decision:** at startup, a route's `body_rewrites` must be a **fixed point**:
applying the whole chain to its own output must not change it again. A violation is
a fatal startup error and there is nothing to declare that downgrades it.

Detection is `body::Rules::fixed_point_violation`. It builds a probe from the
rules' own **replacements** — that is what an unstable rule re-matches — and
compares `apply(probe)` with `apply(apply(probe))`. Comparing against `probe`
itself would be wrong: a chain where rule 2 consumes rule 1's output is perfectly
stable and must not be reported, and comparing the two successive results is what
distinguishes the two cases. `s/a/aa/` is caught; `s/a/b/` then `s/b/c/` is not.

**Why fatal rather than a warning.** `s/a/aa/` matches what it just wrote, so every
pass through Simmer grows the body. That is §1.1 constraint 1 — "append `.new` to
the sending domain is not permitted, because applying it to already-migrated
traffic corrupts it" — in a different place, and it corrupts traffic from an
application that has already been cut over. Unlike `Reply-To`, there is no
migration-only reading of it: nobody configures a rule intending it to apply twice.

**Why no escape hatch.** `unstable_headers` names headers, and a body is not a
header. Adding an `unstable_body_rewrites` acknowledgement would be inventing
configuration `SPEC.md` does not have, which needs the spec's author rather than a
decision here. If a real case for one turns up, that is the conversation to have.

This is a §4.2 rule the spec does not list, in the same way D-034 is.

---

## Phase 6

### D-047 — Multi-recipient transactions are refused outright, and §13 phase 9 is void

> **Confirmed 2026-08-11 by the spec's author.** `SPEC.md` §5.6 has been rewritten
> to specify the refusal unconditionally; the `single_recipient_only` key, the
> result-collapse table and §13's phase 9 are deleted from the spec, and §6.3's
> warning about `recipient.*` templates forcing a split goes with them. This is no
> longer a divergence — it is what the specification says. `docs/RECIPIENTS.md`
> remains the long form of the reasoning. O-8 and O-9 stay dissolved.

**Spec:** §5.6 makes `single_recipient_only` a switch defaulting to true and
specifies, for the `false` case, splitting by route and a four-row table collapsing
the per-recipient outcomes into one reply. §4.1 lists the key; §13 phase 9 builds
the machinery; §9.1 counts the mixed outcome as `simmer_partial_delivery_total`.

**Decision:** a second `RCPT TO` is refused with `452 4.5.3 multiple recipients not
permitted`, unconditionally. `single_recipient_only` is deleted from the schema —
a configuration still carrying it is refused at startup by name (`validate::
removed_keys`), rather than by serde's bare "unknown field". §13 phase 9 is void:
there is no splitting and no collapse table.

**Why.** §5.6's own last paragraph is the argument: "The client is not told which
recipients failed, because SMTP provides no way to say so in a single reply. This
lossiness is the reason the switch defaults to rejecting multi-recipient messages."
Two of its four rows are worse than lossy. `250 partially accepted` empties the
client's queue while some recipients silently got nothing — and Simmer is not an
MTA (§2.2), so nothing generates a DSN for them. And `550` when *any* split failed
permanently is emitted about the transaction but recorded by the client against
every recipient in it, which is §14.1's prohibition exactly.

Keeping it as a switch would mean building, testing and maintaining a path whose
correct answer is "do not use this", to save round trips on an ingress leg inside
our own network.

**What it dissolves** — not settles; the cases stop existing:

- **O-9** — one recipient per transaction, so there is nothing to group and no
  granularity to choose between §5.6's route-splitting and §6.3's.
- **O-8** — there is no collapse table to return `550` from.
- §6.3's "single-recipient case only" caveat on `recipient.*`, which is now the
  only case. The §6.3 startup warning in `validate::warnings` is deleted, and
  D-036's stable VERP spelling `bounce+{{recipient.local}}@newbrand.com` becomes
  available at no cost — the cost D-036 named *was* O-9.

**What it costs.** An application that batches recipients must be changed before it
can sit behind Simmer. Not a §1.1 cost: the invariant constrains Simmer's *output*,
and one message per recipient is exactly what the application keeps doing once
Simmer is unplugged. `max_recipients` becomes vestigial — kept because §4.1
mandates it, warned about above 1, and `reply::too_many_recipients` deleted rather
than left unreachable, because a dead reply in `src/smtp/reply.rs` is what that
file's enumeration test exists to catch.

**This needs the spec's author.** §5.6, §4.1, §13 phase 9 and §9.1's
`simmer_partial_delivery_total` all describe behaviour that no longer exists. Long
form, with the alternatives and what stays open, in `docs/RECIPIENTS.md`.

### D-048 — `recipient_event`'s row shape, which D-029 deferred to here

**Spec:** §11 gives it as `recipient_event(recipient_hash, route, sent_at)`,
"indexed on `(recipient_hash, sent_at)` and on `sent_at` for the sweeper. This is
the high-cardinality table." §7.3 says the key is "a **salted hash** of the
normalised value, never plaintext … This bounds row size".

**Decision:** exactly those three columns, with no primary key and no surrogate
id. `recipient_hash` is `BYTEA` holding **HMAC-SHA256 truncated to 16 bytes**. The
lookup index is `(recipient_hash, route, sent_at)` rather than §11's
`(recipient_hash, sent_at)`.

**Why each part:**

- **Keyed hash, not a plain digest.** An unkeyed SHA-256 of an address is
  reversible by anyone with a word list, which would leave §7.3's "avoids the
  container accumulating a plaintext record of every address mailed" true only in
  the most literal sense. HMAC under a persisted salt is what makes the table
  useless to someone who has only the table.
- **16 bytes, not 32.** §7.3 asks the key to bound row size, and this is the
  table §11 calls high-cardinality. 128 bits is far past what a collision needs to
  be unlikely, and a collision costs one message steered to the next link — not
  anything permanent, and not anything the recipient sees.
- **`route` in the index.** Every read is "this route, this recipient, since this
  instant", because the constraint is per route (D-051). §11's two-column form
  would work and then filter; adding the column keeps it an index scan. §11 calls
  its tables "indicative", so this is a refinement rather than a divergence.
- **No primary key.** The table is append-only and swept by age. A surrogate key
  would be an index to maintain for nobody's benefit, and there is no natural key:
  two messages to one recipient in the same second are two real events.

### D-049 — The frequency check reads outside the reservation transaction

**Spec:** §3.2 3b puts the frequency check *before* the quota check. §7.4 makes
the headroom check and the reservation one operation under a row lock.

**Decision:** the frequency count is an unlocked read of `recipient_event`, taken
immediately before the reserve and outside its transaction. Only the quota half
holds the lock.

**Why.** The two checks fail differently. Quota is a **ceiling**: overshooting it
is the one thing Simmer exists to prevent, so §7.4 pays for a row lock and a
three-phase protocol. Frequency is a **steering rule** — §7.3 says so twice — and
the cost of a race is that two concurrent messages to one recipient both read "2"
against a threshold of 3 and both go out on the warming route. That is one extra
message, on a route that is *under* its quota, to a recipient who was going to
receive it from the overflow route anyway. Nothing is dropped and the ramp is not
corrupted.

Against that: `recipient_event` is the high-cardinality table, and putting a read
of it inside the transaction that holds `quota_usage`'s row lock would serialise
every message on the route behind it. The lock is the ramp's chokepoint by design;
lengthening it to close a race whose worst case is "one extra message" is the
wrong trade.

**What follows:** the check is also *not* re-evaluated after the reservation, and
§3.2 3d's "re-evaluate this route once" still does not apply (D-027).

### D-050 — The salt is minted idempotently and resolved lazily

**Spec:** §7.3: "The salt is generated once and persisted." §11 puts
`instance_config(key, value)` there for "the recipient hash salt and similar
singletons". §7.5: an unreachable database means `451`, not a refusal to start.

**Decision:** `instance_config('recipient_hash_salt')`, written with `INSERT …
ON CONFLICT (key) DO NOTHING` followed by a read, so whoever inserts first wins
and every replica uses that one. It is resolved on **first use**, not at startup —
`main` warms it best-effort and logs, but a failure there is not fatal.

**Why idempotent:** "generated once" has to hold across replicas, not just across
restarts. Two instances each minting their own would give the two of them
different keys for one recipient, and the windows would silently disagree.

**Why lazy:** loading it in `main` would make a database that is merely late into
a process that will not start, which contradicts §7.5's whole posture. Resolved on
the message path, a failure becomes the same `451 4.3.0 quota service
unavailable` every other storage failure produces, and the first message after the
database returns picks it up. A deployment with no `recipient_frequency` anywhere
never asks for it at all.

**Why not `rand`:** two v4 UUIDs are 32 bytes of `getrandom` output, and `uuid` is
already a direct dependency. See `LICENSES.md` §6.

### D-051 — Events are counted and recorded per route, and only for constrained routes

**Spec:** §7.3 makes the constraint per route. §7.4 phase 3: "On downstream `2xx`,
move the count from `reserved` to `committed` and record recipient-frequency
events." §11's row carries `route`.

**Decision:** a window counts only that route's own events, and a row is written
only when the selected route declares a `recipient_frequency` — in the same
transaction as the commit.

**Why per route:** §11 put `route` in the row and §7.3 declares the threshold on a
route. A global count would make that column meaningless.

**Worth knowing, because it is the surprising half:** a message that fell through
to the overflow route does **not** count against the warming route's window. The
recipient did receive it, so an argument exists for counting real inbox pressure
across every route. That argument needs a spec decision rather than an
implementation one — the row shape leaves it open, since a global count is the
same table read without the `route` predicate.

**Why only constrained routes:** rows nothing will ever read are exactly the
accumulation §7.3 exists to prevent. An unconstrained route is not consulted and
so should not be recorded.

**Why in the commit's transaction:** it is one sentence in §7.4 and it should be
one transaction. A delivered message whose event was lost under-counts a window
silently; an event recorded for a message that did not commit over-counts it.
Neither is reachable if they cannot be separated — which is why
`QuotaStore::commit` takes the keys rather than exposing a second method the relay
could forget to call.

### D-052 — A constraint on the last link of a chain is a startup warning

**Spec:** §4.2's list does not mention it. §7.3 says the constraint makes a route
ineligible "so the message falls through to the next link".

**Decision:** `config::validate::warnings` emits a `WARN` when the last route of
any chain — a sender rule's or the `default_chain` — carries a
`recipient_frequency`. Not a violation.

**Why warn:** on the last link there is no next link, so the rule stops steering
and starts refusing: a recipient over threshold gets §10.3's `451` instead of
another route. Nothing about that is invisible in the config, but it is invisible
in *behaviour* until the first recipient reaches the threshold, which may be weeks
after the deployment.

**Why not a violation:** it is the only way to express "never mail this person
more than twice a day, full stop", and that is a legitimate thing to want. §4.2 is
a list of things that are *wrong*, and this is a thing that is usually
unintended — D-040's distinction exactly.

---

## Phase 7 — the control plane

### D-053 — Admin tokens are named (settles O-11)

**Spec:** §9.3 — "All require the bearer token. All mutations are logged at
`INFO` with the acting token's identifier." §4.1's `admin.auth_token` is a single
scalar, which has no identifier. That gap is O-11, carried since phase 1.

**Decision:** `admin.tokens` — an optional list of `{name, token}` — alongside
`auth_token`, which keeps working exactly as §4.1 specifies and is the token
named `default`. The audit line records the name of whichever token was
presented, never the token. §4.2 gains four rules: at least one credential must
exist, names and secrets must each be non-empty, names must be unique, and **no
two credentials may share a secret**. A short token is a `WARN`, not a violation.

**Why not just drop §9.3's wording.** It asks for accountability, and with one
shared credential there is none to be had — but the fix is cheap and additive,
and the alternative is a log line that says `admin` on every mutation and
therefore says nothing. An operator investigating "who paused the warming route
at 02:14" needs a name.

**Why the shared-secret rule.** §9.3 identifies the actor *by the token
presented*. Two names behind one secret make the audit line a coin flip between
them, which is worse than one honest name because it looks like evidence.

**Why not a token fingerprint instead.** Hashing the presented token and logging
eight hex characters identifies the credential rather than the person, needs no
schema change, and was the cheaper option. It was rejected because the thing an
operator wants from an audit trail is a name they recognise, and a fingerprint
only becomes one after somebody writes down the mapping — which is the config
change this decision makes, minus the tooling.

**Why a length *warning* rather than a rule.** §4.1 sets no length and every test
fixture in the repository predates this decision. The warning names the
consequence rather than the policy: §9.3 can pause a route or zero an allowance,
and either answers every affected message `451`.

### D-054 — `simmer_messages_total` gains the `domain_group` §9.1 specifies

**Spec:** §9.1 — `simmer_messages_total{route,domain_group,result}`.

**Context:** D-021 said phase 7 would install a recorder and that "no call site
changes". One did.

**Decision:** `metrics::message` takes the domain group. The single call site is
in `relay.rs`, immediately after the chain walk, which is exactly the code whose
job is to produce the `(route, domain_group)` pair the quota is keyed on.

**Why the placeholder existed:** phase 2 wrote the function before §3.2 step 2
existed to resolve a group, and emitted `"-"` deliberately — "an empty label
value and an absent label are different series in Prometheus, and a placeholder
that later becomes a real value is easier to spot than a blank". It was spotted.
Keeping it would have shipped a metric that §9.1 specifies with three labels and
that carries two useful ones, which is the sort of thing nobody notices until
they are trying to answer "is the Google ramp the one that is deferring".

D-021's prediction was otherwise correct: every other call site is untouched, and
`tests/metrics_endpoint.rs` asserts that the counters written in phases 2–6
against a null recorder produce real series once one is installed.

### D-055 — §9.2's reads require the bearer token; `/metrics` does not

**Spec:** §9.3 says "All require the bearer token" of the *write* endpoints,
which scopes authentication to mutations. §9.2 lists the reads with no such
sentence. §12.2 says the container must not expose port 25 by default but says
nothing about the admin listener.

**Decision:**

| Open | Token required |
|---|---|
| `GET /health`, `GET /healthcheck`, `GET /metrics` | everything else |

So §9.2's `/routes`, `/routes/{name}` and `/quota`, and §9.4's `/dryrun`, need
the token even though §9.3's sentence does not reach them.

**Why wider than the spec.** `/routes` discloses every downstream provider
hostname, the TLS posture of each, the whole routing shape and the live state of
the ramp. `/dryrun` runs the real rewrite engine over attacker-chosen input.
§2.3's trusted-segment assumption is an assumption; nothing enforces it, and the
cost of it being wrong is asymmetric.

**Why `/metrics` stays open.** A Prometheus scrape config that has to carry a
credential usually does not, and the failure mode is a dashboard that is silently
blind — which is its own outage, arriving at the moment the instrument is wanted.
The exposition carries no recipient data by construction (§7.3, and
`simmer_recipient_events_evicted_total` is unlabelled for exactly this reason).

**Why `/health` stays open.** It is what Docker's `HEALTHCHECK` and a load
balancer drive, neither of which can hold a secret usefully.

The comparison is constant-time (`subtle`), and does not stop at the first match.
An admin token that can pause a route deserves the care §5.3 already takes over
passwords — and see the `smtp/auth.rs` defect below for what happens when that
reasoning is applied to one comparison and not another.

### D-056 — The §7 gauges are refreshed from storage on every scrape

**Spec:** §9.1 lists `simmer_quota_allowance`, `_committed` and `_reserved` as
gauges, and `simmer_quota_allowance` as "today's ceiling".

**Decision:** `GET /metrics` recomputes them from the same §9.2 projection
`/routes` uses, before rendering.

**Why:** the gauges are otherwise only ever `set` by a message that relayed. A
route that has sent nothing today would therefore export *yesterday's* numbers
under a label set claiming to describe today — the D-026 trap, in the one place
an operator is least likely to check because a graph looks like a measurement.
Computing both endpoints from one projection also means `/metrics` and `/routes`
cannot disagree.

A storage failure logs and renders anyway. A metrics endpoint that fails during
an outage removes the instrument at the moment it is wanted.

### D-057 — §9.3's mutations warn rather than refuse when they exhaust a chain

**Spec:** §9.3 offers pause and a per-group allowance override. §14.1: Simmer
must never emit a reply that makes a client record permanent state.

**Decision:** an allowance of `0`, and a pause that leaves a chain with nothing
eligible, are both **allowed**. Every mutation computes which
`(chain, domain group)` pairs it has just left with no eligible route, returns
them in a `warnings` array, and logs each at `WARN`.

**Why allowed:** an override of zero is the only way to stop one domain group
without pausing the whole route, and §9.3 implies the capability. Refusing it
would push an operator toward `pause`, which produces a blunter version of the
same state with no warning at all.

**Why it is not a §14.1 violation:** nothing here changes the *class* of reply. A
paused route and a zeroed allowance both make a route ineligible; an ineligible
chain is §10.3; §10.3's default is `451`. The answer to the client stays
temporary however hard an operator leans on the write API. `reply.rs` is
untouched by this phase.

**What the warning deliberately does not model:** §7.3's frequency check, which
is per recipient. "This chain is exhausted" is not a property of the
configuration for it, and asserting one would be a guess dressed as a warning.

### D-058 — `POST /quota/reset` recomputes `reserved` rather than zeroing it

**Spec:** §9.3 — "reset counters for a route/group. Destructive; requires an
explicit confirmation field in the body."

**Decision:** under the row lock, set `committed` to zero and set `reserved` to
the sum of the reservation rows that are actually outstanding for that key. The
row is **not** deleted. The confirmation field is the literal string `"reset"`.

**Why not zero `reserved`:** a reset during a send would hand away headroom that
an in-flight message already owns, and the double-spend surfaces as an overshoot
of the day's ceiling — the one failure the §7.4 protocol exists to make
impossible. Recomputing repairs drift as a side effect.

**Why not delete the row:** `allowance` is authoritative once written (D-026), so
re-creating it would silently adopt whatever the schedule says *now*. A reset is
about the counters, not about the ceiling.

**Why a word rather than a boolean:** a `true` is something a script produces
without anyone having read the sentence explaining what is about to be
discarded.

### D-059 — §9.4's dry run runs the real engine, and counts nothing

**Spec:** §9.4 — "returns the routing decision… It sends nothing and takes no
reservation… should be treated as a first-class feature rather than a debugging
afterthought."

**Decision:** the sender match is `relay::resolve_chain`, the rewrite is
`rewrite::rewrite` with the route's compiled templates, and the chain walk is a
new read-only `chain::dry_walk` that mirrors `walk_and_reserve`'s order exactly —
paused, then §7.3 frequency, then §7.2's start instant, then headroom.
`tests/admin_api.rs` asserts the two produce identical evaluations across all
four skip reasons rather than trusting the comment that says so.

`dry_walk` deliberately does **not** increment `simmer_route_skipped_total`. That
series measures messages that were steered, and an operator testing a
configuration has steered none; inflating it would corrupt the series someone
would use to decide whether the ramp is working.

**What it cannot reproduce:** the race. The real walk checks headroom inside the
transaction holding the row lock; this reads outside any transaction, so it can
say "eligible" for a route another session empties a millisecond later. Same
direction of error as the §5.4 early check, harmless for the same reason —
nothing acts on it.

**One bug this caught in itself.** §9.4 takes "a `From:` header value", which is
the display-name form an operator pastes — `Jane <jane@oldbrand.com>`. §5.4
matches on the *address*, and the relay never sees anything else, because the
session runs `first_from_address` over the header block before building
`Senders`. Passing the header value through raw made every domain rule miss, and
the dry run confidently reported a fall-through to `default_chain` that would not
happen. Found by driving the endpoint against `simmer.yaml` in the running
container, which is the argument for the container gate in one incident: every
test in `tests/admin_api.rs` used a fixture whose rules matched on the envelope.
The fix is to call the relay's own parser; there is now a test with four spellings
of the same address against a `from_header` rule.

**Recipients.** §9.4 asks for a list; D-047 makes a real transaction carry one.
Each address is evaluated independently, as separate transactions would be, and
the response says so. The request body is the only place a plaintext address
enters the control plane; it is supplied by the operator, evaluated, and never
stored or logged above `DEBUG`. §7.3's constraint is on what the container
*accumulates*.

**Two body views, and why both.** `body_rewrites` reports each pattern's match
count against the sample as supplied — including the patterns that matched
nothing, which is the single most useful thing this endpoint can say.
`body_changed` and `skipped_parts` come from the real engine over the whole
message. For a plain-text sample they agree; for a MIME `message` a pattern that
fires in the first and not the second is one matching structure rather than
content, which is worth seeing rather than hiding.

### D-060 — `hs-utils` is removed; its one used module is copied in

**Context:** `hs-utils` had been a dependency since phase 3, taken by git tag with
`default-features = false`, for exactly one function:
`healthcheck::check_subcommand`. Nothing else in the crate ever used it. Config,
logging, the connection pool and the HTTP layer each diverge by an earlier
decision — D-004, D-006, D-005 and D-003 respectively — so a single stdlib-only
file was the entire coupling to the shared library.

**Decision:** copy that module into `src/healthcheck.rs` and drop the dependency.
The behaviour is identical: the same `[host] [port] [deps|--deps]` CLI surface in
any order, the same `/healthcheck` and `/healthcheck?deps=true` paths, the same
four-second read and write timeouts, the same `HTTP/1.1 200` prefix test, the
same `exit(0)`/`exit(1)`, and the same no-op when `argv[1] != "healthcheck"`.
Nothing about the Dockerfile's `HEALTHCHECK` or the compose healthcheck changes.

The only difference is that argument parsing is split into a private
`probe_from_args`, because `check_subcommand` ends in `process::exit` and a test
cannot survive that. It is now covered by ten tests — the inherited version had
none — including a real one-shot HTTP server asserting the exact request line for
both paths, every non-`200` status, nothing listening, and an unresolvable host.

**Why copy rather than keep the dependency.** The two are not equivalent in cost.
The dependency was a *git* dependency, which is why `deny.toml` carried an
`allow-git` entry and a `[licenses.private] ignore-sources` exemption, and why the
Dockerfile's `--locked` had a load-bearing justification: a git tag is mutable, so
without the lock a rebuild could silently pick up different code from the same
tag. Sixty lines of standard library, versus a mutable external reference and two
exemptions in the supply-chain gate, is not a close call for a component whose
entire argument is that it is temporary and auditable.

**What this buys:**

- **No git dependencies at all.** `deny.toml`'s `unknown-git = "deny"` now has no
  allow-list, so it denies *every* git dependency rather than all but one. Adding
  one becomes a decision made in the gate rather than in a Cargo.toml line.
- **One crate with no `license` field instead of two**, and that one is `simmer`
  itself, which declares `publish = false` to say so deliberately. The
  `ignore-sources` exemption is gone. `LICENSES.md` §2's finding shrinks to an
  upstream note.
- **`cargo build` needs no network access to a private repository**, so the build
  no longer depends on credentials for `github.com/Hikari-Systems`.

**What it costs.** A fix upstream in `hs-utils-rs` no longer arrives here. For
this module that is close to meaningless — it is a stdlib TCP probe whose
behaviour is pinned by the Dockerfile and by tests — but it is the real trade,
and it applies to any future divergence from the estate's shared code.

**What this does *not* change:** the storage layer still follows the
hikari-systems data-service pattern in every respect that matters — runtime
`sqlx` over `&PgPool`, `models/<entity>.rs` free functions, plain-SQL migrations
applied at startup, `TIMESTAMPTZ` and `DateTime<Utc>`. The pattern was always the
thing being followed; `hs-utils` was one library that happens to implement parts
of it, and simmer used almost none of them.

**Nothing was copied for config or logging**, because there was nothing to copy:
`src/config/` and `src/logging.rs` have been independent implementations since
phase 1 under D-004 and D-006. Their module comments now say the house helper
exists rather than that this crate declines to call it.

---

## Phase 8 — the DNS preflight

### D-063 — The preflight interval is a constant, not configuration

**Spec:** §6.7 — "Checks run at startup and on an interval (default 15 minutes)."
§4.1's `preflight:` block lists `enabled`, `spf_include`, `dkim_selector` and
`require_dmarc`, and no interval key. §6.7's prose also names `strict`, which
§4.1 omits but which the schema has carried since phase 1.

**Decision:** 15 minutes as a `const` in `src/preflight/mod.rs`. No config key.

**Why:** "default 15 minutes" implies an override, but §4.1 is the schema and it
defines nowhere to put one. Every config struct is `deny_unknown_fields`, so
inventing `preflight.interval` would be a schema divergence — and for a value with
no reader: the thing being observed is a DNS zone somebody edits by hand, and
nothing about SPF or DKIM provisioning changes on a timescale where 15 minutes
versus 5 matters. `strict` is the opposite case and is kept, because §6.7's prose
specifies its *behaviour* in detail; an interval has no behaviour to specify.

If an override is ever wanted, it belongs in §4.1 first.

### D-064 — A route whose identity domain is not a constant is not preflighted

> **Superseded in practice by D-069**, which makes that configuration a §4.2
> startup violation. The behaviour described below is unreachable through a valid
> configuration and is kept as defence — see D-069 for why it was not deleted.

**Spec:** §6.7 checks "the outbound identity's domain". §6.3 makes
`identity.envelope_from` a **template**.

**The problem, which the spec does not anticipate.** A template's domain need not
be a constant: `bounce@{{original.envelope_from.domain}}` means "whatever domain
the message arrived with", and there is then no single domain to check on a timer.
§6.6 does not rule this out — applied twice that template gives the same answer,
which is exactly the property §6.6 tests — so it is reachable configuration.

**Decision:** `preflight::literal_domain` takes the domain when the template's
domain part contains no `{{`. When it does not, the route is **not planned**:
preflight never runs for it, `/routes` reports `preflight: null`, no
`simmer_preflight_ok` series is published, and `config::validate::warnings` emits a
startup `WARN`. **Never a startup error**, including with `strict: true`.

**Why not check a rendered sample.** Rendering the template against the synthetic
probe `validate.rs` already uses would always produce an answer — about a domain no
real message uses. A check reporting confidently on the wrong domain is worse than
one that declines, particularly for a feature whose entire purpose (§6.5) is to
catch a discrepancy nothing else reveals.

**Why not a fatal error, which was the first proposal.** Two reasons, and the
second is the real one. §6.7's whole posture is that preflight must not be able to
stop the service — a config quirk is a poor exception to that. And the deeper
observation, raised while settling this: a *warming route whose identity domain
varies per message is incoherent regardless of preflight*. The ramp, the daily
allowance and reputation accrual all exist to build reputation for one domain, so
such a route is not warming anything and its quota counts a mixture. That argues
for a §4.2 rule on `envelope_from` itself, applying to every route rather than only
preflighted ones — a bigger change than phase 8, and one for the spec's author,
since §6.3's grammar permits the template today. **Recorded as an open item rather
than built.**

**The warning names the consequence for `strict` explicitly**, in those words:

> strict: true therefore has NO EFFECT here: the route is never made ineligible,
> because no check ever produces a verdict

Silence there was the failure mode. An operator sets `strict: true`, sees no error,
and believes a gate is protecting them that is not running at all — a §9-shaped lie
told by the config layer instead of the control plane.

### D-065 — Preflight is evaluated above §7.3 in the chain walk

**Spec:** §3.2 3b calls the recipient-frequency check "evaluated **first** — it can
eliminate routes outright". §6.7 says a failing `strict` check "makes the route
ineligible for selection" without saying where in the order.

**Decision:** `paused → preflight → frequency → not started → quota`, in both
`walk_and_reserve` and `dry_walk`. Confirmed with the repository's owner.

**Why:** preflight is an in-memory read of the last interval's result; §7.3 is a
high-cardinality indexed read against Postgres per recipient. The cheapest check
that can eliminate a route goes first, and the two can never disagree — a route
eliminated by preflight is eliminated whatever §7.3 would have said, so the order
changes cost and not outcome. It displaces §3.2 3b's "first" by one position, which
is why it is written down.

The `dry_walk` half is not optional: §9.4's whole product is *which reason* a route
was skipped for, and `tests/admin_api.rs` pins the two walks against each other
step for step.

### What phase 8 also decided, without needing an entry

- **Fail open when nothing is known.** A `strict` route with no report yet — the
  first pass has not finished, or the resolver could not be built at all — is
  **eligible**. Treating "not yet checked" as a failure would turn a slow resolver
  at boot into a chain-wide outage, the exact outcome §6.7's non-blocking default
  exists to prevent. `tests/preflight.rs` asserts it.
- **`strict: true` on the last link of a chain is a startup `WARN`**, D-052's
  reasoning transplanted: with no next link the rule stops steering and starts
  refusing, so a DNS problem answers `451` instead of routing around itself.
- **A TXT record's character-strings are concatenated** before matching. Not
  pedantry: a 2048-bit DKIM key does not fit in one 255-byte string, so *every*
  real DKIM record arrives split, and a resolver returning them separately would
  find `p=` truncated and report a working selector as broken.
- **An empty `p=` fails the DKIM check.** That is precisely how a *revoked* key is
  published, and it resolves — so a test for mere existence would pass while every
  message went unsigned, which is §6.5's failure exactly.
- **`hickory-resolver` is taken at plain DNS**, with `__tls`, `__quic`, `__https`
  and `dnssec-*` all off. SPF, DKIM and DMARC records are public TXT lookups
  against the container's own resolver; DoT/DoH/DNSSEC would add a TLS stack and a
  trust decision to fetch data that is public by design, whose failure mode is
  already non-blocking.

---

## Phase 10 (partly built in phase 4)

### D-032 — The acceptance harness is a compose profile with two Mailpit traps

**Spec:** §12.3 requires an acceptance tier — "compose stack with real Postgres; a
warm-up walked across simulated day boundaries by manipulating `warmup.started`;
verifying fall-through to overflow at exhaustion; verifying the cutover invariant
by sending the same logical message under both arrangements of §1.1 and asserting
byte-equivalent downstream output". §13.10 places it in phase 10. Neither says how.

**Decision:** designed in full in `docs/ACCEPTANCE.md`. The load-bearing calls:

- **Two downstream traps, not one** — `trap-warming` and `trap-overflow`, separate
  Mailpit containers. Which container holds the message is what makes "delivered
  to the correct downstream" a physical fact. With a single sink the only
  available evidence is headers Simmer itself wrote, which proves Simmer *said*
  it used a route, not that it did.
- **Mailpit** (`axllent/mailpit`, MIT, pinned by digest) over MailHog. Both would
  serve; Mailpit is maintained, and it can be made to require STARTTLS and SMTP
  AUTH, which is what lets the suite close `STATE.md`'s largest gap — no test
  anywhere exercises §8.2's `required_verify` against a real certificate.
- **The bulk sender is a container**, because §2.3 says port 25 must not be
  published to a host interface and `docker-compose.yml` deliberately does not
  publish it. A host-side sender would mean punching a hole in the one file that
  documents why it should stay shut. It is a second `[[bin]]` in this crate added
  by an `acceptance` image stage, so the shipped runtime image is unchanged.
- **The sender reads the acceptance config through `simmer::config`**, so the
  schedule it sends against is the same file Simmer reads. A shell or Python
  sender would need the expected allowances written down a second time, and the
  day the two drift is the day the suite starts lying.
- **Days are simulated by moving `warmup.started` and restarting**, via
  `${SIMMER_WARMUP_STARTED}` and the §4 interpolation already in place. Not by
  moving the container clock: that needs privileges, invalidates certificates, and
  would make the stack lie to itself.
- **A compose profile**, so `docker compose up -d` keeps meaning what it means
  today, and the suite runs behind `--ignored` rather than in `cargo test`.

**Why plan it now rather than in phase 10.** The ramp-walk half is buildable
today — phase 3 finished the quota model, and *which container received the
message* needs no rewriting to be meaningful. Phase 4 then plugs rewrite
assertions into an existing harness instead of building both at once, and the
cutover invariant (§1.1) gets its only real test the moment there is a rewrite to
test. Discovering in phase 10 that the rewrite has been wrong since phase 4 is the
outcome this ordering avoids.

**Open, and listed in `docs/ACCEPTANCE.md` §8:** whether the ramp-walk half lands
now or waits; whether the acceptance config is its own file; and whether CI runs
the suite on every push.

### D-033 — Inbound listeners on 25/465/587, inbound TLS, and a sender ACL (planned; built in phase 11 as D-070 and D-071)

**Spec:** this contradicts `SPEC.md` in four places rather than diverging from one.
§2.2 "**No inbound TLS**"; §5.1 "Plaintext TCP, default port 25. No STARTTLS, no
implicit TLS, no ACME"; §2.3 "The listener is plaintext and accepts plaintext
AUTH"; §5.2 "`EHLO` advertises **exactly** … Nothing else". And §4.2 currently
makes it a *violation* for `allow_insecure_auth` to be false, a rule that has to
invert.

**Decision:** designed in full in `docs/INGRESS.md`. Approved by the spec's author
on 2026-08-11 and **built in phase 11** — D-070 is the listeners and TLS, D-071 the
ACL. What follows is the design as approved; where the build departed from it, the
phase 11 entries say so.

The load-bearing calls:

- **Per-listener policy, not a global switch.** `server.listen` becomes
  `server.listeners`, each with its own `tls` and `auth` mode. Ports 25, 465 and
  587 have genuinely different rules (RFC 5321, RFC 8314, RFC 6409) and one flag
  cannot express them. Defaults follow the RFCs so the common arrangement is not a
  puzzle.
- **No ACME.** That part of §5.1 stands. One PEM certificate and key read at
  startup; §2.2's no-hot-reload makes rotation a restart.
- **The ACL stays in `simmer.yaml`.** Slater keeps its equivalent in a separate
  `acl.json` because that file is reloaded on generation hot-swap and lives on
  shared storage. Simmer has neither property, so a second file would buy a second
  thing to mount and nothing else.
- **The ACL gates acceptance, never routing.** §5.3 says the authenticated
  username "plays no part in route selection", and that survives intact: a grant
  decides whether a sender identity is *permitted*, after which §5.4 selects the
  chain exactly as today. Letting the ACL choose a chain would make the outbound
  identity depend on who authenticated, which is not expressible as
  application-side configuration and so breaks the cutover invariant (§1.1)
  outright, not merely §5.3. Two users permitted the same identity must produce
  byte-identical output, and that needs to be a test.
- **`550 5.7.1` on denial**, reusing §10.3's explicit carve-out for
  `strict_senders` — a statement about the sender, which cannot suppress a
  recipient, so §14.1 holds.
- **§5.4's pattern grammar is reused verbatim** for grants — exact domain,
  `*.subdomain`, full address. `routing::sender_match::Pattern` implements it
  already and operators know it.

**Modelled on Slater** (`/home/rickk/git/hs/slater`), as instructed: users keyed by
name, argon2id PHC strings at the same parameters, `grants` as per-resource
capability lists, default deny, per-connection rather than per-account failure
limits, and a `hash-password` subcommand. Two deliberate differences — the ACL file
location above, and unrecognised grant keys, which Slater ignores (right for a
runtime-reloaded file) and Simmer rejects at startup (D-013: a silently ignored
grant is a limit that quietly does nothing).

**Placed as a new phase 11**, after §13's ten. Not before phase 4: Simmer does not
yet rewrite anything, so it does not yet do its job, and adding certificates to a
component that warms nothing is decoration. A deployment that cannot wait would
reverse that, which is a deployment question rather than an engineering one.

**Found while modelling this, and separable from it:** Slater's `equalisation_hash`
documents a defect Simmer has today — see the note under "Still open" below.

### D-042 — The acceptance harness's three open questions, settled

`ACCEPTANCE.md` §8 left three. All three were decided in phase 4, when the harness
was built.

**1. Land it now, or keep the whole suite for phase 10?** Landed now, whole. The
design's own argument won: "waiting until phase 10 to discover the rewrite has
been wrong since phase 4 would be a bad trade." It earned that immediately — the
first green run was green for the wrong reason, and only the assertions this suite
adds could have shown it (see the phase 4 summary, "What the harness caught").
What remains deferred is what §7 already deferred: real-certificate TLS and
failure injection, both phase 10, and §6.4's body-rewrite row, which is phase 5.

**2. Its own config file, or a mirror of `simmer.yaml`?** Its own —
`simmer.acceptance.yaml`. It has to differ in three ways (the `${SIMMER_WARMUP_STARTED}`
interpolation, trap hostnames, a three-day schedule) and none of them belong in the
file operators copy. The drift the design worried about is contained by
`tests/acceptance.rs::the_acceptance_config_and_the_shipped_config_stay_in_step`,
which runs in the ordinary `cargo test` — *not* behind `--ignored` — so a change
to one file that is not mirrored in the other fails without Docker anywhere near
it.

**3. CI.** Manual dispatch, not every push. It needs Docker-in-Docker, takes about
two minutes of container restarts, and its value is as a gate before a release
rather than as a per-commit signal. The existing `cargo test` gate keeps every
other tier on every push, and that includes the §6.6 property test, which is where
rewriting bugs actually surface.

**Two things the build added that the design did not anticipate:**

- **Every compose invocation must carry `SIMMER_WARMUP_STARTED`.** Compose
  re-renders the whole file on each command and reconciles running containers
  against it, so `docker compose run loadgen` — which resolves `depends_on: app` —
  silently recreated `app` at the compose *default* warm-up instant, mid-test. The
  suite then measured the wrong day while appearing to work. The harness now holds
  the current value and applies it to every invocation, and runs the loadgen with
  `--no-deps`.
- **Quota state needs resetting between tests, exactly as the traps do.** §7.4's
  accounting lives in Postgres and outlives a container restart by design, so a
  test re-using a simulated day another test has already spent finds the allowance
  gone and watches everything fall through to overflow — which looks precisely
  like a routing bug. `reset_quota()` is the analogue of `reset_traps()`.

### D-066 — The unknown-user decoy hash borrows the ACL's costliest parameters

**Spec:** §5.3 asks only that comparison be constant-time. The username-timing
oracle is not something the spec raises at all; phase 2 raised it and mitigated it.

**Decision:** `Verifier::new` picks the costliest hash the configured ACL holds —
by `m_cost × t_cost`, which is the number of memory blocks argon2 computes — and
builds the decoy from its algorithm, version, parameters and salt, replacing only
the digest. An ACL with nothing parseable falls back to a constant at §4.1's
example parameters.

**Why:** phase 2 minted the decoy at a *fixed* `m=19456,t=2,p=1` and its own
comment conceded that this cost "roughly" what a real verification costs. It does
not, in general: argon2 verification is parameter-agnostic — `PasswordHash::new`
reads `m`, `t` and `p` from the **stored** string and re-derives at those — so an
operator who minted their hashes at anything else had an unknown username and a
known one costing measurably different amounts, and username enumeration by
timing was back with the mitigation still apparently in place.

Taking the maximum rather than, say, the mean is what makes it degrade in the safe
direction: with mixed parameters an unknown user costs *at least* what any known
one costs. The other way round is the oracle.

Only the digest is replaced, and that is deliberate on two counts: keeping the
salt keeps the work byte-for-byte identical without reconstructing anything that
could be subtly the wrong length, and replacing the digest means no real
credential hash is held in a second place in memory — and makes "no password
matches the decoy" true by construction rather than by luck.

**Provenance.** Found in phase 8 while modelling D-033's ACL on Slater, which
fixes the same bug (their HIK-222) by borrowing the costliest hash the ACL holds.
Live in phase 2 code, independent of D-033, and fixed here on its own rather than
inside a feature. `SPEC.md` neither asks for this nor forbids it.

### D-067 — `max_connections` is a semaphore, and an exhausted pool is `451 4.4.5`

**Spec:** §8.3, and specifically its last sentence — "the pool bounds concurrency
against each downstream". §10.1 does not have a row for this case.

**Decision:** the per-route pool holds `max_connections` permits, taken for the
whole time a connection is checked out. A session that cannot get one within the
route's **connect** budget is answered `451 4.4.5 downstream connection pool
exhausted`, as its own error class rather than as a connect failure.

**Why a semaphore.** A cache of idle sockets satisfies §8.3's first three clauses
and none of the fourth: under a burst of two hundred concurrent sessions a cache
opens two hundred connections and merely fails to reuse them. Bounding is the
clause that protects the downstream, and it is the reason `max_connections` is
worth configuring at all.

**Why the connect budget bounds the wait.** §8.4 defines no budget for "waiting
for a peer of yours to finish", and the connect budget is the closest thing with
the right meaning — what this route's operator declared they will spend to get a
connection. Unbounded waiting is the alternative, and it holds a client connection
past the client's own timeout, which converts Simmer's saturation into the
client's §10.2-shaped ambiguity.

**Why its own class.** `451` either way, so §14.1 is satisfied by any of them; but
"raise `max_connections`" and "go and look at the provider" are opposite actions,
and a metric that conflates them sends somebody to the wrong place at 3am. Hence
`simmer_downstream_errors_total{class="pool_exhausted"}` and a distinct enhanced
status code.

### D-068 — A dead pooled connection is retried exactly once, and never past the dot

**Spec:** §8.3 says a connection is "validated with `NOOP` before reuse if idle
beyond a short threshold". §10.2 governs what may happen after the terminating dot.
Neither says what to do when a reused connection fails mid-conversation.

**Decision:** `client::relay` retries a message once, on a freshly opened
connection under the same pool permit, when **all three** hold: the connection was
reused, the failure was a `Protocol` error, and the stage was not `FinalDot`.
Counted as `simmer_pool_retries_total{route}`.

**Why it is needed at all.** The `NOOP` catches only the connections that were
already dead when checked out *and* had been idle longer than the threshold. A
connection can die inside the threshold, or between the `NOOP` and the `MAIL
FROM`. What that produces is an EOF on the first command — a `Protocol` error
indistinguishable from a real one — and without the retry it becomes a `451` for a
recipient that is perfectly deliverable. §14.1's concern arriving by a side door:
the reply is temporary, so nothing is suppressed, but Simmer would be manufacturing
failures out of its own optimisation.

**Why each condition is load-bearing.**

- **Reused only.** A failure on a socket opened a millisecond ago is the
  downstream talking, not a stale pool entry, and retrying it just asks twice.
- **Protocol only, not timeout.** A downstream slow enough to blow a stage budget
  is slow; retrying spends the budget a second time and doubles the client's wait
  for the same answer. A rejection is likewise not retried — it is the
  downstream's considered answer, and it arrives over a connection that is still
  perfectly well.
- **Never at the final dot.** That is §10.2's window: past the terminating dot the
  message may already have been accepted, and a retry there is exactly how one
  message becomes two. `tests/pool.rs` asserts the body is offered once and the
  client gets §10.2's "delivery unknown" `451`.

**What goes back in the pool**, by the same logic: a conversation that ended in
success, in a rejection, or in D-018's capability mismatch — everything else left
the connection in a state nobody can describe, and §8.3 says discard it. The
`RSET` happens on the way *back*, so what sits idle is never mid-transaction; a
rejection at `RCPT TO` leaves one, and the next message must not inherit it.

### What phase 10 also decided, without needing an entry

- **`simmer_pool_connections` is read at scrape time, not written by the relay.**
  D-056 again, with one addition: a gauge only ever written by a message that
  relayed publishes *no series at all* for a route that has never sent — the one
  whose pool an operator is most likely to be asking about. An absent series reads
  on a dashboard as a route that does not exist. It also keeps a metric update off
  the latency path of every message.
- **§9.2's pool statistics are zeros, never `null`.** The field was `null` while
  there was no pool. A configured route now always has one, so "nothing has been
  opened" is a fact worth stating, and it is reported against the configured
  `max_connections`. `null` survives only for a route the process has no pool for,
  which cannot happen for a configured one.
- **The `NOOP` validation threshold is a constant** (five seconds), on D-063's
  precedent: §4.1 defines no key for it. Chosen against the asymmetry — too low
  wastes a round trip on a healthy connection, too high spends a whole
  conversation discovering a dead one, and the second is paid on the latency path
  of a client holding a connection open.
- **Idle connections are taken last-in-first-out.** The most recently returned is
  the least likely to have been closed at the far end, and it keeps the rest of the
  set ageing towards `idle_ttl` rather than cycling all of them just below it.
- **A connection retired for `idle_ttl` is dropped without a `QUIT`;** one retired
  for `max_messages_per_connection` gets one. The first is probably already closed
  at the far end and the check would be paid on a waiting message's latency path;
  the second is known-healthy and is being closed by our choice, so it says
  goodbye rather than leaving the provider to count a reset against us.
- **The pool map fills lazily on a miss** despite being seeded from the same
  `Config` the engine holds. The miss is unreachable in the service; making it
  *impossible* costs one `RwLock` and removes a branch whose only other handling
  would have been to lie to a client.
- **`Stage::Keepalive`** names the pool's own traffic. It never reaches §10.1's
  table — a failed `NOOP` or `RSET` makes the pool discard the connection, which
  is the point of issuing them — but it is in `ALL_STAGES` so that if it ever does,
  §14.1's exhaustiveness assertion covers it rather than somebody's suppression
  list discovering it first.

---

## The spec settlements (after phase 10)

Five questions carried for the spec's author, answered on 2026-08-11. Four were
already-recorded findings and needed only a ruling; the fifth authorised new work.
**`SPEC.md` was amended** — this is the first time, and it changes the standing
rule in `CLAUDE.md`: the spec is authoritative and still is, but where a question
has been put to its author and answered, the answer goes into the spec with a
marker saying what it used to say, rather than accumulating as a divergence
nobody reading the spec would see.

What was amended, and what each replaced:

| § | Was | Now |
|---|---|---|
| **2.2** | "One Simmer instance owns its quota state" | "v1 runs a single instance; the storage layer is safe for more", with the row-lock reason (D-061) |
| **2.3** | Deployment assumption only | Plus the two real multi-instance constraints, config skew and the §7.3 bound |
| **4.1** | `envelope_from: "bounce+{{original.envelope_from.local}}@newbrand.com"` | `bounce@newbrand.com`, with a note on why (D-036) |
| **4.2** | — | A new rule: `envelope_from`'s domain must be a literal (D-069) |
| **5.6** | `single_recipient_only`, defaulting true, plus a result-collapse table | One recipient per transaction, unconditionally (D-047) |
| **6.3** | `recipient.*` in a template is a startup warning | Permitted freely; there is only ever one recipient |
| **9.1** | `simmer_partial_delivery_total` | Marked unreachable, retained for continuity |
| **13** | Phase 9: multi-recipient splitting | Struck through as void |

Not amended yet, deliberately: §2.2's "No inbound TLS", §5.1, §2.3's plaintext
sentence and §5.2's "advertises exactly". Those four are what D-033 reverses, and
amending them before the capability exists would make the spec describe something
that is not there — the same failure in the opposite direction. They are amended
by the phase that builds them.

### D-069 — A route's `envelope_from` domain must be a literal

**Spec:** §4.2 had no such rule. §6.6's stability property is the closest thing and
does not cover it.

**Decision:** `config::validate` refuses to start if the part of any route's
`identity.envelope_from` after the final `@` contains a template variable. Applies
to **every** route, overflow included.

**Why stability was not already enough.** `bounce@{{original.envelope_from.domain}}`
passes §6.6: applied twice it gives the same answer, which is exactly what §6.6
tests. It is nonetheless incoherent. The ramp, the daily allowance, reputation
accrual and the `quota_usage` row are all keyed on a route having **one** outbound
domain — that is what a warming route *is*. A route whose domain varies per message
warms nothing, and its quota row counts a mixture of domains under a single label,
so the ledger reads as a healthy ramp while no domain is actually being warmed.
That is the worst available failure mode for this component: silent, and visible
only as reputation that never improves.

**Why it applies to overflow routes too**, which have no ramp: an overflow route's
identity is the *established* domain, and "established" is equally a claim about
one domain. A varying one there means §1.1's cutover cannot be expressed as
application config either, which is the invariant everything else is subordinate
to.

**Why the local part is untouched.** The rule is deliberately narrow. A templated
local part is a different question and §6.6 already owns it — it rejects the
relative ones, which is what D-036 was about, and permits the rest. Widening this
rule to the whole address would take a decision that belongs to §6.6 and would ban
legitimate constructions.

**What it makes unreachable.** D-064's warning — "preflight cannot check this route
and `strict` therefore has no effect" — can no longer be reached through a valid
configuration, and nor can `preflight::plan`'s skip. Both are kept. If the rule is
ever relaxed, `warnings()` is what would have to notice, and rediscovering that
reasoning from a deleted function is not a job to leave for somebody. The two tests
that asserted the warning became tests that assert the refusal; `preflight`'s own
tests still reach the skip by mutating a route after loading, which is the only way
in now.

---

## The multi-instance correction (after phase 7)

### D-061 — Quota **is** safe across instances; D-007's stated reason was wrong

> **Settled 2026-08-11 by the spec's author, on all three questions.** §2.2's
> ownership sentence is reworded to "v1 runs a single instance; the storage layer
> is safe for more", with the row-lock reason stated; the §7.3 bound is accepted as
> a documented property rather than a defect; and both real constraints — config
> skew and that bound — are now written into §2.3's deployment assumptions where a
> deployer will actually meet them. D-007's *decision* still stands: simmer runs
> one instance and stays off the spot fleet.

**What the repository said.** D-007 and `README.md`'s Deployment section both
justified keeping simmer off the spot fleet by saying that the fleet's roll method
"would run two Simmers against one database for the minutes a replacement takes to
build. That window is precisely when quota overshoot would occur."

**That is false, and it had been false since phase 3.** §7.4's reservation does its
headroom check and its write inside *one transaction holding a row lock*.
`models::quota::lock_usage` uses `INSERT … ON CONFLICT DO UPDATE` rather than `DO
NOTHING` specifically because `DO UPDATE` takes the row lock even when the row
already exists — the reason is in that function's own doc comment. Postgres
serialises contenders for one `(route, domain_group, day_index)` row whether they
are two tasks in one process or two processes on different hosts. There is no
per-instance state in the path at all: the counters, the reservations and the
route states are all rows.

Two other things were already written for concurrency and are also fine.
`sweep_expired` is a single CTE in which only one `DELETE` can win a row and the
decrement is derived from the rows that `DELETE` actually removed, so two sweepers
cannot double-release. `release_by_ids` is scoped to its own reservation ids
rather than truncating, so one instance's shutdown does not free another's
in-flight headroom. Both were kept under D-007's own "costs nothing now, expensive
to retrofit" clause. They turn out to have been the difference between a claim and
a fact.

**What genuinely constrains two instances** is narrower, and neither part is
fixed by a lock:

1. **D-049's frequency race.** The §7.3 recipient-event count is read *outside* the
   reservation transaction, so sends that all read before any of them writes all
   see room. **Settled 2026-08-10: bound it and document it, do not move it inside
   the transaction.** §7.3 is a reputation-shaping heuristic, not an accounting
   invariant like quota, and holding the ramp's hot row lock across a
   high-cardinality index read on every message costs more than the messages it
   would save. This is not a multi-instance bug — it is a concurrency bug that a
   second instance widens the window on.
2. **Config skew during a roll.** `quota_usage.allowance` is authoritative once
   written (D-026), so two instances running different schedules for the minutes a
   replacement takes will have whichever writes the day's first row set that day's
   ceiling, and the other will silently honour it. Inherent in D-026; belongs in
   the deployment constraints rather than in the code.

**The bound on (1), stated exactly.** Not "one extra message" — that is the
two-instance case mistaken for the general one. With `C` sends whose §7.3 reads all
land before the first of them commits, the window reaches `threshold + (C - 1)`.
The overshoot is one per concurrent send, bounded by peak concurrency against a
single recipient key and by nothing else.
`tests/quota_multi_instance.rs` asserts that figure at `C = 2` and again at
`C = 6`, so the document and the code cannot drift apart quietly.

**Decision.**

- Correct D-007's reasoning (below) and `README.md`'s Deployment section. The
  *decision* — simmer is not deployed on the spot fleet — is unchanged; only its
  justification was wrong, and it now rests on config skew and §7.3 rather than on
  a quota overshoot that cannot happen.
- Evidence the row-lock guarantee with `tests/quota_multi_instance.rs`, which
  builds **two independent pools** against one database rather than racing tasks
  on one pool. `tests/quota.rs`'s existing concurrency test cannot distinguish the
  row lock from a pool that happens to serialise its own contenders; this one can.
- **Do not add a lock table or a coarser lock.** A table-level lock would serialise
  every route and domain group against each other, which the row lock deliberately
  does not, and would add nothing to correctness that the row lock does not already
  provide.
- `docs/MULTI_INSTANCE.md` for the spec's author. §2.2 says "No multi-instance
  clustering. One Simmer instance owns its quota state" in as many words, so this
  is a spec divergence and not merely a README fix.

**A note on how the evidence was checked, because a green race test proves
nothing on its own.** All five tests passed on their first run, so
`lock_usage` was temporarily replaced with an unlocked read and they were re-run.
Two failed, as they must: the N-way test granted 16 reservations against 15 slots,
and the contender in the mechanism test was `Taken` where it must be `NoHeadroom`.

That exercise also corrected the mechanism test itself, twice.

- Its first version raced for a row that **did not yet exist**, where two `INSERT`s
  collide on the unique index and Postgres serialises them whatever the conflict
  clause says. It passed against a deliberately broken `lock_usage`. It now
  pre-creates the row, which is the only case in which `DO UPDATE` versus `DO
  NOTHING` is the difference — and the case the doc comment is actually about.
- Its "the contender is still blocked" assertion turns out to be **necessary but
  not sufficient**, and the test now says so. Against an unlocked read the
  contender still blocks — later, on the `UPDATE` inside `insert_reservation`, and
  after it has already decided it has headroom from a stale read. The assertion
  with the teeth is the one about *what it decided*, not the one about whether it
  waited.

### D-062 — The cargo-deny licence gate stays, at Apache-2.0 compatibility

**Open since phase 1**, as "decide whether to keep the cargo-deny licence gate".
`LICENSES.md` concluded copyleft was a low risk for an internal container that is
never distributed, which made the gate arguably ceremony.

**Decided 2026-08-11 by the repository's owner: keep it, and the bar is that
nothing incompatible with Apache-2.0 enters the graph.**

Nothing needed building — the gate was already in this shape — so this entry
records the decision and the verification rather than a change:

- `deny.toml`'s `[licenses] allow` is an **allow-list**, so copyleft fails by
  omission rather than by anyone remembering to add a rule. Every entry is
  permissive and Apache-2.0-compatible.
- CI runs `cargo deny check` on every push to every branch, and it is the same
  bare command the README tells a developer to run.
- `[licenses.private] ignore = true` covers exactly one crate — `simmer` itself,
  which sets `publish = false`. A third-party crate with no `license` field still
  fails, and since D-060 removed the `ignore-sources` exemption there is no
  crate in the graph relying on one.

**The one finding worth writing down.** `cargo deny list` reports
`LGPL-2.1-or-later (2): r-efi@5.3.0, r-efi@6.0.0`, which reads as a violation and
is not: `r-efi` is `MIT OR Apache-2.0 OR LGPL-2.1-or-later`, an `OR`, so the LGPL
branch is never taken. `cargo deny list` prints every licence *named* in an
expression including branches not taken; `cargo deny check licenses` resolves it
and passes. Verified against crates.io for both pinned versions. It is recorded in
`LICENSES.md` because the next person to run `list` will otherwise re-derive it in
a hurry.

Also fixed while here: `.github/workflows/build.yml`'s `--locked` comment still
justified itself by hs-utils being a git-tag dependency, which D-060 removed.

---

## Defects found

**Fixed in phase 10 — see D-066.** ~~Timing-based username enumeration in
`smtp/auth.rs`.~~ The decoy now takes its parameters and salt from the costliest
hash the configured ACL holds, so the unknown-user path runs the derivation a real
login runs. Four tests in `src/smtp/auth.rs` pin it: the parameters are copied, the
costliest of several wins, the decoy is not a copy of anybody's hash, and an ACL
with nothing usable falls back rather than losing the ballast entirely. The
original report follows, unchanged.

**Timing-based username enumeration in `smtp/auth.rs`.** `Verifier` hashes an
unknown username against a *fixed* decoy minted at `m=19456,t=2,p=1`, so that a
miss costs what a hit costs. But argon2 verification is parameter-agnostic —
`PasswordHash::new` reads `m`/`t`/`p` from the *stored* PHC string and re-derives at
those. An operator who mints with anything other than the §4.1 example's parameters
therefore makes the two paths diverge in cost, and username enumeration by timing
returns with the mitigation still apparently in place. The code's own comment
concedes it: the decoy costs "*roughly*" what a real verification costs.

Slater solves this (their HIK-222) by borrowing the costliest hash the ACL actually
holds, so the unknown-user path runs the very derivation a real login runs — exact
when parameters are uniform, and degrading in the safe direction when they are not.

Live in phase 2 code and independent of D-033. Worth fixing on its own rather than
inside a feature.

---

## Phase 11 — listeners, inbound TLS, and the sender ACL

D-033, built. Beyond §13's ten phases; the spec's author approved it on 2026-08-11
and `SPEC.md` §2.1, §2.2, §2.3, §4.1, §4.2, §5.1, §5.2, §5.3 and §13 are amended by
this phase, each with a marker saying what it used to say — the passages the spec
settlements deliberately left alone until the capability existed.

`docs/INGRESS.md`'s five open questions, all closed:

| # | Question | Answer |
|---|---|---|
| 1 | Amend `SPEC.md`, or let D-033 override it? | Amended, by this phase (the spec settlements' rule) |
| 2 | Before or after phase 4? | Moot — after phase 10 |
| 3 | What replaces `server.listen`? | `server.listeners`, required; `listen` is named by `removed_keys` (D-070) |
| 4 | Should the ACL grant anything besides `send_as`? | No — a capability nobody uses is a capability nobody tests (D-071) |
| 5 | Does `auth: optional` on port 25 earn its place? | Yes, as the RFC default, with a startup warning once users exist (D-071) |

### D-070 — Listeners, inbound TLS, and the `allow_insecure_auth` inversion

**Spec:** §5.1 said "Plaintext TCP, default port 25. No STARTTLS, no implicit TLS,
no ACME"; §5.2 listed exactly five extensions; §4.2 required `allow_insecure_auth`
to be true. All amended.

**Decision:** `server.listeners` replaces `server.listen`, one entry per port, each
with `tls: off | starttls | starttls_required | implicit` and `auth: disabled |
optional | required`. Absent values take the port's RFC default — 465 implicit and
required, 587 `starttls_required` and required, everything else `off` and
`optional` — so `listeners: [{ address: "0.0.0.0:25" }]` means exactly what
`listen: "0.0.0.0:25"` meant. `server.auth.required` is gone too: whether AUTH is
required is a property of a port, and two switches for one thing would disagree.
Both removed keys fail with a message naming the replacement.

`allow_insecure_auth` now means what its name says and defaults false. The §4.2
rule it inverts becomes: `auth: required` on a `tls: off` listener with plaintext
AUTH refused is a violation, because that listener would refuse every message.

The load-bearing details, several of which the design left open or got slightly
wrong:

- **Certificate loading goes through one function** (`smtp::tls::load`), called by
  §4.2 validation and again by the listener, so the check and the listener cannot
  disagree — §6.6's reasoning, again. A missing, unreadable, unparseable or
  mismatched file is a violation reported with everything else; a permission
  failure names the process uid, the file's owner and mode, and UID 1000.
- **`rustls-pki-types` parses PEM, not `rustls-pemfile`.** The design named
  `rustls-pemfile`; it has been archived since August 2025 and is
  RUSTSEC-2025-0134 (unmaintained), which `cargo deny check` would fail on. The
  replacement is the code `rustls-pemfile` had become a wrapper for, already in
  the graph. **No new runtime crate**: `rustls-webpki` becomes a direct
  dependency, but it was already compiled as rustls's verifier.
- **Name coverage and expiry are warnings, not violations.** Refusing to start
  over either would take the plaintext listeners down with the TLS ones. Coverage
  uses `rustls-webpki`'s own name matching — what a verifying client runs. The
  expiry date is read by a forty-line DER walk (`tls::not_after`) rather than
  `x509-parser`, whose tree is not worth one field; it is fuzzed by truncation in
  its tests and returns `None` rather than guessing. Warned at fourteen days.
- **The injection check is the outbound one, mirrored.** Bytes already buffered
  behind `STARTTLS` drop the connection before `220` is sent. It is the one place
  RFC 2920's never-discard-buffered-input rule is overridden, and the session's
  module comment says so.
- **The reset keeps `auth_failures`.** RFC 3207 §4.2 has the server forget the
  greeting, authentication and transaction; §5.3's three strikes are per
  connection and survive, or a handshake would buy unlimited guesses.
- **`530` and `538` are split by cause, not as the design put it.** INGRESS.md §3
  gave `538 5.7.11` to "AUTH before TLS where the listener requires it". On a
  `starttls_required` listener RFC 3207 §4 already answers *every* command but a
  few with `530 5.7.0 must issue a STARTTLS command first`, AUTH included, and
  that is what is built. `538` is for AUTH over plaintext where plaintext
  credentials are refused — a `starttls` or `off` listener with
  `allow_insecure_auth: false` — which is exactly RFC 4954 §6's meaning. RSET
  joins RFC 3207's permitted list, since no transaction can exist to reset.
- **AUTH is not advertised where it could not succeed.** Before the handshake on a
  `starttls_required` port, or over plaintext with plaintext AUTH refused.
  Advertising it there invites a password in clear only to refuse it.
- **A failed handshake gets no reply.** The plan said `454`; that code is for a
  server that cannot *start* TLS, which a certificate loaded at startup rules
  out. Once `220` has gone and the handshake fails, the TLS layer owns the socket
  and there is no channel left to reply on. The close is the answer.
- **An implicit-TLS listener refuses with a TCP close**, never a plaintext `554`
  or `421`, which would arrive in place of a ServerHello. §5.1 already permitted
  the bare close.
- **Sessions end with `close_notify`.** Found by the tests: a session over TLS was
  being dropped without one, which rustls reports to the client as an unexpected
  EOF — indistinguishable from truncation — so a `221` after `QUIT` arrived
  followed by an error. `Session::close` now shuts the stream down, bounded at
  two seconds so a peer that has stopped reading cannot hold a permit.
- **One session bound and one CIDR check for every listener.** The limits are
  about the process, and a test holds a session on one port and gets `421` on
  the other.
- **`Received:` gains RFC 3848's `S`** — `ESMTPS`, `ESMTPSA`. D-002 already
  excludes the header from §12.3's comparison, so the cutover invariant is
  unaffected.
- **The inbound stream is `downstream::stream::Stream`**, generalised to hold
  tokio-rustls's client-or-server `TlsStream`, so there is one `Taken` state
  rather than two — as the design asked.

**Metric:** `simmer_inbound_tls_failures_total{mode,reason}`, `reason` being
`handshake` or `plaintext_after_starttls`.

### D-071 — The sender ACL: `grants.send_as`, default deny, gating acceptance only

**Spec:** §5.3 said the username "plays no part in route selection" and nothing
about what an authenticated user may send as. Amended; the sentence stands.

**Decision:** each user carries `grants: { send_as: [...] }` in §5.4's pattern
grammar, via `routing::sender_match::Pattern`. For an authenticated session the
envelope sender is checked at `MAIL FROM` and the first `From:` address at the final
dot; either outside the grant is `550 5.7.1 sender not permitted`.

- **`grants` is required, and an empty `send_as` is a violation.** Default deny
  with no grants is a user who can log in and send nothing, which is never meant.
- **Patterns that can match nothing are refused** — whitespace, a bare `*`, a `*`
  anywhere but a leading `*.`, `@x.com`, `sales@`. `Pattern::parse` is total,
  which suits routing, where a rule that never matches shows up in
  `simmer_unmatched_sender_total`; a grant that never matches shows up only as
  refusals.
- **Unknown capability keys are refused**, by `deny_unknown_fields`, where Slater
  ignores them — D-013's reasoning, as the design said.
- **The null sender passes `MAIL FROM`** and is judged by its `From:`. A bounce has
  no envelope identity to grant.
- **A message with no parseable `From:` is refused** for an authenticated user.
  Default deny: an identity that cannot be found is not inside the grant.
- **Unauthenticated sessions are not subject to the ACL.** On an `auth: optional`
  listener a client that never authenticates may present any identity
  `allowed_cidrs` admits — the pre-ACL trust model. Rather than change port 25's
  RFC default, §4.2 warns once per `optional` listener whenever users exist, and
  §2.3 now says it outright.
- **It never routes.** The ACL answers "may this user present this identity?" and
  can only refuse. A test sends the same message as two users granted the same
  identity and asserts the downstream received byte-identical bodies, and that
  `Received:` names neither user.
- **`550`, under §10.3's carve-out** for `strict_senders`: a statement about the
  sender, which cannot put a recipient on a suppression list, and loud because it
  means a misconfigured application or somebody else's credentials.

**Metric:** `simmer_sender_not_permitted_total{stage}`, `stage` being `mail_from`
or `from_header`. Deliberately not labelled by user or address: the log line names
both, and a label per address is §7.3's high-cardinality series.

**Also:** `server hash-password` reads a password from stdin — never argv, which
lands in shell history and `ps` — and prints an argon2id PHC string at argon2's
defaults, `m=19456,t=2,p=1`. Uniform parameters are the case D-066's decoy is exact
for. It runs before the config is read, since the config is what needs the hash.

### D-072 — Pre-authentication limits are not built

**Spec:** silent. `docs/INGRESS.md` §6 suggested a tighter session cap and a byte
ceiling that apply until `AUTH` succeeds.

**Decision:** deferred. Each would be a new §4.1 key, and nothing forces them yet:
Simmer still sits behind `allowed_cidrs` on a trusted segment (§2.3), and the pieces
that exist — `max_concurrent_sessions`, D-020's line caps, `timeouts.command`,
which also bounds an implicit-TLS handshake — already stop a peer that never
authenticates from holding a slot indefinitely. It becomes worth building the day
Simmer listens somewhere `allowed_cidrs` cannot be tight.

## The test programme (after phase 11)

The plan is in `docs/TESTING.md` once it lands: six tiers — fast, end-to-end and a
multi-server matrix, stress at 10x the ceilings, a 1 h / 24 h soak, fault injection,
and deep generative testing — run nightly and on manual dispatch. Its step 0 was a
defect found while planning it.

### D-073 — CI builds the shipped image with `--target runtime`, and the acceptance suite gets its manual job

**Found:** `.github/workflows/build.yml` ran `docker build` with no `--target`. The
Dockerfile's last stage has been `acceptance` since phase 4 (D-042), whose entrypoint
is `/app/loadgen`, and an unpinned build produces the last stage. **Every image CI
pushed to GHCR since phase 4 — the `sha`, branch and `latest` tags — was the
acceptance load generator, not the server.** `CLAUDE.md` and `docker-compose.yml`
both warned that "a deployment pipeline would have to" pin the target; the pipeline
in this repository did not. Nothing is deployed yet (STATE.md), so nothing ran the
wrong image; the first deployment would have.

**Decision:** `--target runtime` in `build.yml`, with a comment saying why.
Verified by the entrypoint of the image CI pushes after this change
(`docker image inspect … --format '{{.Config.Entrypoint}}'` → `[/app/server]`).

**Also:** D-042 item 3 decided the acceptance suite would run on manual dispatch and
never built the job. It is `.github/workflows/acceptance.yml`: `workflow_dispatch`
only, a separate workflow so that dispatching it never builds or pushes an image,
running exactly the two commands `docs/ACCEPTANCE.md` gives. `README.md` said "CI
runs all of these" of a list that included the compose steps, which was never true;
it now says which job runs what.

**Why reordering the Dockerfile was not the fix:** putting `runtime` last would make
the unpinned build correct, but it cannot be last — `acceptance` is `FROM runtime`, so
it must come after it. Pinning the target is the only fix that keeps the stage graph,
and it is the one every other builder already uses.

### D-074 — 8BITMIME towards a downstream that does not advertise it (finding F13)

**Found:** by the test programme's Postal spike (`test/postal/`). Postal — the
production downstream — advertises neither `8BITMIME` nor `SIZE`, and answers `250`
to `BODY=8BITMIME` regardless. The outbound client refused any message whose client
had declared `BODY=8BITMIME` to a downstream not advertising the extension
(`MissingCapability` → `451 4.3.5`). Many clients declare it for every message, so
against Postal a large share of real traffic would have been deferred on every
retry, indefinitely — never delivered, never refused.

**Spec:** §8 is silent; RFC 6152 §3 says a relay facing a server without 8BITMIME
must convert the message to 7-bit or refuse it.

**Decision** (the option the user chose from four):

- **A body with no byte above 0x7F is 7-bit, whatever the client declared**, and is
  relayed without the `BODY=8BITMIME` parameter. Fully RFC-correct; no decision
  needed. `BODY=8BITMIME` is now sent only to a downstream that advertised the
  extension.
- **A genuinely 8-bit body** goes to a downstream without the extension only where
  the route declares it 8-bit clean: a new per-route `downstream.assume_8bitmime`,
  default `false` — the same shape as D-018's `downstream.smtputf8`, for the same
  reason: `EHLO` does not tell the truth, so the operator must. Sent without the
  parameter, bytes unchanged. An undeclared route keeps `451 4.3.5` and
  `simmer_downstream_config_error_total{stage="capability"}`.
- **The shipped config declares it on the Postal route.** `/routes` reports it next
  to `smtputf8`.

**Not chosen:** converting to quoted-printable (RFC-correct always, but a sizeable
feature that changes body bytes and overrides D-045's never-change-the-encoding
rule); sending 8-bit bodies to any downstream (silently wrong for a server that
genuinely is not 8-bit clean); fixing only the 7-bit case.

Three tests in `tests/smtp_ingress.rs` pin the three cases against a fake downstream
with Postal's `EHLO`.

### D-075 — Process, session, reservation, pool and task gauges on `/metrics`

**Spec:** not in §9.1's list. Added because the test programme's stress and soak
tiers need these to be assertable — and because production needs them for the same
reasons: a session bound that is only visible through refusals, a stranded
reservation that is visible only as a sweeper count seven minutes later, and a
Postgres pool that is visible only as `451 4.3.0`.

**Decision:** eleven gauges, each read when `/metrics` is scraped, like D-056's
quota gauges and phase 10's pool gauges:

- `process_resident_memory_bytes`, `process_open_fds`, `process_max_fds`,
  `process_threads`, `process_start_time_seconds` — the standard Prometheus client
  names, read from `/proc/self` with no new dependency. A field procfs cannot supply
  is left unwritten rather than written as zero. The start time is taken when the
  recorder is installed, the first thing `main` does after reading its config.
- `simmer_sessions_active` / `simmer_sessions_max` — the shared §5.1 semaphore.
- `simmer_reservations_in_flight` — the §10.4 registry. Nonzero on an idle instance
  means stranded reservations (finding F2's symptom).
- `simmer_db_pool_connections{state="in_use"|"idle"}` / `simmer_db_pool_max`.
- `simmer_tasks_alive` — tokio's live-task count, stable since 1.39.

`AdminState` gains the session semaphore and the Postgres pool, both optional so a
test that runs no listener still builds one.

### D-076 — The metrics exporter's upkeep runs every 5 s (finding F8)

**Found:** by the test programme's resource map. `metrics::install` uses
`install_recorder()`, which — unlike the exporter's own `install()` — starts no
upkeep task, and histogram samples (`simmer_downstream_latency_seconds`) sit in the
exporter's buffer until `run_upkeep` drains them. Only the `/metrics` handler called
it. An instance nobody scrapes therefore grew with every message relayed.

**Decision:** `main` spawns the task `install()` would have: `run_upkeep` every
5 s (the exporter's default), stopped with the other background tasks at shutdown.
The scrape handler keeps its own call, which is harmless. Verified by the soak's A/B
comparison of a scraped and a never-scraped instance, which is the only place it is
observable; there is no cheap deterministic test for it.

### D-077 — The outbound `EHLO` names Simmer, not the downstream (finding F14)

**Found:** by the T2 server matrix. Every Postfix `Received:` header gave the
downstream's own host as the client's name: `downstream/client.rs` sent
`EHLO {downstream.host}` on connect and again after `STARTTLS`. §4.1 gives
`server.hostname` as Simmer's identity — "EHLO banner, Received headers,
certificate name" — and RFC 5321 §4.1.1.1 wants the client's own name in that slot.
A receiving MTA that refuses a client claiming to be itself, a common anti-forgery
rule, would refuse at `EHLO`, which D-023 turns into a `451` for every message on
the route, indefinitely. Postal, the production downstream, would have been
greeted with its own name.

**Decision:** both `EHLO`s send `server.hostname`, threaded from `relay` through
the pool's checkout and reopen into `Connection::open`. `deliver` loses the
hostname parameter it had never used, since the name is spent before the envelope.
Nothing is configurable: there is one identity to present, and §4.1 already names
it. Tested in-process (`tests/pool.rs`: every `EHLO` the fake downstream receives)
and live (`tests/e2e_matrix.rs`: Postfix's `Received:` on a plaintext route and on
a `STARTTLS` route, where the name that counts is the second `EHLO`).

### D-078 — The server's global allocator is jemalloc (finding F15)

**Found:** by the stress tier's first smoke scenario (T3 S0): 32 clients, one
connection and one `AUTH` per message, small bodies, no faults. `app` was
OOM-killed at its 1 GiB limit within seconds. Measured rather than guessed, and
memory tracked the *number* of logins, not how many overlapped: one login per
100-message session peaked at 196 MiB and fell back to 5 MiB; one per message at
concurrency 1 sat at 156 MiB; at concurrency 8 it plateaued at ~1015 MiB; at 32
it was killed.

glibc's malloc is the cause. Once the first 19 MiB argon2 block (§4.1's
`m=19456`) is freed, the dynamic mmap threshold rises past that size, so later
blocks come from per-thread arenas — up to eight per CPU the *host* has, not the
cgroup's quota — and stay resident when freed. `MALLOC_ARENA_MAX=2` did not help.
Pinning the threshold (`GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072`) did —
concurrency 32 peaked at 213 MiB and returned to 6 MiB — which confirms the
mechanism.

**Decision:** the server binary uses jemalloc (`tikv-jemallocator`, default
features) as its global allocator. A different allocator was the user's choice
over the environment tunable, which would cover only the shipped image, and a
`mallopt` call at startup, which needs a direct `libc` dependency and `unsafe`.
It is set in `src/main.rs`, so the library, the tests and the loadgen/sink keep
the system allocator. `MIT/Apache-2.0` in every published version of both crates;
the vendored C library is `BSD-2-Clause` (`LICENSES.md` §7).

mimalloc was adopted first, on priors — `MIT`, a C-compiler-only build — and it
fixed F15, but it had not been compared with anything. The user asked for the
comparison before either was pushed, and this entry records it.

**Measured** on the stress stack (`app` at 2 CPUs and 1 GiB, the counting sink as
downstream), each case twice on a freshly started `app`: the mean, with the range
across the two runs in brackets. B1 is 32 clients × 2,000 messages with one login
per message; B2 is 32 × 20,000 with one login per 100; B3 is 16 × 400 messages of
2 MiB with one login per 4. "After" is cgroup anon memory once the load stopped.
Every run delivered every message; none was OOM-killed.

| Case | Allocator | msg/s | p99 ms | Peak MiB | After 5 s | After 30 s |
|---|---|---|---|---|---|---|
| B1 | mimalloc | 43.5 (42–45) | 1433 | 630 | 499 | 18 |
| B1 | **jemalloc** | 32.4 (32–33) | 1865 | 296 (242–351) | 12 | 10 |
| B2 | mimalloc | 217 (172–263) | 324 | 651 | 18 | 16 |
| B2 | **jemalloc** | 166 (156–176) | 531 | 151 (122–179) | 10 | 9 |
| B3 | mimalloc | 1.6 | 11345 | 344 | 87 | 71 |
| B3 | **jemalloc** | 1.8 | 10749 | 202 | 175 | 21 |

Idle, jemalloc holds 4.2 MiB against mimalloc's 14.5. A clean image build takes
106 s against 101 s, and the binary is 7,450,288 bytes against 7,265,032. The
host was shared with other workloads: one mimalloc B2 run moved from 263 to 172
msg/s between identical runs, so throughput differences inside that spread are not
read as real.

**What the choice costs.** jemalloc is about 25% slower on B1, consistently across
both runs. By default it sends allocations above `oversize_threshold` (8 MiB) to
an arena that returns them to the OS at once, so every login faults and zeroes
argon2's 19 MiB block afresh. Raising the threshold to 32 MiB
(`_RJEM_MALLOC_CONF=oversize_threshold:33554432`, no rebuild needed) recovered
39–41 msg/s on B1 — but put its peak back at 600–695 MiB, mimalloc's figure. The
login speed and the memory are one trade, not two separate wins.

**Why the trade is accepted** (the user's judgement): one login per message is an
outlier in real use. Bulk sending looks like B2 and B3 — long sessions, many
messages per login, larger bodies — and there throughput is within the host's
noise (B2's mean is lower but its range overlaps; B3 is level), while jemalloc's
peak is a quarter (B2) to three-fifths (B3) of mimalloc's and returns to baseline
within seconds. Memory is what F15 was about, and headroom under the container's
limit is what that needs. A deployment that really is login-bound has one knob,
`oversize_threshold`, and turning it gives the memory back.

F3a — the *concurrent* peak, 64 sessions × 19 MiB — is untouched by this and
stays open: it wants a bound on concurrent verifications, proposed separately.

### D-079 — A bound on concurrent argon2 verifications (finding F3a, corrected)

**What F3a claimed.** Concurrent `AUTH` verifications are bounded only by
`max_concurrent_sessions`, so 64 sessions × §4.1's `m_cost` of 19 MiB ≈ 1.2 GiB
would OOM a 1 GiB container. The stress tier was built to catch it.

**What the measurements actually showed.** It does not reproduce, in five
attempts, all on `app` at 1 GiB with the counting sink:

| Shape | Peak |
|---|---|
| 200 logins, 64 concurrent, 2 CPUs | ~530 MiB |
| the same at 8 CPUs | ~295 MiB |
| the same at 1 CPU | ~484 MiB |
| 64 logins released together at one instant, 2 CPUs | 526 MiB (`memory.peak`) |
| the same at 8 CPUs | 608 MiB (`memory.peak`) |

More cores made it *better*, not worse: a verification holds its 19 MiB only while
it runs, and more cores finish it sooner. Nor is the session cap the ceiling —
even with all 64 clients connected, greeted and released to `AUTH` at one shared
instant (the loadgen's `--auth-delay`), only about 32 were ever in flight, because
`spawn_blocking` cannot create threads faster than the earlier hashes complete.
The figures above are the kernel's own `memory.peak`, not sampled, after sampling
at 0.4 s was found to under-read by ~40%.

**Decision:** bound it anyway, at `max(4, 2 × available_parallelism())`, held as
`auth::VerifyLimit` and shared by every session. Not for F3a's reason, which is
not real: for the reason the measurements exposed. ~32 in flight is 608 MiB, 59%
of the budget, and nothing in the design holds it there — it is the outcome of a
race between thread creation and hash duration, and it moves with the kernel, the
allocator, `m_cost`, or a raised session cap. The bound makes the ceiling a
property of the configuration (`permits × m_cost`: 4 × 19 MiB ≈ 76 MiB on two
CPUs, 32 × 19 MiB ≈ 608 MiB on sixteen) instead of a coincidence of timing.

`available_parallelism` honours the cgroup quota, so a container bounds itself to
what it may actually use; twice the cores leaves one ready to start as another
finishes, and argon2 being CPU-bound means more in flight than that buys memory
and nothing else. The cost is latency for a burst of clients, bounded by
`timeouts.command`, and `simmer_auth_verifies_in_flight` against
`simmer_auth_verifies_max` says when that is happening.

**Verified** on the same synchronised burst of 64 logins, `memory.peak` again:

| | before | after | bound |
|---|---|---|---|
| 2 CPUs | 526 MiB | **85 MiB** | 4 permits (~76 MiB) |
| 8 CPUs | 608 MiB | **316 MiB** | 16 permits (~304 MiB) |

All 64 clients were still answered `235` at both sizes; the queueing shows up only
as latency, about three seconds for 64 simultaneous logins on two CPUs and nothing
measurable on eight. The peak now tracks `permits × m_cost`, which is the point.

Throughput is unaffected, as expected: the bound is never below the core count and
argon2 is CPU-bound, so queueing an already-saturated resource costs memory
nothing and throughput nothing.

This is not a red-green fix: the bound was written after the behaviour was
measured, so the evidence above stands in for a failing test. What is pinned in
tests is the bound itself.

### D-080 — The spilled `DATA` buffer batches its writes (finding F16)

**Found:** by the soak tier (`docs/SOAK.md` §5). The 6–12 s latency tail that three
soak runs reproduced, and eight hypotheses failed to explain, was every 4 MiB
message: both instances were slow on the same message ids, and a probe at 1–5 MiB
was flat below §8.1's spill threshold and linear above it at about **2.35 s per
MiB**. Once a body passed `SPILL_THRESHOLD`, `MessageBuffer` held a bare
`tokio::fs::File`, and the `DATA` loop appends twice per line. tokio performs every
file write as its own blocking-pool job, so a body in 76-column lines — the shape of
any base64 attachment — cost about 27,000 round trips per MiB. At the shipped
25 MiB `max_message_bytes`, a maximal message would have spent roughly 56 s being
received.

The stress tier had measured the same thing and taken it for the container's
speed. S4's and S6's sizing comments cite "about 0.4 MB/s per stream", which is
2.35 s per MiB, and S4 was cut from 20 MiB to 10 MiB after it ran past the 60 s
data timeout and later crossed it again on a loaded run. That flakiness was F16.

**Decision:** the spilled file is wrapped in a 64 KiB `tokio::io::BufWriter`
(`SPILL_WRITE_BUFFER`), and `header_block` and `read_all` flush it before seeking
back. Sixteen blocking writes per MiB instead of 27,000. Nothing else about §8.1
changes: still an unlinked temporary file, still tmpfs, still not a spool, and
`len` counts buffered bytes exactly as before.

**Not chosen:** merging the loop's two appends per line into one, which halves the
count and leaves it per line; holding more in memory before spilling, which is
§8.1's threshold and not this decision's to move; and a std `BufWriter` inside
`spawn_blocking`, which is the same batching with more code.

**F5 is deliberately unchanged.** With writes batched, a full tmpfs surfaces when a
64 KiB batch is written rather than on the line that crossed the limit. The first
build flushed only before reading back, and S4b caught what that did: two of
sixteen sessions whose last batch was still pending met ENOSPC at that flush,
logged `ERROR reading buffered message` and were answered `451 4.3.0` — failing
the stress tier's no-ERROR log check. Which sessions land there depends on timing,
so it could not be recorded as an expected failure: an XFAIL that sometimes passes
fails the run. `MessageBuffer::finish` now writes the last batch at the terminating
dot, inside `DATA`, so a full spill area fails exactly where and how it always did.
Answering those clients `451` rather than dropping them is F5's fix, and a
separate decision.

**Verified:**

- **Red, then green.** `spilled_appends_in_data_line_shape_are_fast` appends 4 MiB
  in the `DATA` loop's exact shape under a 2 s bound. Before the fix it failed at
  7.86 s; after it, the whole buffer module runs in 0.02 s.
- **Through the shipped image**, on the idle stress stack — p50 of three messages
  per size, one at a time:

  | body | 1 MiB | 2 MiB | 3 MiB | 4 MiB | 5 MiB |
  |---|---|---|---|---|---|
  | before | 45 ms | 2,430 ms | 4,747 ms | 7,131 ms | 8,772 ms |
  | after | 48 ms | 55 ms | 57 ms | 66 ms | 74 ms |

- **S4**, eight 10 MiB messages at four concurrent: its sizing comment records
  about 23 s per message; after the fix all eight took 0.57 s (p50 279 ms), with
  peak anon at 33 MiB.
- **S4b**, re-run on the final build: every check `ok` including `log`, and F5
  XFAILed exactly as before — 6 of 16 clients dropped without a reply, 10
  accepted, none answered `4xx`.
- `cargo test` 869 passed across 29 binaries, `clippy -D warnings`, `fmt` and
  `cargo deny check` clean.

### D-081 — `timeouts.session` is enforced at waits on the client, never mid-relay (finding F2)

**Found:** by the test programme — `tests/findings.rs`, stress S9 and soak V4
(`docs/SOAK.md` §8). The session ceiling was a `tokio::select!` around the whole
session in `smtp::handle`, and when it fired during the downstream conversation it
dropped the relay future:

- The downstream had stored the message, and the client was told `421`. It would
  retry, so the recipient got the message twice.
- `reserve_relay_commit`'s commit or release never ran. The route's ledger was
  short by every cut, and the reservation row waited for the sweeper, which
  releases it as if the send had failed.
- The entry stayed in the §10.4 registry for the life of the process, so
  `simmer_reservations_in_flight` climbed by one per cut: eleven an hour per
  instance in V4's hours.

**Decision:** the deadline is a field of the session, fixed when the session
starts, and it caps every wait inside the session: the command read, the `DATA`
read, the `STARTTLS` handshake and D-079's permit wait. When the cap, not the
stage's own budget, is what runs out, the reply is `421 4.4.2 session timeout`, as
before.

With no time left, a read is refused outright rather than attempted.
`tokio::time::timeout` polls its future once before it looks at the clock, so a
command the client had already pipelined would still be served, and a client that
kept its pipe full would never be refused.

A relay in flight is never cut. It finishes, the client gets the real reply, and
the client's next command is answered `421`. The timer arm of `handle`'s
`select!` is gone. §10.4's hard stop stays.

**The refusal answers the next command; it does not arrive unprompted.** The
first build sent `421` and closed the socket the moment the deadline was found to
have passed. Soak V4 showed what that does to a client that is sending. Its
client got its `250` at the 300 s deadline and wrote its next `MAIL FROM` at once,
into a socket Simmer had just closed. The write failed with a broken pipe, the
client never read the `421`, and V4's `driven` check saw no session timeout at
all.

Now, when the deadline has already passed at a command boundary, Simmer waits up
to `DEADLINE_GRACE` (2 s) for the next command. It reads that command and
answers it `421 4.4.2 session timeout` without executing it. A client that sends
nothing gets the `421` when the grace runs out. The grace adds at most 2 s to an
idle client's overrun.

**What it costs:** a session can outlive `timeouts.session` by one relay, and a
relay is bounded. The pool checkout waits at most the route's `connect` budget
(`pool.rs`), and the conversation itself is bounded by `connect`, `command` and
`data` (§8.4). The README's "Timeout budget" puts the conversation at 160 s at the
shipped defaults, so the overrun is at most 170 s, against a 600 s ceiling. The
quota statements on either side of the conversation have no timeout until F4 is
fixed, so a stalled database stretches the overrun as it stretches everything else.

Stress S9 showed what that overrun means for a client, on the stack. Its sink
held every dot for 30 s, exactly as long as its own client waits for a reply.
The relays were no longer cut at the 20 s deadline, and they finished with `250`
at 30,003 ms. By then the client had given up, milliseconds earlier, and the sink
had four messages stored that no client heard about. Before this change, the same
run told the client `421`. Either way the client retries, and the recipient gets
the message twice.

What protects a client is the README's "Timeout budget" rule: the downstream
budget must sit comfortably under the client's own timeout. A session deadline
cannot substitute for it. S9's stall is now 25 s, inside its client's wait.

**Against the spec:** §8.4 lists only per-stage timeouts, and §4.1 has
`timeouts.session` in its schema without saying what it bounds. "A hard ceiling on
the whole conversation" was this code's own comment, not the spec's. So the change
is recorded here, not as an amendment, as agreed with the project owner on
2026-09-15.

**Not chosen:**

- **Running the relay as its own task,** so that commit or release always runs.
  That fixes the ledger and the registry, but it still tells the client `421` for a
  stored message, so the duplicate stays.
- **A guard that removes the reservation from the registry when the relay is
  dropped.** This was approved alongside the decision as defence in depth, then
  left out. After this change the only relay still dropped mid-flight is the §10.4
  hard stop's, and that is the one case the registry exists for
  (`quota/registry.rs`): `main` fires `hard_stop` and then drains the registry to
  release what the cut sessions held. A guard removing the entry on drop would race
  that drain, and any reservation it won would wait for the sweeper instead of being
  released at shutdown.

**Verified:**

- **`tests/findings.rs`'s F2 test, without its `xfail`.** The downstream holds the
  dot for 3 s against a 1 s session. The client is told `250`. The next reply is an
  unprompted `421 … session timeout`, followed by a close. The reservation is
  committed once and never released, and the registry is empty.
- **Three §8.4 tests in `tests/smtp_ingress.rs`:**
  - A client sending `NOOP` every 400 ms is still refused within its 2 s deadline,
    well inside the 5 s command budget.
  - A `DATA` transfer is cut at a 1 s deadline despite a 5 s data budget.
  - A `NOOP` pipelined behind a dot whose relay outlives the deadline gets `421`,
    not `250`. This one was seen red before green: with the zero-budget refusal
    disabled, the `NOOP` was served `250 2.0.0 ok`.
- **The gates.** `cargo test`: 880 passed across 29 binaries, which is 876 before
  plus these three and the duplicate-delivery control. `clippy -D warnings` and
  `fmt` are clean.
- **Not yet on the stack.** Stress S9 and a soak V4 run, as regression checks, and
  the compose gate follow the planted-defect control that is running on the host
  now.

### D-082 — A `DATA` line is read at most one byte past `MAX_DATA_LINE` (finding F1)

**Found:** by the test programme, in `tests/finding_f1_data_line.rs` and stress
S8a. `read_data_inner` read each line with `read_until(b'\n')` and no `take()`, so
`MAX_DATA_LINE` (64 KiB) was checked only once the whole line was in memory. One
client on an allowed address could grow a session's line buffer for as long as
`timeouts.data` allowed. The finding's test saw peak resident memory grow past its
16 MiB allowance on a 64 MiB line, and S8a's 300 MiB line pushed cgroup anon past
its bound. `max_message_bytes` did not help: it limits what is kept, not what is
read into the line.

**Decision:** the command reader's pattern (`take(MAX_COMMAND_LINE)`). Each read
takes at most `MAX_DATA_LINE + 1` bytes, and a line longer than the cap marks the
message over-long. If the cap stopped the read short of the line's LF, the rest of
the line is read and dropped a buffer at a time, up to and including that LF. It
cannot hold the terminator, which is a line of its own.

Nothing a client sees changes. The reply is still `552` at the terminating dot,
with the connection closed (D-020), exactly as for a long line that fitted in
memory. The limit is where it was: a line of 65,536 bytes with its CRLF is
accepted, and one byte more is not.

**Not chosen:** answering as soon as the line overflows, mid-`DATA`. A reply
before the dot puts a pipelining client out of step, and D-020 already settled
that an over-long message is answered at the dot and the connection closed.

**Verified:**

- **`tests/finding_f1_data_line.rs`, without its `xfail`.** A 64 MiB line with no
  LF, then the dot, gets `552`, and peak resident memory grows by less than the
  test's 16 MiB allowance. Before the fix this assertion failed, and the test
  passed only because it was wrapped in `xfail`.
- **Three §5.5 tests in `tests/smtp_ingress.rs`** pin the limit where it was:
  - 65,536 bytes with the CRLF is accepted.
  - 65,537 bytes is refused `552` at the dot. The capped read still reaches the LF.
  - 70,000 bytes is refused `552` at the dot. This one goes through the discard
    path.

  Both refusals close the connection and relay nothing. These tests pin behaviour
  that did not change, so they would have passed before the fix too. The F1 test
  is the one that went from failing to passing.
- **The gates.** `cargo test`: 883 passed across 29 binaries, which is 880 plus
  these three. `clippy -D warnings` and `fmt` are clean.
- **Stress S8a, on the stack**, on images rebuilt from `597b4a8`. One client
  reaches `DATA` and sends 300 MiB with no line ending. Every check was `ok`,
  including `memory`: peak cgroup anon was 5.5 MiB against the scenario's 96 MiB
  bound. Its known-findings entry is gone, so this was a plain pass, not an XPASS.
  The run went through a loopback forwarder, because the stress harness addresses
  published ports on `127.0.0.1` and the jail cannot reach them.

## Still open — to settle at the start of the phase that needs them

Raised during planning, defaulted as described, and worth an explicit call before
the phase that depends on each.

| # | Question | Working assumption | Needed by |
|---|---|---|---|
| ~~O-1~~ | *Settled in phase 2 — see **D-019**.* | | |
| ~~O-2~~ | *Settled in phase 3 — see **D-024**.* | | |
| ~~O-3~~ | *Settled in phase 3 — see **D-025**. The proposed extra table proved unnecessary: the override is a column on the day's own row, so it expires by construction.* | | |
| ~~O-4~~ | *Settled in phase 3 — see **D-026**.* | | |
| ~~O-5~~ | *Settled in phase 3 — see **D-027**.* | | |
| ~~O-6~~ | *Settled in phase 3: the default chain is walked normally, like any other. Implemented in `relay::resolve_chain`.* | | |
| ~~O-7~~ | *Settled in phase 3: one reservation of magnitude `recipient_count`, tested directly.* | | |
| ~~O-8~~ | ***Dissolved** in phase 6 by **D-047**, not answered: there are no splits, so there is no collapse table to return `550` from.* | | |
| ~~O-9~~ | ***Dissolved** in phase 6 by **D-047**, not answered: one recipient per transaction, so there is nothing to group and no granularity to choose between.* | | |
| ~~O-10~~ | *Settled in phase 2 — see **D-018**. The working assumption did not survive: the route is not known at `MAIL FROM`. Replaced by a config-declared capability.* | | |
| ~~O-11~~ | *Settled in phase 7 — see **D-053**. Named `admin.tokens` alongside `auth_token`, which is the token named `default`. The working assumption held: named tokens, not dropped wording.* | | |
| ~~O-12~~ | *Settled in phase 3: DST transitions both directions, a start inside a DST gap, and a future start are all tested; the leap-second case is asserted to be a no-op rather than merely argued.* | | |


---

## Phase 2 summary

### What changed

| Module | §  | What |
|---|---|---|
| `src/smtp/command.rs` | 5.2 | Command grammar as a pure function; `MAIL FROM` parameters |
| `src/smtp/reply.rs` | 5.2, 10.1 | The whole reply vocabulary in one file; downstream-text sanitising |
| `src/smtp/auth.rs` | 5.3 | `AUTH PLAIN`/`LOGIN`, argon2id, three-strike lockout |
| `src/smtp/buffer.rs` | 8.1 | Memory → tmpfs spill at 1 MiB, dot transparency |
| `src/smtp/session.rs` | 5.2–5.6 | The state machine, PIPELINING-aware |
| `src/smtp/mod.rs` | 5.1, 10.4 | Listener, `allowed_cidrs`, session cap, two-phase shutdown |
| `src/downstream/stream.rs` | 8.2 | All four TLS modes over rustls |
| `src/downstream/client.rs` | 8.2–8.4 | The outbound conversation, per-stage timeouts |
| `src/downstream/outcome.rs` | 10.1, 10.2 | The reply mapping as data, with D-008's split |
| `src/relay.rs` | 3.2, 5.4 | decide → *(reserve)* → relay, the O-1 seam |
| `src/metrics.rs` | 9.1 | Named counters, no exporter yet |

241 tests (was 74): 146 unit, 43 config validation, 21 reply mapping, 26
validation integration, 5 shipped config.

### What is tested

Command grammar including quoted local parts and parameter parsing; the reply
vocabulary including a mechanical §14.1 audit that fails when an undocumented
`5xx` is added; sanitisation against reply-splitting; both AUTH mechanisms and
the timing-equalised unknown-username path; the buffer across the spill boundary
and the dot-stuffing round trip; §10.1 exhaustively, as a pure function *and*
against a scripted downstream that returns arbitrary codes, stalls, drops
mid-`DATA`, drops after the terminating dot, and refuses TLS; PIPELINING
including a batch containing an error; every limit in §5.5 and §5.6; the CIDR
and session caps; command, data and session timeouts.

> Since D-047, §5.5's `max_recipients` has no reachable limit to test and §5.6's
> switch is gone. The two tests named here became one: a second `RCPT TO` is
> refused whatever `max_recipients` says.

The end-to-end assertion phase 4 has to keep passing: a body containing a bare
dot line, a dot-prefixed line and a trailing blank line arrives at the downstream
**byte for byte**.

### What is not tested

- **Real TLS.** The `required_verify` path is asserted only through its failure
  modes; there is no test with a real certificate. §12.3's acceptance suite
  (phase 10) is the place for that.
- **Concurrency under load.** §12.3's "N concurrent sessions against a route with
  N-1 remaining allowance" needs quota, so it belongs to phase 3.
- **`max_concurrent_sessions` under genuine contention** — the test holds
  connections open rather than racing them.
- The `Stream::Taken` variant's error path, which is unreachable by construction.

### Things the spec did not cover

Recorded above as D-018 through D-023. In brief: the `SMTPUTF8` advertisement
problem (§5.2 promises a capability Simmer cannot guarantee); line-length limits
and what to do with the remainder of an oversized `DATA`; where a downstream
`5xx` at `EHLO`/`AUTH` belongs in D-008's split; and whether `550 5.6.0` depends
on the matching rule or on any rule.

Two smaller calls not worth their own entry:

- **`RCPT TO` addresses are not validated.** §5.2 lists no syntax rule for them
  and RFC 5321 §4.5.1 requires bare `postmaster` to be accepted. The downstream
  is the authority on deliverability, and pre-validating would mean Simmer
  emitting `550`s about recipients on its own initiative — precisely what §14.1
  is about.
- **`AUTH=` on `MAIL FROM` is accepted and dropped.** §5.3 makes the
  authenticated identity irrelevant to routing, so there is nothing to do with an
  unverified assertion except not forward it.

### Carried into phase 3

`src/relay.rs` marks the two seams: the §3.2 step 3 eligibility walk replaces one
`chain.iter().find_map(...)`, and `reserve()` goes between `select()` and
`deliver()`. `downstream::Outcome` already carries the `commit` flag every row of
§10.1 owes the reservation protocol, so phase 3 consumes it rather than
re-deriving it. O-2 through O-7 and O-12 are still open and due then.


---

## Phase 3 summary

### What changed

| Module | § | What |
|---|---|---|
| `migrations/…_quota.sql` | 7, 11 | `quota_usage`, `quota_reservation`, `route_state` |
| `src/quota/day.rs` | 7.2 | Day-index arithmetic, in milliseconds, signed |
| `src/quota/store.rs` | 11 | The storage trait, `Usage`, `Reserved`, `RouteState` |
| `src/quota/postgres.rs` | 7.4 | The three-phase protocol, under a row lock |
| `src/quota/mod.rs` | 7.2, 7.4 | `Allowance` resolution, reservation expiry |
| `src/quota/registry.rs` | 10.4 | Reservations this process holds |
| `src/quota/sweeper.rs` | 7.4 | Expired-reservation release |
| `src/models/quota.rs` | 11 | Runtime `sqlx`, free functions, house pattern |
| `src/models/route_state.rs` | 9.3 | `paused`, `graduated`, allowance override |
| `src/routing/domain_group.rs` | 3.2.2 | Literal, case-insensitive, catch-all fallback |
| `src/routing/chain.rs` | 3.2.3 | The walk, with a typed skip reason per route |
| `src/relay.rs` | 3.2, 7.4 | decide → reserve → relay → commit/release |

323 tests (was 241): 190 unit, 43 config validation, 30 quota storage, 26
validation integration, 21 reply mapping, 8 quota-through-the-relay, 5 shipped
config.

### What is tested

Day-index arithmetic across both DST directions, a start inside a DST gap, a
future start, a 400-day monotonicity sweep, and the leap-second no-op; the
schedule's final-value-repeats rule and graduation; domain-group resolution
including the suffix-match trap; the chain walk producing the right skip reason
per route.

Against real Postgres: reserve/commit/release round trips; a reservation counting
against headroom *before* it commits; exhausting an allowance exactly; per-group
and per-day bucket independence; the allowance staying authoritative across a
config change; the override raising, lowering and expiring; the sweeper; and both
sides of the sweeper race — committing after a sweep still counts the delivery,
releasing after a sweep does not steal another message's slot.

**§12.3's concurrency requirement, twice**: 16 concurrent reservations against 15
slots granting exactly 15, and 8 concurrent *sessions* through the real ingress
where the warming route carries exactly its allowance and one message spills to
overflow. A post-hoc increment passes everything else and fails these.

Through the relay: a failed send leaving `committed` unchanged (§12.3, verbatim),
a connect failure not falling through to the next route (§3.3), fall-through to
overflow at exhaustion, and chain exhaustion answered `451` at `RCPT TO` when
rules are envelope-only and at the final dot when they are not.

### What is not tested

- **Clock movement.** No test advances `warmup.started` across a real day
  boundary in a running process; the day-index tests are pure and the storage
  tests set `day_index` directly. §12.3's acceptance suite (phase 10) is where a
  ramp gets walked across simulated boundaries.
- **`fail_closed` behaviour** — the `451 4.3.0` path is unit-tested as a reply
  but not driven by an actually-unreachable database.
- **The sweeper's interval loop** — `sweep_once` is tested, `run` is not.
- **§10.4's reservation release under a real SIGTERM** — `release_by_ids` and the
  registry are tested; the wiring in `main` is not.

### Things the spec did not cover

D-024 through D-031. The two that changed a schema: overflow routes needing a day
origin at all (D-024), and §11's `route_state.allowance_override` being unable to
express §9.3's per-group scope (D-025).

One smaller call: **`route_state` is read once per message rather than cached.**
A cache would mean `POST /routes/{name}/pause` did not take effect immediately,
which is the one property an operator reaching for that endpoint needs. At warm-up
volumes the query costs nothing.

### Carried into phase 4

Nothing in the quota model blocks it. The rewrite engine slots between the buffer
and `downstream::relay`, both of which are already isolated behind
`relay::reserve_relay_commit`. O-8 and O-9 (phase 9) and O-11 (phase 7) remain
open.


---

## Phase 4 summary

### What changed

| Module | § | What |
|---|---|---|
| `src/rewrite/template.rs` | 6.3 | The variable table as a parsed grammar; position-aware rendering |
| `src/rewrite/encode.rs` | 6.3 | Sanitising, RFC 2047 encoding, phrase quoting, folding |
| `src/rewrite/headers.rs` | 6.1 | The header block as something editable without disturbing what it was not asked to change |
| `src/rewrite/mod.rs` | 6.1, 6.2, 6.5 | The order of operations; auth-artefact stripping; `Received:`; the outbound envelope sender |
| `src/rewrite/stability.rs` | 6.6 | The property, against a synthetic probe |
| `src/config/validate.rs` | 4.2, 6.6 | The three stability rules the phase-1 `TODO` left open, plus D-034 |
| `src/relay.rs` | 6.1, 7.4 | Rewriting between the reservation and the downstream conversation |
| `src/smtp/session.rs` | 6.1 | HELO name and peer address threaded through for `Received:` |
| `src/bin/loadgen.rs` | 12.3 | The acceptance suite's bulk sender |
| `simmer.acceptance.yaml`, `docker-compose.yml`, `Dockerfile` | 12.3 | The acceptance profile |
| `tests/acceptance.rs`, `tests/rewrite_stability.rs` | 12.3, 6.6 | The two new tiers |

452 tests, from 431: 298 unit (was 190), 43 ingress, 34 config validation, 30
quota, 24 reply mapping, 9 rewrite stability, 8 quota-through-the-relay, 5 shipped
config, 1 acceptance drift guard — plus 4 acceptance tests behind `--ignored`.

### What is tested

Every §6.3 variable; the template grammar including the malformed cases D-034
makes fatal; RFC 2047 encoding across the chunk boundary, with a decode-back
assertion that no character is split; phrase quoting for every RFC 5322 special,
with the unquote/requote round trip that keeps it idempotent; the header block
preserving lowercase names, doubled spaces, tab continuations and missing spaces
after the colon; §6.2's remove-before-set and replace-all-instances rules;
§6.5 against multiple instances; header injection through `{{original.subject}}`
and through the `EHLO` name; SMTP command injection through `envelope_from`.

`rewrite(rewrite(m)) == rewrite(m)` as a proptest over generated messages —
display names that are absent, quoted, comma-bearing, non-ASCII and
already-encoded; folded and unfolded headers; bodies that look like header blocks;
the awkward body from phase 2. Run at 20,000 cases before landing. The negative
case is asserted too: §6.6's worked example *must* fail the property, or every
other assertion in the file is vacuous.

Through the container, against two real SMTP servers: the ramp walked across three
simulated days carrying exactly its allowance each day; the excess reaching a
different provider under a different identity; the §6.2 and §6.5 rewrites as a
receiving mail server sees them; and **both arrangements of §1.1 producing the
same bytes**, which is the one claim nothing else in the suite can make.

### What is not tested

- **§6.4 body rewriting** — phase 5. `tests/acceptance.rs` asserts the body link
  is *not yet* rewritten, so the assertion fails the day it lands rather than
  being forgotten.
- **Real-certificate TLS** — the traps are plaintext. Unchanged from phase 3;
  `ACCEPTANCE.md` §5 and phase 10.
- **`fail_closed` and §10.4 under a real `SIGTERM`** — still only unit-level.
  Both are now cheap to add: the compose stack is there, and stopping `simmer-db`
  mid-run is one command.
- ~~**Multi-recipient rewriting** — `recipient.*` renders empty above one
  recipient. §5.6 splitting is phase 9 (O-9).~~ Closed by **D-047**: a transaction
  carries exactly one recipient, so `recipient.*` always renders a real value.
- **A `Received:` chain long enough to fold**, and messages above the §8.1 spill
  threshold *through the rewrite* — the spill test covers the buffer, not a
  rewritten 25 MiB message.

### What the harness caught

Worth recording, because it is the argument for having built it in this phase
rather than phase 10.

Its first full run was **green on three of four tests, for the wrong reason**.
`SIMMER_WARMUP_STARTED` was not reaching the container (D-042), so every test ran
at day index 9 — past the end of a three-day schedule, where §7.2's
final-value-repeats rule gives an allowance of 20. Nothing hit a ceiling, nothing
fell through to overflow, and the routing test's assertions were loops over an
empty trap, which pass. The rewrite and cutover-invariant tests were genuinely
passing; the ramp test was the only one that failed, and it was the only one
looking at a number.

Two lessons, both applied: assert the counts *before* iterating over content, or
an empty collection passes everything; and a suite that drives infrastructure has
to verify the infrastructure took the settings it was given.

### Things the spec did not cover

D-034 through D-042. The three that change behaviour a reader would not predict
from `SPEC.md`:

- **D-035** — the null sender is never rewritten, so bounces stay bounces.
- **D-037** — `Received:` is the only header the engine adds on its own.
- **D-040** — a stale `unstable_headers` declaration is now a `WARN` rather than a
  fatal error, which *loosens* phase 1's validation.

And **D-036 is not a decision but a defect in `SPEC.md`**: §4.1's example
configuration fails §4.2's stability rule, which §6.6 makes non-overridable. The
shipped `simmer.yaml` was corrected; `SPEC.md` was not touched.

Two smaller calls not worth their own entry:

- **`original.header["X-Foo"]` returns the raw value, not a decoded one.** §6.3
  decodes `original.subject` and the `From:` parts because it says so explicitly
  ("Decoded subject"); the arbitrary-header escape hatch has no such wording, and
  raw is the more predictable answer for a header whose semantics Simmer cannot
  know.
- **A `set_headers` entry whose template renders to empty still writes the
  header**, producing `X-Foo: ` rather than omitting it. Omitting would make the
  presence of a header depend on the message, which is a relative behaviour in
  §1.1's sense. The `envelope_from` case is the exception and is handled
  explicitly, because an empty envelope sender means something specific (D-035).

### Carried into phase 5

§6.4 body rewriting is the only part of §6 still missing, and `rewrite::headers`
is deliberately shaped for it: `split()` already hands back the body as its own
slice, and nothing else in the engine touches it. The `regex` dependency and the
`body_rewrites` validation have been in place since phase 1.

The one design point to settle first: §6.4 requires decode → rewrite → re-encode
per `text/*` part, which means the body stops being byte-preserved and D-039's
structural guarantee becomes a tested one. `tests/acceptance.rs`'s "not yet
rewritten" assertion is the reminder.

O-8 and O-9 (phase 9) and O-11 (phase 7) remain open. The `smtp/auth.rs` timing
defect above is still unfixed and still worth fixing on its own.

---

## Phase 5 summary

### What changed

| Module | § | What |
|---|---|---|
| `src/rewrite/transfer.rs` | 6.4 | Quoted-printable and base64, decoder *and* matching encoder |
| `src/rewrite/charset.rs` | 6.4 | UTF-8, US-ASCII, ISO-8859-1, Windows-1252 (D-044) |
| `src/rewrite/mime.rs` | 6.4 | The MIME structure as byte ranges into the body (D-043) |
| `src/rewrite/body.rs` | 6.4, 6.6 | The engine: compiled rules, per-part decode/rewrite/re-encode, the fixed-point check |
| `src/rewrite/mod.rs` | 6.1 | Step 7, in its place — after `set_headers`, before `Received:` |
| `src/config/validate.rs` | 4.2, 6.6 | D-046's fatal rule; `body_rewrites.pattern` compilation moved onto the same path as the templates |
| `src/metrics.rs`, `src/relay.rs` | 9.1, 6.4 | `simmer_body_rewrite_skipped_total{route,reason}` |
| `simmer.acceptance.yaml`, `tests/acceptance.rs` | 12.3 | `ACCEPTANCE.md` §4.3's last row, which phase 4 left as a deliberate reminder |

537 tests, from 452: 375 unit (was 298), 43 ingress, 36 config validation, 30
quota, 28 reply mapping, 11 rewrite stability, 8 quota-through-the-relay, 5
shipped config, 1 acceptance drift guard — plus the 4 acceptance tests behind
`--ignored`, all of which still pass with body rewriting live.

**No new dependencies.** The phase was budgeted for a charset crate and did not
need one; `LICENSES.md` §4 records why, since the reasoning was licence-adjacent
even though nothing was adopted.

### What is tested

- **The transfer codecs round-trip every byte.** `encode` then `decode` returns
  the input for all 256 byte values, for each of the three encodings. This is the
  claim `body.rs` makes when it puts a part back, so it is asserted rather than
  assumed. Same for the single-byte charsets, byte by byte.
- **§6.4's headline case**, three tiers deep: `https://old=\r\nbrand.com/x` is
  matched and rewritten as a unit test, through the relay against a scripted
  downstream, and — as the plain form — off a real Mailpit trap.
- **The MIME spans reassemble the message byte for byte.** Copying the untouched
  spans back in order reproduces the original, for a single part, a multipart with
  a preamble and epilogue, and a nested multipart. This is what licenses D-043's
  claim that an unmatched part is unchanged.
- **"Nothing matched, so nothing changed"**, as two proptests over generated
  messages: once for a route with no rules, once for the shipped route over bodies
  chosen to contain no match *under any decoding*. Filtering the general generator
  would not have worked — `aHR0cHM6…` is a match once base64 is undone.
- **§6.6 with the body live.** `tests/rewrite_stability.rs`'s SHIPPED route now
  carries the shipped `body_rewrites`, and the generated bodies include a live
  match, an already-migrated body, a soft-broken URL, a base64 text part, a
  multipart with a matching text part beside an attachment encoding the same
  string, and a signed message. `rewrite(rewrite(m)) == rewrite(m)` holds over all
  of them.
- **§6.4's exclusions**, positively: a signed message arrives at the downstream
  byte-identical with a live match inside it, and the base64 attachment beside a
  rewritten text part comes back untouched.
- **D-046 at startup**, both directions: `s/…/…?ref=1/` is refused, and a chain
  where rule 2 consumes rule 1's output is accepted.

### What is not tested

- **A part above the §8.1 spill threshold through the body rewriter.** Carried
  from phase 4 and now sharper: the engine copies the whole body when any part
  matched, so a 25 MiB message with one matching part allocates 25 MiB. Nothing
  measures that.
- **Windows-1252 and ISO-8859-1 end to end.** Unit-tested byte by byte and through
  `body.rs`, but no message in the relay or acceptance tiers carries one.
- **A malformed part reaching the metric.** `SkipReason` is asserted at the
  `body.rs` boundary and the relay call site is one line, but no test observes the
  counter — there is still no recorder installed (phase 7, D-021).
- **RFC 2231 parameter continuations.** A `boundary*0=`/`charset*=` spelling is not
  reassembled, so the parameter is simply not found. That lands the part in
  §6.4's "cannot be decoded" case rather than producing a wrong answer, which is
  the safe direction, but it is untested because no mailer writes them.

### Things the spec did not cover

D-043 through D-046. The two a reader would not predict from `SPEC.md`:

- **D-045** — Simmer never changes a part's `Content-Transfer-Encoding`, so the
  fix-up §6.4 asks for never happens. A rewrite that would need one does not
  happen either.
- **D-046** — an unstable `body_rewrites` chain is a fatal startup error that
  nothing can downgrade. §6.6's two field classes do not cover the body, and this
  adds a third with the identity fields' severity.

Three smaller calls not worth their own entry:

- **`body_rewrites` replace every occurrence, not the first.** §6.4 says "as a
  regex replacement" without saying which. Replacing only the first would make the
  result depend on where in the body a link appeared, which is exactly the kind of
  relative behaviour §1.1 rules out.
- **Capture references (`$1`) work in a replacement**, because that is what the
  `regex` crate's `replace_all` does and taking it away would need an escape pass
  of its own. `$$` escapes a literal dollar.
- **Where two `Content-Type` headers exist, the first wins**, inherited from
  `HeaderBlock::get`. The message is malformed either way; first-wins at least
  matches what §6.3's `original.header[…]` already does.

### Carried into phase 6

Nothing in §6 is outstanding. `body_rewrites` was the last unimplemented part of
the rewriting engine, and §6.7's DNS preflight — the only other §6 subsection with
no code — is phase 8 by §13's own ordering.

Phase 6 is §7.3 recipient frequency: hashing, normalisation and the sweeper. The
`recipient_event` table does **not** exist yet — D-029 deferred it out of phase 3
precisely so its row shape could be decided alongside them. `config::RecipientFrequency`
is parsed and validated, and `chain::SkipReason::Frequency` exists and is never
produced: the chain walk has a hole where the check goes.

O-8 and O-9 (phase 9) and O-11 (phase 7) remain open; none of them is phase 6's.
The `smtp/auth.rs` timing defect above is still unfixed and still separable.

> **What actually happened at the start of phase 6.** D-047 landed first: multi-
> recipient transactions are refused outright, which dissolved O-8 and O-9 and made
> §13 phase 9 void. Only O-11 (phase 7) is still open. The frequency check that
> follows therefore sees exactly one recipient, always.

---

## Phase 6 summary

Two changes, in order: D-047's reversal of §5.6, then §7.3 itself.

### What changed

| Module | § | What |
|---|---|---|
| `src/config/mod.rs`, `src/smtp/session.rs`, `src/smtp/reply.rs` | 5.5, 5.6 | D-047: `single_recipient_only` deleted, the second `RCPT TO` always refused, `too_many_recipients` removed |
| `src/config/validate.rs` | 4.2 | `removed_keys` names a key a previous version accepted; two new warnings (vestigial `max_recipients`, D-052's last-link constraint); the dead §6.3 recipient-template warning deleted |
| `docs/RECIPIENTS.md` | 5.6, 13 | The reversal in full, for the spec's author — `docs/INGRESS.md`'s shape |
| `migrations/20260810000000_recipient_event.sql` | 7.3, 11 | D-048's row shape, which D-029 deferred out of phase 3 |
| `src/frequency/mod.rs` | 7.3 | Normalisation, the keyed hash, the rolling window, the retention |
| `src/frequency/sweeper.rs` | 7.3 | Hourly eviction, on the same shutdown token as `quota::sweeper` |
| `src/models/instance_config.rs` | 11 | D-050's idempotent get-or-insert |
| `src/models/recipient_event.rs` | 7.3 | Count, record, evict |
| `src/quota/store.rs`, `src/quota/postgres.rs` | 7.3, 7.4 | Three new trait methods; `commit` takes the keys, so phase 3's transaction now carries both halves of §7.4 phase 3 |
| `src/routing/chain.rs` | 3.2 3b | The check, producing the `SkipReason::Frequency` that had existed unconstructed since phase 3 |
| `src/relay.rs`, `src/main.rs` | 7.3 | The keys carried from walk to commit; the sweeper started only when a route declares a constraint |
| `src/metrics.rs` | 7.3 | `simmer_recipient_events_evicted_total`, deliberately unlabelled |

593 tests, from 537: 405 unit (was 375), 44 ingress, 40 config validation, 30
quota, 28 reply mapping, **21 frequency (new)**, 11 rewrite stability, 8
quota-through-the-relay, 5 shipped config, 1 acceptance drift guard — plus the 4
acceptance tests behind `--ignored`, all of which still pass.

**Two new direct dependencies that add nothing to the build**: `sha2` and `hmac`,
both already compiled as transitive dependencies of `sqlx-postgres`. `LICENSES.md`
§6 records the check and why `argon2`, already present, is the wrong tool here.

### What is tested

- **§7.3's normalisation, case by case.** The spec's own headline pair
  (`Bob.Smith+news@gmail.com` ≡ `bobsmith@gmail.com`), the `+` rule applying at
  every domain, the dot rule applying at *only* the configured ones, dots in the
  domain never touched, a quoted local part left entirely alone, and the last `@`
  as the separator.
- **The key is not the address.** Asserted twice: no substring of the address
  appears in the key bytes, and none appears in `Key`'s `Debug` form. Then again
  against the database — after a real delivery, the stored `recipient_hash` is 16
  bytes and contains neither `bob` nor `gmail`.
- **The window is rolling**, against stored timestamps rather than arithmetic: an
  event 30 hours old is outside a 24-hour window and inside a 48-hour one, and two
  events 25 and 26 hours old do not keep a route ineligible.
- **The salt survives.** Two stores over one database agree, including when they
  race for the first insert.
- **§7.4 phase 3, both halves.** A delivered message records exactly one event; a
  message the downstream refused at the final dot records none.
- **The whole feature end to end**: three messages to one recipient, the first two
  leaving by the warming route and the third by the overflow route *under the
  overflow identity* — steered, not dropped. And the same with three different
  spellings of one Gmail inbox, which is the case §7.3 exists for.
- **§10.3 when the chain runs out**: over threshold with nothing to fall through
  to is `451 4.7.1`, never `550`.
- **The sweeper's cutoff**, either side of it, and the interval loop itself —
  which `quota::sweeper` still lacks (`sweep_once` is tested there, `run` is not).
- **D-047**: a second `RCPT TO` refused whatever `max_recipients` says, the
  transaction still usable for the recipient that was accepted, every downstream
  transaction carrying exactly one recipient, and a config carrying the removed
  key refused by name.

### What is not tested

- **The acceptance tier does not exercise §7.3.** `simmer.acceptance.yaml`
  declares no `recipient_frequency`, so the ramp assertions measure quota alone.
  Adding one would need the loadgen to send repeatedly to *one* recipient, which
  is a different shape of run from the bulk it does now. The relay-level coverage
  in `tests/frequency.rs` is against real Postgres and a real downstream, so what
  is missing is only the real-mail-server leg.
- **No test drives the frequency check with an unreachable database.** The path is
  the same `QuotaError` → §7.5 → `451` every other storage failure takes, and that
  mapping is tested, but the specific "salt could not be resolved" case is not.
  Same gap `fail_closed` has had since phase 3.
- **Nothing observes `simmer_recipient_events_evicted_total`**, for the same
  reason as every other counter: there is no recorder until phase 7 (D-021).
- **No test of two concurrent messages to one recipient racing the check.** D-049
  argues the race is benign and bounded at one extra message; it is argued rather
  than demonstrated.
- **The dot-insensitive list is not validated.** A typo (`gmial.com`) silently
  turns the folding off for the domain it was meant for. §4.2 has no rule for it
  and inventing one would mean deciding what a "valid provider domain" is.

### Things the spec did not cover

D-047 through D-052. The two a reader would not predict from `SPEC.md`:

- **D-047** — §5.6 reversed and §13 phase 9 deleted. The largest divergence so
  far, and the only one that removes a numbered phase. `docs/RECIPIENTS.md`.
- **D-051's per-route counting.** §7.3 does not say whether a send via the
  overflow route counts against the warming route's window. §11's row carries
  `route`, so it counts per route — which means it does not. The recipient did
  receive the message, so there is a real argument for the other answer; it needs
  the spec, not an implementation.

Two smaller calls not worth their own entry:

- **"At or over" is the threshold test**, per §3.2 3b's wording, so `threshold: 3`
  admits two messages and steers the third. §7.3 alone would have allowed the
  off-by-one reading.
- **A domainless recipient** (`RCPT TO:<postmaster>`, legal SMTP) keys on the whole
  string in both modes rather than on an empty domain, so it shares a bucket only
  with itself.

### Carried into phase 7

Phase 7 is the admin API, the metrics exporter and dry-run. **O-11 is its open
question** and the only one left: §9.3 logs mutations "with the acting token's
identifier", but `admin.auth_token` is a single scalar with no identity.

Everything the exporter needs is already recorded through `src/metrics.rs`,
including this phase's two additions. The `smtp/auth.rs` timing defect above is
still unfixed and still separable.

---

## Phase 7 summary

§9 in full: the metrics exporter, the read API, the write API and dry run. O-11
was settled first, as the working agreement asks, and it shaped the rest — the
audit line is what named tokens exist for.

### What changed

| Module | § | What |
|---|---|---|
| `src/config/mod.rs` | 4.1, 9.3 | D-053's `admin.tokens`; `auth_token` becomes optional and is the credential named `default`; `Config::chains()` promoted out of `validate.rs`, where §9.3 now needs it too |
| `src/config/validate.rs` | 4.2 | Four new rules over the admin credentials, and a short-token warning |
| `src/metrics.rs` | 9.1 | `install()` — the Prometheus recorder, with real histogram buckets — and `describe()`, a `# HELP` line per metric. D-054's `domain_group` label. Two new counters for the write API |
| `src/admin/mod.rs` | 9.1, 9.2 | The router; `/routes`, `/routes/{name}`, `/quota`, `/metrics`; the D-056 gauge refresh |
| `src/admin/view.rs` | 9.2 | The projections, as pure functions. D-026's drift flag lives here |
| `src/admin/auth.rs` | 9.3 | The bearer token, constant-time, as an axum extractor. D-053 and D-055 |
| `src/admin/error.rs` | 7.5, 9 | One error shape; a storage failure is `503`, not `500` |
| `src/admin/mutate.rs` | 9.3 | The four mutations, the audit line, and D-057's exhaustion warnings |
| `src/admin/dryrun.rs` | 9.4 | D-059 — the real engine over a synthesised message |
| `src/routing/chain.rs` | 3.2, 9.4 | `dry_walk`: the same order, reserving nothing and counting nothing |
| `src/quota/store.rs`, `src/quota/postgres.rs`, `src/models/quota.rs`, `src/models/route_state.rs` | 9.2, 9.3, 11 | Five new trait methods: `usage_many`, the three setters, and D-058's `reset_counters` |
| `src/rewrite/body.rs` | 6.4, 9.4 | `Rules::match_report` — sequential match counts, off the message path |
| `src/rewrite/headers.rs` | 9.4 | `HeaderBlock::fields()`, a position-aware view of the block |
| `src/relay.rs`, `src/main.rs` | 9.1 | The recorder installed before anything counts; the admin state now holds the engine |
| `src/healthcheck.rs` | 12.2 | D-060 — `hs_utils::healthcheck` lifted in verbatim; the `hs-utils` dependency, `deny.toml`'s `allow-git` list and its `ignore-sources` exemption all removed |

709 tests, from 593: 458 unit (was 405), **47 admin API (new)**, 44 ingress, 46
config validation (was 40), 30 quota, 28 reply mapping, 21 frequency, 11 rewrite
stability, **10 metrics endpoint (new)**, 8 quota-through-the-relay, 5 shipped
config, 1 acceptance drift guard — plus the 4 acceptance tests behind `--ignored`,
which still pass and which this phase does not touch.

`tests/metrics_endpoint.rs` is its own binary deliberately: `metrics` permits one
global recorder per process, so a test that needs a real one has to be alone with
it. Every other suite builds its state with `metrics: None` and exercises the
no-op recorder phases 2–6 ran against.

**Two new direct dependencies**, plus two dev-only ones, all permissive:
`metrics-exporter-prometheus` with `default-features = false` (the default set
pulls hyper and a push-gateway client, and axum already owns the port) and
`subtle`, already compiled since phase 2 via `argon2`. `LICENSES.md` §7.

**And one removed.** D-060 drops `hs-utils`, which existed for a single
stdlib-only function. Simmer now has no git dependencies at all, and the only
crate in its graph without a `license` field is itself.

### What is tested

- **The §9.2 projections, without a database.** `view.rs` is pure functions, so
  the case that matters — D-026's drift, where the row says 100 and the schedule
  says 200 — is a unit test rather than a fixture. Then again end to end against
  a real row.
- **§9.4 answers about the relay, not about itself.** Five tests walk the real
  `walk_and_reserve` and `dry_walk` over the same state and assert identical
  evaluations: everything eligible, paused, quota exhausted, not started, and
  over the §7.3 frequency threshold. The last two are the ones that would catch a
  reordering, because `walk_and_reserve` checks frequency *before* the day index.
- **A dry run writes nothing.** Asserted against the tables: no
  `quota_reservation` row after five calls, and no `quota_usage` row after one —
  the second being the stronger claim, since an inserted row would fix today's
  ceiling at whatever the schedule said when somebody tested a configuration.
- **Every protected endpoint refuses both an anonymous request and a wrong
  token**, from one table, so a route added without authentication fails the
  suite. A missing token and a wrong one produce byte-identical responses.
- **The mutations reach the message path.** Pausing a route through HTTP and then
  running the real chain walk yields `warming=paused,overflow=selected`; an
  override of 3 over a schedule of 1 lets three reservations through.
- **D-058's arithmetic**, with a live reservation held across the reset:
  `committed` goes to zero and `reserved` stays at one.
- **D-057's warnings.** Pausing one route of two produces none; pausing both
  produces warnings naming the chains and the `451` clients will see.
- **The gauges are right with nothing having relayed.** The scrape asserts the
  Google override series, the default series and `+Inf` for the overflow route,
  in a process where no message has ever been sent.
- **D-021's claim, finally testable.** The counters written in phases 2–6 against
  a null recorder produce real series once a recorder exists, with no change to
  any call site but D-054's.
- **The healthcheck subcommand**, which had no tests at all as an external
  dependency: argument parsing in every documented form, a real one-shot HTTP
  server asserting the exact request line for both paths, every non-`200` status,
  nothing listening, and an unresolvable host.
- **§7.3, applied to the control plane.** Every read endpoint is asserted to
  contain no recipient, no recipient key, and no address-shaped value, after a
  real recipient has been put through the system. The dry run echoes the address
  the caller supplied and no other. The evicted-events counter is asserted to
  carry no labels at all.

### What is not tested

- **No test drives the admin listener over a socket.** Everything goes through
  `oneshot` against the router, so the bind, the graceful-shutdown wiring in
  `main` and the real TCP path are exercised only by `docker compose up`.
- **`/metrics` under a failing store.** The handler logs and renders anyway; that
  branch is reasoned about, not driven.
- **Concurrent mutations.** Two operators pausing and resuming the same route
  interleave as last-writer-wins, and the audit line's "previous" is read before
  the write. Correct, and unproven.
- **The `403`-shaped case does not exist.** Every token can do everything; there
  are no scopes. A token that could read but not mutate is a plausible next ask
  and is not built.
- **Nothing asserts the audit line itself.** The `INFO` record is emitted through
  `tracing` and no test captures a subscriber to read it back, so what is proven
  is that the mutation happened and what the response said, not what was logged.

### Things the spec did not cover

D-053 through D-059. The three a reader would not predict from `SPEC.md`:

- **D-055** — §9.2's reads need the token, which §9.3's wording does not require.
  The reasoning is that `/routes` discloses the entire routing shape and every
  downstream hostname, and §2.3's trusted-segment assumption is an assumption.
- **D-056** — the §7 gauges are recomputed on every scrape. Without it they are
  only ever set by a message that relayed, so an idle route exports yesterday's
  ceiling under today's label set. §9.1 does not say, and the naive reading is
  wrong in the direction that looks fine on a dashboard.
- **D-057** — an allowance of zero is allowed and warned about rather than
  refused. §9.3 offers the capability and §14.1 governs the *reply*, which stays
  `451` either way; what the spec does not provide is any way for an operator to
  discover they have just made every message on a chain temporary-fail.

Two smaller calls not worth their own entry:

- **`POST /routes/{name}/graduate` accepts `{"graduated": false}`.** §9.3 names
  only the forward direction, but graduation pins a route to its *final*
  allowance immediately and an operator who does that to the wrong route needs a
  way back that is not a manual `UPDATE`.
- **Graduating an overflow route is a `400`.** §3.1 gives it no warm-up schedule,
  so there is no final value to pin it to, and storing the flag would report
  success for a no-op.

### Carried into phase 8

Phase 8 is §6.7's DNS preflight, which is the last of §6 and which supplies the
two §9.1 metrics this phase left absent by dependency —
`simmer_preflight_ok{route,check}` and the `preflight` skip reason, which
`SkipReason::Preflight` has carried unconstructed since phase 3. `/routes`
reports `"preflight": null` today, and phase 8 is what fills it in.

**There are no open questions left.** All twelve are closed or dissolved.

Separately, and now scheduled: the **multi-instance** question. `README.md`'s
deployment section and D-007 say that two Simmers against one database is the
window in which quota overshoot occurs, and reading the code says otherwise —
§7.4's reservation takes a row lock that serialises contenders whatever process
they are in. What genuinely blocks two instances is narrower: D-049's frequency
race, and config skew during a roll under D-026. That correction, a two-pool
concurrency test to evidence it, and a `docs/MULTI_INSTANCE.md` for the spec's
author are the next piece of work after this phase.

> **Done, 2026-08-11 — see D-061 and `docs/MULTI_INSTANCE.md`.** It held up:
> quota is cross-instance safe by the row lock, evidenced by
> `tests/quota_multi_instance.rs` racing two independent pools, and falsified
> against an unlocked `lock_usage` to prove the tests have teeth. The D-049 bound
> turned out to be `threshold + (C - 1)` for peak concurrency `C`, not the flat
> "one extra message" the plan assumed. No code changed.

The `smtp/auth.rs` timing defect is still unfixed and still separable.

> **Fixed in phase 10 — D-066.**

---

## Phase 10 summary

§13's last phase, minus the two parts of it that were already done: the acceptance
suite landed in phase 4 (D-032, D-042), and §8.4's timeout-budget documentation has
been in `README.md` since phase 2. What was left was §8.3's pool, §10.4's fourth
clause, and the `smtp/auth.rs` defect that has been carried since phase 2.

Phase 9 is not here and never will be: D-047 refuses multi-recipient transactions
outright, so the splitting and result-collapse phase has nothing to build.

### What changed

| Module | § | What |
|---|---|---|
| `src/downstream/pool.rs` | 8.3 | **New.** The per-route pool: `max_connections` as a semaphore (D-067), `idle_ttl`, `max_messages_per_connection`, `NOOP` validation, and §10.4's drain |
| `src/downstream/client.rs` | 8.3, 10.2 | `relay` checks a connection out instead of dialling; D-068's single retry; `noop`/`rset`; `Budget` moves to the pool, resolved once per route |
| `src/downstream/outcome.rs` | 10.1, 14.1 | `RelayError::PoolExhausted` → `451 4.4.5`, its own error class; `Stage::Keepalive` |
| `src/smtp/auth.rs` | 5.3 | **D-066** — the decoy borrows the ACL's costliest parameters and salt, replacing only the digest |
| `src/metrics.rs` | 9.1 | `simmer_pool_connections{route,state}`, the last unemitted §9.1 metric, plus `simmer_pool_retries_total` |
| `src/admin/mod.rs` | 9.1 | The pool gauges refreshed on every scrape, for D-056's reason and one more |
| `src/admin/view.rs` | 9.2 | `pool` stops being `null` and becomes `PoolStats` |
| `src/relay.rs`, `src/main.rs` | 8.3, 10.4 | `Engine.pools`, built once; the drain, after the grace period and the reservation release |
| `tests/support/mod.rs` | 12.3 | The fake counts connections and logs commands, and can answer an `RSET` then vanish |

771 tests, from 747: 485 unit (was 474), 48 admin API (was 47), 50 config
validation, 44 ingress, 30 quota, 28 reply mapping, 21 frequency, 13 preflight,
**11 pool (new)**, 11 rewrite stability, 11 metrics endpoint (was 10), 8
quota-through-the-relay, 5 multi-instance, 5 shipped config, 1 acceptance drift
guard — plus the 4 acceptance tests behind `--ignored`.

### What is tested

- **What the downstream sees, not what the pool thinks.** Every assertion in
  `tests/pool.rs` is a count of accepted TCP connections or of command lines on
  the wire: three messages over one connection, an `RSET` between each, one `EHLO`
  and no re-authentication, four messages at two per connection producing two
  connections and two `QUIT`s.
- **The retry, falsified.** Disabling the condition in `client::relay` makes
  `a_connection_the_downstream_closed_costs_a_reconnect_and_not_the_message` fail
  and nothing else — so the test measures the retry and not the fixture.
- **§10.2 is not retried.** The downstream takes the whole message and drops
  without replying to the dot; the client gets `451 … delivery unknown` and the
  body was offered exactly once.
- **The bound.** With `max_connections: 1` and a downstream stalled at the final
  dot, the second session is answered `451 4.4.5` and **no second connection is
  opened** — the assertion a socket cache would fail.
- **The statistics agree with the wire.** `stats.opened` is asserted equal to the
  fake's accepted-connection count, so §9.2 cannot report a fiction.
- **D-066, four ways**, including that the decoy is not a copy of anybody's hash
  and that the right password under the wrong username still fails.

### What is not tested

- **§10.4's drain under a real `SIGTERM`.** `Pool::drain` is driven directly and
  asserted to `QUIT` its idle connections; the wiring in `main` is exercised only
  by `docker compose up`. The same gap `release_by_ids` has had since phase 3.
- **Concurrency against the bound.** The saturation test uses one permit and two
  sessions. Nothing drives sixteen sessions at four permits and asserts the pool
  never exceeded four — the invariant is argued in `pool.rs`'s module comment and
  enforced by the semaphore rather than measured.
- **A pooled connection over TLS.** Every pool test runs `tls: off`. The `Stream`
  is the same object either way and `open` is unchanged, but no test carries a
  reused connection through a completed handshake.
- **The `NOOP` validation path.** `VALIDATE_AFTER` is five seconds, so exercising
  it costs five seconds of wall clock per test; the `idle_ttl` path is driven
  instead, at one second. What is untested is specifically the branch where a
  connection is validated and *passes*.

### Things the spec did not cover

D-066 through D-068, and the five smaller calls listed under them. The two a
reader would not predict from `SPEC.md`:

- **D-067** — §8.3 asks the pool to bound concurrency and §10.1 has no row for
  what happens when the bound binds. `451 4.4.5` as its own class is the answer,
  and the reasoning is §14.1's: a saturated pool is Simmer's problem and must never
  be charged to a recipient.
- **D-068** — nothing in the spec says what to do when a *reused* connection dies
  mid-conversation, and the naive answer (report it like any other failure)
  manufactures deferrals out of Simmer's own optimisation. The retry's three
  conditions are each there to stop it becoming the other failure mode, which is a
  duplicate message.

## Phase 11 summary

`docs/INGRESS.md`, built: listeners on 25, 587 and 465 with per-port TLS and AUTH
policy, `STARTTLS` and implicit TLS on one certificate, and a sender ACL. The four
`SPEC.md` passages the spec settlements left alone until the capability existed
are amended, with §2.1, §4.1, §4.2 and §13 alongside them.

### What changed

| Module | § | What |
|---|---|---|
| `src/config/mod.rs` | 4.1, 5.1 | `server.listeners` with `IngressTls`/`IngressAuth` and per-port defaults; `server.tls`; `grants` on each user. `listen` and `auth.required` removed |
| `src/config/validate.rs` | 4.2 | Listener, certificate and grant rules; the inverted plaintext-AUTH rule; removed-key messages; two warnings (unused certificate, `optional` listener with users) |
| `src/smtp/tls.rs` | 5.1 | **New.** One `load` for §4.2 and the listener; permission errors that name the uid; name coverage via `rustls-webpki`; `notAfter` by hand; startup advisories |
| `src/smtp/acl.rs` | 5.3 | **New.** `grants.send_as` over §5.4's `Pattern`, default deny |
| `src/smtp/mod.rs` | 5.1 | One accept loop per listener sharing one `Shared`; implicit TLS on accept, inside the permit and the command budget; bare close on a TLS port |
| `src/smtp/session.rs` | 5.1–5.3 | Over `Stream`; `STARTTLS` with the injection check and the RFC 3207 reset; the `starttls_required` gate; `538`; the ACL at `MAIL FROM` and the dot; `close_notify` on every exit |
| `src/smtp/command.rs`, `reply.rs` | 5.2 | `STARTTLS`; `Capabilities`; six new replies, five of them `5xx` and each argued in the §14.1 audit test |
| `src/downstream/stream.rs` | 8.2 | `Stream::Tls` holds tokio-rustls's client-or-server enum, so ingress reuses it |
| `src/rewrite/mod.rs`, `src/relay.rs` | 6.1 | `Received:` carries RFC 3848's `S` |
| `src/metrics.rs` | 9.1 | `simmer_inbound_tls_failures_total`, `simmer_sender_not_permitted_total` |
| `src/hash_password.rs`, `src/main.rs` | 5.3 | **New** subcommand; per-listener startup lines; certificate expiry and coverage logged |
| `src/bin/loadgen.rs` | 12.3 | `--starttls`, verifying against `--ca` |
| `docker-compose.yml` | 12.3 | `tls-init` and the `acceptance-tls` volume |

830 tests, from 772: 509 unit (was 485), **20 inbound TLS (new)**, 65 config
validation (was 51), and every other suite unchanged in count — plus 5 acceptance
tests behind `--ignored` (was 4). The existing suites changed only in their
fixtures, mechanically: `listen:` became a one-entry `listeners:`, and
`auth: { required: false, … }` lost the key.

### What is tested

- **Every handshake is verified.** `tests/support::TestPki` mints a CA and a leaf
  per test, and the client trusts that CA alone. A second CA's client failing the
  handshake is what proves the configured certificate is the one served.
- **The injection check.** `STARTTLS` and a `MAIL FROM` in one write get no reply
  at all — not `220`, not anything.
- **The reset, both halves.** After the handshake the greeting and the
  authentication are gone (`503`, then `530`); the failed-password count is not (a
  third failure after two pre-TLS ones is `421`).
- **RFC 3207 §4's gate**, command by command, and `538` for plaintext AUTH — which
  does not spend a strike.
- **The ACL at both points**, the null sender, the missing `From:`, the
  unauthenticated exemption, and **two users producing byte-identical output**
  with no username in `Received:`.
- **One session bound across listeners.**
- **Certificate loading**: a mismatched pair named as such, both missing files
  reported together, swapped files diagnosed, an unreadable key naming the uid and
  mode; `notAfter` for both ASN.1 time forms, the 1950 pivot, and every truncation
  of a real certificate without a panic.
- **In the shipped image**: the grants refusing at both stages through the real
  container, the metric counting them, `hash-password`'s three exits, a verified
  `STARTTLS` submission on 587 arriving at a real mail server as `ESMTPSA`, and a
  real `SIGTERM` draining both listeners.

### What is not tested

- **A handshake that stalls.** Both TLS paths are bounded by `timeouts.command`;
  the failures driven are a wrong CA and plaintext on the implicit port.
- **The outbound half of real-certificate TLS**, as before. `tls-init` now mints
  the CA `docs/ACCEPTANCE.md` §5's trap-side design needs.
- **`advisories` from `main`.** The expiry and coverage warnings are unit-tested
  against fixed clocks; `main`'s logging of them was observed in the container
  (the expiry warning fired on the first run's seven-day certificate, which is why
  `tls-init` now mints thirty) but no test captures the log.

### Things the spec did not cover

D-070 through D-072. The ones a reader would not predict from `SPEC.md` or from
`docs/INGRESS.md`:

- **`close_notify`** — nothing said sessions must end with one, and dropping the
  stream made every clean TLS close look like truncation to the client. Found by
  the tests, not the design.
- **`530` vs `538`** — the design gave `538` to AUTH before a required handshake;
  RFC 3207 §4 already answers that with `530`, and `538` went to the case RFC 4954
  §6 defines it for.
- **No `454` after a failed handshake** — the plan said `454`, but once `220` has
  gone there is no channel left to send it on.
- **The unauthenticated exemption** — a legitimate reading of "auth optional", and
  the one most likely to surprise an operator, so it is a startup warning and a
  sentence in §2.3 rather than only a line here.

---

## Test programme step 5 summary — the soak tier (T4)

`docs/SOAK.md` is the long form. The tier was built from `28382f9` (step 5a) to
`7bcbb87` (V4) and run eight times on 2026-09-13 and 14: three 20-minute runs,
one hour killed at 52.7 minutes and salvaged, and three clean hours. It found and
fixed one defect (F16, D-080), drove two known ones over hours (F7, F2), and
**its last hour failed on a harness gate that cannot tell a bounded step from a
climb** — recorded, not waved through.

### What changed

| File | Step | What |
|---|---|---|
| `tests/soak.rs` | 5a, V2–V4 | **New.** `soak_run` drives two instances against one database and one sink and writes every sample to the host as it goes; `soak_analyze` judges the files as a separate test, so a killed run keeps its verdict. V2: `app` scraped, `app2` never. V3: fresh senders. V4: cancelled relays, its row sampler, the sweeper wait and `final-<instance>.prom`. `app` is scraped from inside its container |
| `tests/compose/findings.rs` | V4 | A non-panicking `assess`: every instance is judged and failures are raised once, at the end |
| `tests/compose/stack.rs` | 5a | The soak's additions to the shared compose helpers |
| `test/config/simmer.soak.yaml` | 5a, V3, V4 | **New.** V3 drops the `*.soak.test` sender rule so fresh senders fall through to the default chain; V4 adds `cancel.soak.test` → `warming-cancel`, with its own pool and quota row so V2 and V3 are untouched |
| `test/known-findings.json` | V4 | `soak/V4/accounting` and `soak/V4/registry` under F2; the S9 baseline note corrected about the registry |
| `src/bin/loadgen.rs` | V3 | One message in twenty from a fresh `u<n>.soak.test`, varying the envelope and `From:` both |
| `src/smtp/buffer.rs` | F16 | **D-080**: the spill file's writes batched at 64 KiB |
| `docs/SOAK.md` | — | **New.** Every run, what it established and what it did not |
| `tests/compose/leak.rs`, `tests/soak.rs`, `src/bin/sink.rs`, `test/config/simmer.soak.yaml` | 5c | `released_at_rest` and `unaccounted_fds`; `soak/rest/baseline`; the `X-Simmer-Correlation` link, recorded by the sink (`docs/SOAK.md` §9) |

`cargo test`: 869 passing across 29 binaries before `7bcbb87`, which changed
nothing after that run but `docs/SOAK.md`; 876 after step 5c's seven self-tests. The soak's own config test runs in it;
`soak_run` and `soak_analyze` are behind `--ignored` and need the stress stack.

### What is tested

- **Nothing lost, nothing doubled, over hours.** In each clean hour, 36,010 of
  36,010 accepted per instance and every sink record `delivered` and
  `mismatch:false`, joined by id: 0 duplicates, 0 answered `250` and never
  delivered. The killed hour's 67,443 agree.
- **No leak, twice.** Memory, descriptors and threads flat on both instances —
  scraped and unscraped — over §3a's and §3b's hours. For memory, that means no
  leak much above about 6 MiB/h: run A (`docs/SOAK.md` §10) showed that a one-hour
  run cannot see 2.2 MiB/h. A verdict needs eight
  five-minute floors after the warm-up, so a 20-minute run is `inconclusive` by
  design, and §4 records the one false `fds LEAKING` so it is not rediscovered.
- **Return to baseline**, checked by hand after every run: no sessions, nothing
  reserved, pools and the DB pool idle, 11 tasks, 3 threads.
- **F16, found and fixed.** Every 4 MiB message took 4.6–17.7 s: about 2.35 s per
  MiB past the §8.1 spill threshold, one blocking-pool job per line. After D-080,
  4 MiB runs at p50 36 ms, and the whole next hour's slowest message took 263 ms.
- **F7 driven** (XFAIL): unmatched-sender series grow by about 1,500 an hour with
  no ceiling, one per client-chosen domain.
- **F2 driven over hours** (V4, XFAIL): 11 cuts per instance an hour, each one
  stored by the sink and told `421`, so a client retry delivers it twice; the ledger
  short by exactly the cuts; the §10.4 registry keeping one entry per cut for the
  life of the process — a staircase, never a sawtooth. The database side is
  bounded: at most 4 rows, all swept. **Fixed since by D-081**, and V4 and stress
  S9 are now its regression checks.
- **The harness guards itself.** An unexpected pass fails the run, which forces a
  fixed finding out of `known-findings.json`; the F2 checks are judged only once
  `driven` passes, so "not reproduced" cannot read as "fixed"; a run in which `app`
  was never scraped fails.

### What is not tested

- **What adds the V4 hour's thread.** Both instances went from 5 to 6 threads once,
  kept it until the load stopped and were at 3 at rest; the gate failed `app2` on
  it. Step 5c settled the gate — a count back at its baseline at rest is a
  ratchet, not a leak, and the hour now passes (`docs/SOAK.md` §9) — but which
  blocking-pool job V4 adds is still not established.
- **What stalls the shared database.** Step 5c's hour traced its 9–16 s tail
  through the link. Simmer's downstream time for the slowest messages was 144 ms and
  1 ms. The time went to §7.4 statements waiting on Postgres: `COMMIT`s of 2–5 s and
  up to 15.4 s, on both instances at once, in three clusters (`docs/SOAK.md` §9).
  It is not the checkpointer. Contention from other workloads on the shared host is
  the suspicion, and it is not shown. The V4 hour's 538 ms tail, at the same moments
  on both instances, is probably the same thing and has not been re-checked.
- **Bursts, idle gaps (F10, CLOSE_WAIT) and the 24-hour run**, in `docs/SOAK.md`
  §7. The return to baseline as an assertion and the `correlation_id` ↔
  `X-Test-Id` link were built in step 5c, and a 20-minute run and a clean hour have
  passed them (§9).
- **A small memory leak.** The planted-defect controls have run (§10). Duplicate
  delivery is caught, and so are a leaked task and a leaked descriptor, each on
  exactly its own check. **A 2.2 MiB/h memory leak is not caught.** The one-hour
  gate's slope error, 3.2–3.8 MiB/h on real floors, is larger than the leak,
  because its self-test calibrated it against floors with no noise in them. What
  would restore the power is open: a longer judged run, an allocated-bytes gauge on
  `/metrics`, and a self-test with realistic noise.
- **`app2` while it runs.** Unscraped by design, so its registry is seen only in
  `final-app2.prom`.

### Things the spec did not cover

None of these amends `SPEC.md`; each is a question for its author.

- **§10.4's registry has no pruning path** but commit, release and shutdown. It was
  written for shutdown; under F2 it grows for the life of the process — about 280 a
  day per instance at V4's rate — and `simmer_reservations_in_flight` then reports
  reservations in flight when none are, which is the control plane misleading an
  operator (D-026, D-056).
- **§7.4's expiry warning names two causes**, a crash and an expiry shorter than
  the downstream's latency. F2 is a third, and the warning's text points an
  operator at neither of the real ones.
- **§9.1's `domain` label is client-controlled** (F7). Capping it diverges from
  §9.1.
- **F4, corroborated**: with no `statement_timeout`, §3a's 11-second database stall
  was simply 11 seconds of waiting under ordinary soak load.

Two traps worth carrying beyond this tier (`docs/SOAK.md` §6, §8): `stress-config`
reseeds the config volume from its *own* image, so a stale one restores an old
configuration and exits 0 — verify by the startup line's `"senders"`; and on this
host the containers' clock ran about 7% slow against the loadgens' monotonic one,
so run-seconds cannot be turned into log timestamps by addition.

