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

## 4. Phase 2 additions

Checked **across every published version**, not just the pinned one, before
adoption. Verified 2026-08-07 against the crates.io API.

| Crate | Pinned | Licence | Versions checked | For |
|---|---|---|---|---|
| `mail-parser` | 0.11.5 | `Apache-2.0 OR MIT` | all 36 | `From:` extraction (D-022) |
| `argon2` | 0.5.3 | `MIT OR Apache-2.0` | all | §5.3 |
| `base64` | 0.22.1 | `MIT OR Apache-2.0` | all | AUTH PLAIN/LOGIN payloads |
| `tempfile` | 3.27.0 | `MIT OR Apache-2.0` | all with metadata | §8.1 spill |
| `uuid` | 1.24.0 | `Apache-2.0 OR MIT` | all with metadata | §9.5 correlation_id |
| `rustls` | 0.23.43 | `Apache-2.0 OR ISC OR MIT` | all | §8.2 |
| `tokio-rustls` | 0.26.4 | `MIT OR Apache-2.0` | all | §8.2 |
| `rustls-native-certs` | 0.8.4 | `Apache-2.0 OR ISC OR MIT` | all | §8.2 platform root store |
| `metrics` | 0.24.6 | `MIT` | all | §9.1 (D-021) |

No copyleft, so §1's "stop and ask before taking the dependency" clause was not
reached. The oldest `tempfile` and `uuid` releases predate crates.io licence
metadata and report none; every version this century declares a permissive
licence, and the pinned ones are in the table.

`mail-parser` is the one the prompt's warning was aimed at, and §1 already
recorded the finding: the AGPL association in this ecosystem is with
`stalwart-mail`, the mail *server*, which Simmer does not depend on.

**`rustls` uses the `ring` provider, not the default `aws-lc-rs`.** Not a licence
decision: `aws-lc-rs` needs cmake and a C toolchain in the builder image, and
sqlx already pulls rustls with `ring` through `runtime-tokio-rustls`, so this
keeps one crypto provider in the graph instead of two.

Still not adopted: `mail-send` and `lettre`, for the reasons in §1 — §10.2's
final-dot ambiguity requires distinguishing "no reply was read" from "an error
occurred", which client crates normalise into one error type.

### Phase 3

| Crate | Pinned | Licence | Versions checked | For |
|---|---|---|---|---|
| `async-trait` | 0.1.91 | `MIT OR Apache-2.0` | all | §11's storage trait, held as `Arc<dyn QuotaStore>` |

Taken because §11 asks the storage layer to sit behind a trait and `async fn` in
a trait is not yet dyn-compatible. Droppable the moment it is.

### Phase 4

A **dev-dependency only** — it is not linked into the shipped binary. §6.6 asks
for the stability property to be "enforced by a property test over generated
messages", and that is what this is for.

| Crate | Pinned | Licence | Versions checked | For |
|---|---|---|---|---|
| `proptest` | 1.11.0 | `MIT OR Apache-2.0` | all 48 published, 0.1.0 → 1.11.0 | §6.6 `rewrite(rewrite(m)) == rewrite(m)` |

The 38 versions before 1.3.0 declare `MIT/Apache-2.0` — the deprecated SPDX slash
syntax for the same pair, not a different licence. `proptest-derive` is a separate
crate and is **not** taken; the generators here are hand-written strategies over
message parts, not derived from types.

Adopted with `default-features = false, features = ["std", "bit-set"]`. The
`fork` and `timeout` default features spawn a subprocess per test case to survive
a panicking or hanging property, which this property cannot do — it is a pure
function over bytes — and turning them off keeps `rusty-fork`, `wait-timeout` and
`quick-error` out of the graph entirely.

What it did add, all permissive and all dev-only:

| Crate | Version | Licence |
|---|---|---|
| `bit-set` | 0.8.0 | `Apache-2.0 OR MIT` |
| `bit-vec` | 0.8.0 | `Apache-2.0 OR MIT` |
| `getrandom` | 0.2.17 | `MIT OR Apache-2.0` |
| `rand` | 0.8.7 | `MIT OR Apache-2.0` |
| `rand_chacha` | 0.3.1 | `MIT OR Apache-2.0` |
| `rand_core` | 0.6.4 | `MIT OR Apache-2.0` |
| `rand_xorshift` | 0.4.0 | `MIT OR Apache-2.0` |
| `unarray` | 0.1.4 | `MIT OR Apache-2.0` |
| `r-efi` | 5.3.0 | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` |
| `wasip2` | 1.0.4 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| `wit-bindgen` | 0.57.1 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |

`r-efi` is the only entry with a copyleft option in its disjunction, and it is
never built here: it is `getrandom`'s UEFI-target backend, pulled into the lock
file by target resolution and compiled on no platform Simmer runs on. The
disjunction offers `MIT` regardless, which is the branch cargo-deny accepts.

`cargo deny check licenses` passes unchanged.

### Phase 5

**No new dependencies.** §6.4 needs quoted-printable and base64 codecs and a
charset layer, and the phase was expected to take at least one crate for the
last of those. It did not, and the reason is worth recording because it was a
licence-adjacent decision even though nothing was adopted.

The obvious candidate was `encoding_rs`, reachable without a new *direct*
dependency by turning on `mail-parser`'s `full_encoding` feature. Its licence is
clean — `(Apache-2.0 OR MIT) AND BSD-3-Clause`, all three permissive — so this
was not a licence rejection. It was rejected on behaviour: its encoder
substitutes numeric character references for characters the target charset cannot
represent, which is correct for HTML form submission and wrong in a mail body,
where it would emit a literal `&#8212;` in place of an em dash. §6.4 already
specifies what to do with a charset we cannot handle — leave the part alone, warn,
count it — so the narrower hand-written codec (D-044) is both closer to the spec
and one fewer crate.

`base64` and `regex` were already in the graph, from phase 2 and phase 1
respectively.

### Phase 6

Two new **direct** dependencies that add nothing to the build, because both were
already being compiled as transitive dependencies of `sqlx-postgres` (SCRAM
authentication needs exactly this pair). Taking them directly pins nothing new
and downloads nothing new; it only makes the use in `src/frequency/mod.rs`
explicit. Verified 2026-08-10 against the crates.io API.

| Crate | Pinned | Licence | Versions checked | For |
|---|---|---|---|---|
| `sha2` | 0.10.9 | `MIT OR Apache-2.0` | all | §7.3's salted hash |
| `hmac` | 0.12.1 | `MIT OR Apache-2.0` | all | §7.3's salted hash |

Both are RustCrypto crates, the same family `argon2` already comes from.

**Why not reuse `argon2`, which is already a direct dependency.** §5.3 wants a
password hash to be *slow*; §7.3's key is computed on the message path for every
recipient of every message. The threat models differ as well: the salt and the
hashes live in the same database, so slowness buys nothing against an attacker
who has the table. What §7.3 asks for is that the container stop *accumulating*
plaintext, which a keyed hash does.

**Randomness for the salt did not need a crate.** `rand` is in the resolved graph
transitively but is not a direct dependency, and adding it for 32 bytes would be
a fourth way to reach the operating system's randomness. `uuid` — already direct,
for §9.5's correlation id — carries 122 bits of `getrandom` output per v4 value,
so two of them make the salt.

---

## 3. Direct dependencies, as resolved

| Crate | Version | Licence |
|---|---|---|
| `anyhow` | 1.0.104 | `MIT OR Apache-2.0` |
| `axum` | 0.8.9 | `MIT` |
| `chrono` | 0.4.45 | `MIT OR Apache-2.0` |
| `hmac` | 0.12.1 | `MIT OR Apache-2.0` |
| `hs-utils` | 0.31.2 | *(none declared — see above)* |
| `ipnet` | 2.12.1 | `MIT OR Apache-2.0` |
| `regex` | 1.13.1 | `MIT OR Apache-2.0` |
| `serde` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_json` | 1.0.151 | `MIT OR Apache-2.0` |
| `serde_yaml_ng` | 0.10.0 | `MIT` |
| `sha2` | 0.10.9 | `MIT OR Apache-2.0` |
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
