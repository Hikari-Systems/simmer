# Resume prompt — Simmer, phase 5

> Paste everything after the horizontal rule into a fresh Claude Code session
> opened in `/home/rickk/git/hs/simmer`.

---

We are building `simmer` in this directory — a Rust SMTP relay facade
(`bookworm-slim` container) that applies a domain reputation warm-up ramp between
an application and real SMTP providers. Phases 1–4 of ten are done. You are
picking up at phase 5.

## Read these first, in order

1. `docs/SPEC.md` — **authoritative**. Never amend it; record divergences in
   `DECISIONS.md`. §6.4 is phase 5's subject, and it is short — read §6.1, §6.6
   and §12.3 around it, because they are what constrain how it may be done.
2. `CLAUDE.md` — the three constraints that will bite you, and the key-file map.
3. `docs/STATE.md` — where the build has got to, what is tested, what is not.
4. `DECISIONS.md` — 42 decisions (D-001..D-042), 3 open questions (O-8, O-9,
   O-11) none of which phase 5 needs, and one **known defect** in phase 2's
   `smtp/auth.rs` still worth fixing on its own.
5. `docs/ACCEPTANCE.md` — the §12.3 acceptance suite, now **built**. Its §4.3
   has one row left unbuilt, and it is phase 5's: the body-rewrite assertion.

## Where we are

452 tests pass, plus 4 acceptance tests against a real stack.
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and
`cargo deny check` are clean. `docker compose up -d --build` comes up healthy.

Simmer accepts a message, authenticates the client, buffers the body, matches a
sender rule, resolves the recipient's domain group, walks the chain reserving
quota under a row lock, **rewrites the message to the selected route's identity**,
forwards to that route's downstream over TLS, and maps the verdict back —
committing quota only on a downstream `2xx`.

**The one thing §6 still does not do is rewrite bodies.** `body_rewrites` is
parsed and its regexes are compiled at startup (§4.2), and then ignored. The body
is carried through the engine as an opaque slice and arrives byte for byte.

Committed on `main` (remote `origin`): phase 1, then phases 2–3, then five commits
of planning and CI work, then phase 4. Working tree is clean, and **`main` is one
commit ahead of `origin/main`** — phase 4 is committed but not pushed. Do not
commit or push unless asked.

## Phase 5 — body rewriting

`SPEC.md` §13.5, and §6.4 is the whole specification:

> Scope is `text/*` parts only. Attachments and non-text parts are never touched.
> For each `text/*` part: decode according to `Content-Transfer-Encoding`
> (handling `quoted-printable` and `base64`), decode the charset to UTF-8, apply
> each `body_rewrites` entry in order as a regex replacement, re-encode, and fix
> up `Content-Transfer-Encoding` and any length-bearing headers.

§6.1 step 7 is where it goes: after `set_headers`, before the `Received:` header
is prepended. The seam is already there — `rewrite::rewrite` in `src/rewrite/mod.rs`
has steps 6 and 8 adjacent with a comment saying so.

### What will bite you

- **This is the step that ends D-039.** Phase 4 keeps the body as an opaque slice
  and every untouched header as its original bytes, which is what makes "nothing
  Simmer was not configured to change is changed" structurally true rather than
  tested. Body rewriting cannot preserve that structurally, so it has to become a
  *tested* property: a message with no matching `body_rewrites` must still come
  out byte-identical, including its MIME boundaries, its transfer encodings and
  its trailing whitespace. Write that test before the feature.
- **`tests/acceptance.rs::the_rewrite_is_what_a_real_mail_server_receives` will
  start failing on purpose.** It asserts the body link is *not yet* rewritten,
  with a message telling you to finish `ACCEPTANCE.md` §4.3's last row. That is
  the reminder working; replace the assertion with the real one.
- **§6.4 rejects raw-byte matching explicitly**, and gives the reason: a URL split
  across a quoted-printable soft line break (`https://old.=\r\nbrand.com/x`)
  would not match, and that is the common case rather than an edge case. Decode
  first, always.
- **Signed and encrypted parts are never rewritten** — `multipart/signed`,
  `multipart/encrypted`, `application/pkcs7-*`. Rewriting invalidates them.
- **A part that cannot be decoded is left untouched**, logged at `WARN`, and
  counted in `simmer_body_rewrite_skipped_total{route,reason}` — one of the §9.1
  counters, and the only one phase 5 owns.
- **§6.6's property must still hold.** `tests/rewrite_stability.rs` composes the
  whole rewrite with itself over generated messages; a regex whose replacement
  can match its own output (`s/a/aa/`) is unstable in exactly §6.6's sense, and
  the body is not covered by `unstable_headers`. Decide what happens there and
  record it — it is the phase's real open question.
- **Charset decoding needs a crate.** `mail-parser` decodes for reading, but
  re-encoding is not its job. Check the licence of anything new **for the exact
  version pinned, across every published version**, and record it in
  `LICENSES.md` before adopting it. If the only good option is AGPL, stop and ask.

## Non-negotiables

- **Not an MTA**: no spool, queue, retry scheduler or DSN. Only quota state and
  recipient-frequency events persist. The `DATA` buffer is tmpfs and not durable.
- **Cutover invariant (§1.1)**: rewrites are absolute assignments, never relative,
  and stable under repetition. Identity fields are not overridable; other headers
  only via `unstable_headers` (§6.6, D-001). Enforced at startup since phase 4 —
  and it already caught a violation in `SPEC.md`'s own §4.1 example (D-036).
- **Never emit a reply that makes a client record permanent state** (§14.1).
  `src/smtp/reply.rs` holds every reply Simmer can emit and has a test that fails
  when an undocumented `5xx` is added. Keep it that way.
- **No DKIM signing, no key material.** `DKIM-Signature`, `Authentication-Results`
  and `ARC-*` are stripped unconditionally (§6.5) — and body rewriting is one of
  the two reasons that has to happen.
- Quota increments on downstream `2xx` only, via §7.4. No failover between routes
  (§3.3). Fail closed when Postgres is unavailable (§7.5).
- **Building the shipped image needs `--target runtime`.** The Dockerfile's last
  stage is the acceptance loadgen.

## How to work

Build in the phases of `SPEC.md` §13. **Write a short plan at the start of the
phase and wait for confirmation before writing code.** Test-first for the
logic-heavy parts — transfer-encoding round trips and the "nothing matched, so
nothing changed" property are exactly that. At the end, summarise what changed,
what is tested, what is not, and anything the spec did not cover, appending to
`DECISIONS.md`. Raise ambiguities rather than defaulting past them.

Storage follows the hikari-systems data-service pattern (`hs-rust-data-service`
skill): runtime sqlx, never `query_as!`; `models/<entity>.rs` free functions over
`&PgPool`; plain-SQL idempotent migrations applied at startup. Simmer diverges on
config, HTTP and logging — see D-003..D-006.

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

And the acceptance tier, which phase 5 changes and which is not in `cargo test`:

```sh
docker compose --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

`DATABASE_URL` is needed only by `tests/quota*.rs` (D-031). It does not affect the
Docker build.

## Outstanding non-code items

- **`SPEC.md` §4.1's example configuration fails `SPEC.md` §4.2** (D-036). The
  spec author's call; `simmer.yaml` was corrected and `SPEC.md` was left alone.
- The `smtp/auth.rs` timing defect, still unfixed and still separable.
- Decide whether to keep the cargo-deny licence gate (analysis in `LICENSES.md`).
- `hs-utils` — and every hikari-systems Rust service — declares no `license`
  field. One line upstream fixes it estate-wide.
- The five older data services call `prepare_config` before `apply_env_overrides`,
  which is the wrong order: a `[SECRET]:` supplied via env is never resolved.
