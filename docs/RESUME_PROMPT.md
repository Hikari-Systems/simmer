# Resume prompt — Simmer, phase 4

> Paste everything after the horizontal rule into a fresh Claude Code session
> opened in `/home/rickk/git/hs/simmer`.

---

We are building `simmer` in this directory — a Rust SMTP relay facade
(`bookworm-slim` container) that applies a domain reputation warm-up ramp between
an application and real SMTP providers. Phases 1–3 of ten are done. You are
picking up at phase 4.

## Read these first, in order

1. `docs/SPEC.md` — **authoritative**. Never amend it; record divergences in
   `DECISIONS.md`. §6 is phase 4's subject; read §1.1 and §6.6 twice.
2. `CLAUDE.md` — the three constraints that will bite you, and the key-file map.
3. `docs/STATE.md` — where the build has got to, what is tested, what is not.
4. `DECISIONS.md` — 33 decisions (D-001..D-033), 3 open questions (O-8, O-9,
   O-11) none of which phase 4 needs, and one **known defect** in phase 2's
   `smtp/auth.rs` worth fixing before anything else touches authentication.
5. `docs/ACCEPTANCE.md` — the §12.3 acceptance harness, **designed but not
   built**. Read §4.4 before writing the rewrite engine: the cutover invariant is
   the property phase 4 exists to make true, and that section is the only test
   that will ever state it.
6. `docs/CLAUDE_CODE_PROMPT.md` — the original kickoff and working agreement.

## Where we are

323 tests pass. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`
and `cargo deny check` are clean. `docker compose up -d --build` comes up healthy
and relays a real message end to end.

Simmer today accepts a message, authenticates the client, buffers the body,
matches a sender rule, resolves the recipient's domain group, walks the chain
reserving quota under a row lock, forwards to the selected route's downstream over
TLS, and maps the verdict back — committing quota only on a downstream `2xx`.

**It does not rewrite anything.** The body is forwarded byte for byte under the
identity it arrived with. `identity.set_headers`, `envelope_from`,
`remove_headers` and `body_rewrites` are parsed and validated at startup, then
ignored. That is what phase 4 fixes, and until it lands the component does not
actually do its job.

Committed on `main`, no remote: one commit for phase 1, one for phases 2–3 (they
could not be split — see `docs/STATE.md` §8). Working tree clean. Do not commit or
push unless asked.

## Phase 4 — the rewriting engine

`SPEC.md` §13.4: "Rewriting engine: templates, header set/remove, authentication
artefact stripping, idempotency property test." Body rewriting (§6.4) is phase 5
and stays out.

The governing sections are §6.1 (order of operations), §6.2 (rewritable fields),
§6.3 (templating), §6.5 (auth artefacts) and §6.6 (stability). §6.1's numbered
list is the shape of the work:

```
2. Parse into headers and MIME structure
4. Strip authentication artefacts (§6.5)
5. Apply remove_headers
6. Apply set_headers, rendering templates
8. Prepend a Received: header naming Simmer
9. Compute the outbound envelope sender
10. Serialise and transmit
```

Where it goes: `src/rewrite/` (the directory exists and is empty), called from
`relay::reserve_relay_commit` between the reservation and
`downstream::relay`. `mail-parser` is already a dependency (D-022), used so far
only to pull the first `From:` address out of the header block.

### What will bite you

- **§6.6 is already enforced at startup** by `config::validate` against a
  synthetic probe. Phase 4 makes that check real — the same rewrite function must
  now drive both, or the startup validation is testing something the relay does
  not do. Check what `validate.rs` currently does before writing a second
  implementation.
- **The §12.3 byte-equivalence assertion** (D-002) excludes `unstable_headers`,
  the `Received:` header and `X-Simmer-*`. It cannot pass otherwise.
- **`rewrite(rewrite(m)) == rewrite(m)`** is a property test over generated
  messages, not just a startup probe. Identity fields (`envelope_from`, `From:`,
  `Sender:`, `Message-ID:`) have no override; other headers need
  `unstable_headers`.
- **Header order is part of byte-equivalence.** `OrderedHeaders` exists in
  `config/mod.rs` for exactly this reason.
- **`tests/relay_mapping.rs::phase_2_forwards_the_body_byte_for_byte`** is the
  baseline phase 4 must consciously break and replace. Its awkward body (a bare
  dot line, a dot-prefixed line, a trailing blank line) should survive rewriting
  unchanged in the parts nothing touches.

## The acceptance harness

`docs/ACCEPTANCE.md` designs a compose profile with two Mailpit traps, a bulk
sender container, and a ramp walked by moving `warmup.started` and restarting
(D-032). It is planned, not built.

Its ramp-walk half is buildable today and does not need phase 4. Its rewrite and
cutover-invariant assertions need phase 4 and are the strongest available check
that the rewriting engine is right — §1.1's whole thesis is that both arrangements
produce byte-equivalent output, and nothing else tests it.

`docs/INGRESS.md` separately designs listeners on 25/465/587, inbound TLS and a
sender ACL (D-033) as a new phase 11. It is planned, not built, and **its §1 is a
question for the spec's author**: it reverses four `SPEC.md` passages rather than
diverging from one. Do not start it without that answer.

**Worth agreeing with the user up front** whether to build the harness before,
alongside, or after the rewrite engine. Building it first means phase 4 has
somewhere to plug its assertions in; building it after risks discovering a
rewriting fault several phases late.

## Non-negotiables

- **Not an MTA**: no spool, queue, retry scheduler or DSN. Only quota state and
  recipient-frequency events persist. The `DATA` buffer is tmpfs and not durable.
- **Cutover invariant (§1.1)**: rewrites are absolute assignments, never relative,
  and stable under repetition. Identity fields are not overridable; other headers
  only via `unstable_headers` (§6.6, D-001).
- **Never emit a reply that makes a client record permanent state** (§14.1).
  `src/smtp/reply.rs` holds every reply Simmer can emit and has a test that fails
  when an undocumented `5xx` is added. Keep it that way.
- **No DKIM signing, no key material.** Strip `DKIM-Signature`,
  `Authentication-Results` and `ARC-*` unconditionally (§6.5).
- Quota increments on downstream `2xx` only, via §7.4. No failover between routes
  (§3.3). Fail closed when Postgres is unavailable (§7.5).

## How to work

Build in the phases of `SPEC.md` §13. **Write a short plan at the start of the
phase and wait for confirmation before writing code.** Test-first for the
logic-heavy parts — template rendering and the idempotency property are exactly
that. At the end, summarise what changed, what is tested, what is not, and
anything the spec did not cover, appending to `DECISIONS.md`. Raise ambiguities
rather than defaulting past them.

Storage follows the hikari-systems data-service pattern (`hs-rust-data-service`
skill): runtime sqlx, never `query_as!`; `models/<entity>.rs` free functions over
`&PgPool`; plain-SQL idempotent migrations applied at startup. Simmer diverges on
config, HTTP and logging — see D-003..D-006.

**Check any new crate's licence for the exact version pinned, across every
published version, and record it in `LICENSES.md` before adopting it.** Several
Rust mail crates have changed licence between releases. If the only good option is
AGPL, stop and ask.

## Gate before pushing

```sh
docker compose up -d simmer-db
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
docker compose up -d --build      # not optional
```

`DATABASE_URL` is needed only by `tests/quota*.rs` (D-031). It does not affect the
Docker build.

## Outstanding non-code items

- Decide whether to keep the cargo-deny licence gate (analysis in `LICENSES.md`).
- `hs-utils` — and every hikari-systems Rust service — declares no `license`
  field. One line upstream fixes it estate-wide.
- The five older data services call `prepare_config` before `apply_env_overrides`,
  which is the wrong order: a `[SECRET]:` supplied via env is never resolved.
