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
fixed `127.0.0.1:5433`.

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
| O-8 | §5.6's collapse table returns `550` when *any* split failed permanently, which records permanent state about recipients that did not fail — the §14.1 problem again. | `550` only when *all* failures are permanent; `451` otherwise. | Phase 9 |
| O-9 | §5.6 splits by route; §6.3 implies per-recipient splitting when a template references `recipient.*`. | If a selected route's templates reference `recipient.*`, split that route's recipients one per transaction; otherwise group by route. | Phase 9 |
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
