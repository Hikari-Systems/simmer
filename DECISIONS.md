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

## Still open — to settle at the start of the phase that needs them

Raised during planning, defaulted as described, and worth an explicit call before
the phase that depends on each.

| # | Question | Working assumption | Needed by |
|---|---|---|---|
| O-1 | §6.1 orders reservation after `DATA`; §5.4 says decide at `RCPT TO` when all rules are envelope-only. A reservation held across the whole `DATA` transfer can outlive §7.4's expiry, which does not budget for body transfer. | Split eligibility evaluation from reservation: evaluate (and reject) early, reserve immediately before the downstream conversation. | Phase 3 |
| O-2 | Do overflow routes participate in quota accounting? §3.1 "never quota-limited" vs §1.1 "still counts against quota". | Run the full reserve/commit protocol with a sentinel "unlimited" allowance, so metrics are uniform and the code path has no special case. | Phase 3 |
| O-3 | §9.3's allowance override is per domain group; §11's `route_state` is keyed on route alone and cannot represent it. "Expires at the next day boundary" is also undefined for an overflow route, which has no `warmup` and so no boundary. | A `route_group_override(route, domain_group, allowance, expires_at)` table; expiry from the route's own day boundary, 24h for overflow routes. | Phase 3 |
| O-4 | Is `quota_usage.allowance` authoritative or a cache of the config schedule? | Written once when the row is created for a `(route, group, day_index)` and authoritative thereafter; the admin override is the only mid-day change. | Phase 3 |
| O-5 | §3.2 step 3d's "re-evaluate once on a concurrent claim" has no meaning under §7.4's row lock, which serialises contenders. | Drop the retry; treat a lock-wait timeout as a database failure per §7.5. | Phase 3 |
| O-6 | §3.2 step 1 says an unmatched sender goes "directly to the overflow route" of the default chain, but §4.2 permits that chain to contain warming routes first. | Walk it normally, like any other chain. | Phase 3 |
| O-7 | §3.2: "decremented per message, by the recipient count, not per recipient" reads two ways. | One reservation of magnitude `recipient_count`. Moot while `single_recipient_only` defaults true. | Phase 3 |
| O-8 | §5.6's collapse table returns `550` when *any* split failed permanently, which records permanent state about recipients that did not fail — the §14.1 problem again. | `550` only when *all* failures are permanent; `451` otherwise. | Phase 9 |
| O-9 | §5.6 splits by route; §6.3 implies per-recipient splitting when a template references `recipient.*`. | If a selected route's templates reference `recipient.*`, split that route's recipients one per transaction; otherwise group by route. | Phase 9 |
| O-10 | §5.2 advertises `SMTPUTF8`, but a downstream may not support it. | Reject with `550 5.6.7` at `MAIL FROM` if a UTF-8 address is presented and the selected route's downstream does not advertise it, rather than discovering it mid-relay. | Phase 2 |
| O-11 | §9.3 logs mutations "with the acting token's identifier", but `admin.auth_token` is a single scalar with no identity. | Either named admin tokens, or drop the wording. Currently one token, logged as `admin`. | Phase 7 |
| O-12 | §12.3 asks for day-index tests "across DST boundaries and leap seconds". Unix time cannot represent a leap second, so §7.2's elapsed-duration arithmetic is unaffected. | Test DST transitions and a future `warmup.started` (negative day index → route ineligible per §7.2). The leap-second case reduces to a no-op. | Phase 3 |
