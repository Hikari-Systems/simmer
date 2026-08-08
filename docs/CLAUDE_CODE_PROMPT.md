# Claude Code kickoff prompt — Simmer

> Paste the text below into Claude Code in an empty directory, having first copied
> `SPEC.md` into that directory. Everything after the horizontal rule is the prompt.

---

We are building `simmer` from scratch in this directory. `SPEC.md` in the repository root is
the authoritative specification — read it in full before writing anything, and re-read the
relevant section before starting each phase. Where this prompt and `SPEC.md` disagree,
`SPEC.md` wins; tell me about the disagreement rather than silently picking one.

## What it is

A Rust SMTP relay facade, shipped as a `bookworm-slim` Docker container, that sits between an
application and one or more real SMTP providers. It applies a domain reputation warm-up ramp,
selects an outbound route according to quota state, rewrites the message's identity to match
that route, forwards it synchronously, and maps the downstream's reply back to the client on
the same connection.

Read §1 of `SPEC.md` carefully. The cutover invariant described there is the constraint that
shapes everything else: Simmer is temporary infrastructure, and its output must always be
exactly expressible as application-side configuration so that it can be removed in either
order relative to the application being reconfigured.

## Non-negotiable constraints

These are the ones most likely to be eroded by a plausible-looking shortcut. Do not.

1. **Simmer is not an MTA.** No spool, no queue, no retry scheduler, no DSN generation. If you
   find yourself designing message persistence, stop and ask. The only persistence is quota
   state and recipient-frequency events.
2. **Rewrites are absolute assignments and must be idempotent.** `rewrite(rewrite(m)) ==
   rewrite(m)` for every route. This is enforced by a property test and by startup validation.
3. **No DKIM signing and no key material anywhere in the container.** Strip
   `DKIM-Signature`, `Authentication-Results`, and `ARC-*` on the way through; the downstream
   signs.
4. **Quota increments on downstream `2xx` only**, via the reserve/send/commit protocol in
   §7.4. A post-hoc increment that permits overshoot defeats the purpose of the component.
5. **No failover between routes on downstream failure.** A route failure returns `451` to the
   client. Falling through would emit under the wrong identity and corrupt the ramp.
6. **Fail closed** when Postgres is unavailable.
7. **Body rewriting decodes before matching.** Raw-byte regex over a quoted-printable body is
   wrong in the common case, not the edge case.

## Environment and conventions

- Use the **hikari-systems Rust data service pattern** from my local skills for Postgres
  connection management, migrations, and query organisation. Read that skill before writing
  any database code. If anything in `SPEC.md` §11 conflicts with the pattern, follow the
  pattern and tell me what changed.
- Postgres runs as a sibling container. Provide a `docker-compose.yml` for local development
  and testing.
- Hand-roll the SMTP server state machine rather than adopting a server crate — §5.2 explains
  why. Do adopt a library for MIME parsing and building.
- **Before adding any mail-handling crate, check its licence for the exact version you pin.**
  Several popular Rust mail crates are AGPL or have changed licence between releases. Record
  what you find in `LICENSES.md`. If the only good option is AGPL, stop and ask me before
  taking the dependency.

## How I want you to work

Build in the phases listed in `SPEC.md` §13. Each phase must end in something that compiles,
passes its tests, and does something demonstrable.

**At the start of each phase**, write a short plan: what you're building, which spec sections
govern it, what you intend to test, and any decision the spec leaves open. Wait for me to
confirm before writing code. I would rather correct a plan than a pull request.

**Test-first for the logic-heavy parts** — sender matching and wildcard precedence, day-index
arithmetic, the reservation protocol, template rendering, the reply mapping table, and rewrite
idempotency. These are where the bugs will be, and they are all cheaply testable in isolation.
Integration tests need a scripted fake downstream that can return arbitrary codes, stall,
drop mid-`DATA`, and refuse TLS.

**At the end of each phase**, summarise what changed, what's tested, what isn't, and anything
you had to decide that the spec didn't cover. Add spec-divergences to a running
`DECISIONS.md`; do not silently amend `SPEC.md`.

## Things to raise rather than assume

If you hit any of these, stop and ask:

- The spec is silent or ambiguous on a behaviour you need.
- A constraint above appears to conflict with something else in the spec.
- An approach would require persisting a message beyond the lifetime of its client connection.
- A dependency's licence is restrictive.
- You believe one of the open decisions in `SPEC.md` §14 needs resolving to proceed.

Do not paper over an ambiguity with a plausible default and carry on. I would much rather
answer a question than discover an assumption three phases later.

## Start here

Read `SPEC.md` in full. Then read the hikari-systems data service pattern skill. Then give me:

1. Anything in the spec that is ambiguous, contradictory, or under-specified — I expect there
   to be some, and I would like them surfaced now rather than discovered during phase 6.
2. Your proposed crate list with licences.
3. Your proposed repository and module layout.
4. Your plan for phase 1.

Do not write any code until I have responded to that.
