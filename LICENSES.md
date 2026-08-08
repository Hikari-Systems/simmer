# Dependency licences

The kickoff prompt requires that every mail-handling crate's licence be checked
**for the exact version pinned**, because several well-known Rust mail crates are
AGPL-licensed or have changed licence between releases, and that the finding be
recorded here.

Two checks are recorded below: the mail crates specifically (the risk the prompt
names), and the whole resolved graph (what actually ships).

## How much does copyleft actually matter here?

Less than the amount of ceremony in this file implies, and it is worth being
straight about that.

Simmer is an internal container on a trusted network segment (§2.3), not software
we ship to anyone. **GPL obligations attach to distribution**, and running a
binary on your own infrastructure is not distribution — so a GPL dependency would
create no source-disclosure obligation in this deployment. **AGPL** is the one
that could plausibly bite, because its §13 extends the trigger to users
interacting with the software over a network; but Simmer's only clients are our
own applications, which is a long way from the public-SaaS case AGPL was written
for.

And the specific scare turns out to be misdirected. The AGPL association in the
Rust mail ecosystem is with `stalwart-mail`, the mail *server*. The library crates
Simmer would actually want — `mail-parser`, `mail-builder`, `mail-send` — have
been `Apache-2.0 OR MIT` for every version ever published (§1 below).

So the check found nothing, and would not have mattered much if it had. It is
kept because:

- the prompt asked for it explicitly, including "stop and ask me before taking
  the dependency" if AGPL were the only option;
- it costs nothing — no crate we want is copyleft, so the policy constrains
  nothing real;
- it is cheap insurance against this code or its approach later ending up
  somewhere that *is* distributed, where the analysis changes.

The genuinely useful thing the check surfaced was not copyleft at all: it was
that `hs-utils`, and by extension every hikari-systems Rust service, declares no
`license` field (§2 below).

**If you would rather not carry a licence gate in CI, say so** — deleting the
`cargo deny check` step and `deny.toml` costs nothing and loses nothing that the
above analysis says we need. The advisories check (known vulnerabilities) is
worth keeping either way, and is a separate subcommand.

Verified 2026-08-07 against the crates.io API and `cargo metadata` on the
committed `Cargo.lock`. Re-run the commands in "How to re-check" after any
dependency change.

---

## 1. The mail crates — the flagged risk

**Checked, and the warning does not land on the crates Simmer wants.** Every
published version of the Stalwart library crates is `Apache-2.0 OR MIT`, not just
the current one:

| Crate | Latest | Licence | Versions checked |
|---|---|---|---|
| `mail-parser` | 0.11.5 | `Apache-2.0 OR MIT` | all 36 published, 0.1.0 → 0.11.5 |
| `mail-builder` | 0.4.4 | `Apache-2.0 OR MIT` | all 18 published, 0.1.0 → 0.4.4 |
| `mail-send` | 0.6.1 | `Apache-2.0 OR MIT` | all 24 published, 0.1.0 → 0.6.1 |
| `mailparse` | 0.16.1 | `0BSD` | all published |
| `lettre` | 0.11.23 | `MIT` | all published |

The AGPL association in this ecosystem is with **`stalwart-mail`, the mail
*server*** — a separate crate that Simmer does not and will not depend on. The
library crates above have never been AGPL. There is therefore no AGPL dependency
to escalate, and no need to invoke the prompt's "stop and ask before taking the
dependency" clause.

**Planned selection (phase 4/5):** `mail-parser` for parsing and `mail-builder`
for building — a matched pair from one author, with the RFC 2047 / RFC 2231 /
quoted-printable / base64 / charset decoding that §6.4 requires, and zero-copy so
the buffered body is not duplicated. `mailparse` (`0BSD`) is the fallback if
`mail-parser` proves awkward for header-order-preserving rewrite.

**Not adopted:** `mail-send` and `lettre`. Not for licence reasons — both are
permissive — but because §8.2's four TLS modes, §8.4's per-stage timeouts, §8.3's
pool semantics and §10.2's final-dot ambiguity all need control a client library
abstracts away. §10.2 in particular requires distinguishing "no reply was read"
from "an error occurred", which client crates normalise into one error type. The
outbound SMTP client is hand-rolled alongside the server state machine.

None of these are dependencies yet; they arrive with the rewriting engine
(phase 4) and the outbound leg (phase 2). Recorded now because the check was
required before adopting them, not after.

---

## 2. The whole resolved graph

241 third-party packages. **No copyleft licence appears anywhere** — no GPL, no
AGPL, no LGPL, no MPL, no SSPL, no EPL, no CDDL.

| Count | Licence |
|---|---|
| 135 | `MIT OR Apache-2.0` |
| 38 | `MIT` |
| 19 | `Apache-2.0 OR MIT` |
| 18 | `Unicode-3.0` |
| 7 | `MIT/Apache-2.0` (deprecated SPDX slash syntax) |
| 3 | `Unlicense OR MIT` |
| 2 | `ISC` |
| 2 | `Apache-2.0 OR BSL-1.0 OR MIT` |
| 2 | `CDLA-Permissive-2.0` |
| 2 | `BSD-2-Clause OR Apache-2.0 OR MIT` |
| 1 each | `Apache-2.0`, `Apache-2.0/MIT`, `Apache-2.0 AND ISC`, `Apache-2.0 OR ISC OR MIT`, `Apache-2.0 OR BSL-1.0`, `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`, `BSD-3-Clause`, `MIT AND BSD-3-Clause`, `MIT OR Apache-2.0 OR Zlib`, `Zlib`, `Zlib OR Apache-2.0 OR MIT`, `(MIT OR Apache-2.0) AND Unicode-3.0` |
| 1 | **none declared** — see below |

### The finding: two crates declare no licence, both in-house

`cargo deny check licenses` initially failed on two, and both are ours:

- **`simmer` itself.** Fixed by `publish = false` in `Cargo.toml`, which is
  accurate — this is a service, not a library, and it never goes to crates.io.
  (Amusingly, `cargo-deny` had also picked up *this file* as a candidate licence
  text and scored it 0.03. `publish = false` settles that too.)
- **`hs-utils 0.31.2`**, which has no `license` field. It is the in-house shared
  crate consumed by git tag from a private repository, so this is a metadata gap
  rather than a licensing risk — but it means an automated check cannot
  distinguish "in-house, fine" from "unlicensed, not fine", and **every
  hikari-systems Rust service has the same hole**.

Handled in `deny.toml` by `[licenses.private]` with `ignore-sources` scoped to
the one `hs-utils-rs` git URL, rather than by switching the unlicensed check off.
A third-party crate with no licence still fails.

Worth fixing upstream by adding a `license` field to `hs-utils-rs`. That is a
one-line change that would benefit every service in the estate, and would let the
`ignore-sources` line here be deleted.

---

## 3. Direct dependencies, as resolved

| Crate | Version | Licence |
|---|---|---|
| `anyhow` | 1.0.104 | `MIT OR Apache-2.0` |
| `axum` | 0.8.9 | `MIT` |
| `chrono` | 0.4.45 | `MIT OR Apache-2.0` |
| `hs-utils` | 0.31.2 | *(none declared — see above)* |
| `ipnet` | 2.12.1 | `MIT OR Apache-2.0` |
| `regex` | 1.13.1 | `MIT OR Apache-2.0` |
| `serde` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_json` | 1.0.151 | `MIT OR Apache-2.0` |
| `serde_yaml_ng` | 0.10.0 | `MIT` |
| `sqlx` | 0.8.6 | `MIT OR Apache-2.0` |
| `thiserror` | 2.0.19 | `MIT OR Apache-2.0` |
| `tokio` | 1.53.1 | `MIT` |
| `tracing` | 0.1.44 | `MIT` |
| `tracing-subscriber` | 0.3.23 | `MIT` |

Note `serde_yaml_ng` rather than `serde_yaml`: §12.1 names the latter, but it is
unmaintained and published as `0.9.34+deprecated`. `serde_yaml_ng` is the
maintained continuation and carries the same `MIT` terms.

---

## How to re-check

`cargo-deny` is the gate in CI:

```sh
cargo install cargo-deny      # once
cargo deny check licenses
cargo deny check bans sources advisories
```

The policy lives in `deny.toml`: permissive licences are allow-listed, anything
outside the list fails, and `hs-utils`'s git source is the one allow-listed
non-registry origin.

For the per-version history check that this file's section 1 records — the thing
`cargo-deny` cannot do, because it only sees the version you resolved — query
crates.io directly:

```sh
curl -s "https://crates.io/api/v1/crates/<name>/versions?per_page=100" \
  | jq -r '.versions[] | "\(.num)\t\(.license)"'
```

Do this before adopting any new mail-handling crate, not after.
