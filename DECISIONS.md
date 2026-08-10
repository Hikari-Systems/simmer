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
build. That window is precisely when quota overshoot would occur.

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

### D-033 — Inbound listeners on 25/465/587, inbound TLS, and a sender ACL (planned)

**Spec:** this contradicts `SPEC.md` in four places rather than diverging from one.
§2.2 "**No inbound TLS**"; §5.1 "Plaintext TCP, default port 25. No STARTTLS, no
implicit TLS, no ACME"; §2.3 "The listener is plaintext and accepts plaintext
AUTH"; §5.2 "`EHLO` advertises **exactly** … Nothing else". And §4.2 currently
makes it a *violation* for `allow_insecure_auth` to be false, a rule that has to
invert.

**Decision:** designed in full in `docs/INGRESS.md`. Planned only — nothing is
built, and §1 of that document is a question for the spec's author before anything
is.

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

---

## Defects found, not yet fixed

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
| O-11 | §9.3 logs mutations "with the acting token's identifier", but `admin.auth_token` is a single scalar with no identity. | Either named admin tokens, or drop the wording. Currently one token, logged as `admin`. | Phase 7 |
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
