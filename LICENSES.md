# Dependency licences

The kickoff prompt requires that every mail-handling crate's licence be checked
**for the exact version pinned**, because several well-known Rust mail crates are
AGPL-licensed or have changed licence between releases, and that the finding be
recorded here.

Two checks are recorded below: the mail crates specifically (the risk the prompt
names), and the whole resolved graph (what actually ships).

## Settled: the gate stays, and it is Apache-compatible-only

**Decided 2026-08-11 by the repository's owner (D-062).** This file previously
left "whether to keep the cargo-deny licence gate" as an open question. It is
closed: the gate stays, and the bar is that **nothing incompatible with Apache-2.0
may enter the graph**.

`deny.toml`'s `[licenses] allow` list is what enforces it, and it is an
allow-list, not a deny-list — a licence that is not named cannot resolve, so
GPL, AGPL, LGPL, MPL and anything else copyleft fails **by omission** rather than
by anyone remembering to add it. Every entry on the list is permissive and
Apache-2.0-compatible: `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`,
`BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `0BSD`, `Zlib`, `Unicode-3.0`,
`CDLA-Permissive-2.0`, `BSL-1.0`, `Unlicense`. CI runs `cargo deny check` on every
push to every branch, so this is a build failure rather than a discovery.

**One thing that will look like a violation and is not.** `cargo deny list`
reports:

```
LGPL-2.1-or-later (2): r-efi@5.3.0, r-efi@6.0.0
```

`r-efi` is licensed `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. It is an **`OR`**:
we take MIT or Apache-2.0, and the LGPL option is never exercised. `cargo deny
list` prints every licence *named in an expression*, including the branches not
taken, which is why it appears; `cargo deny check licenses` resolves the
expression against the allow list and passes. Verified 2026-08-11 against
crates.io for both pinned versions. No crate in the graph imposes a copyleft
obligation.

The reasoning below predates that decision and is kept because it explains *why*
the risk was judged low. It is no longer the operative policy — the allow list is.

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
`license` field (§2 below). Simmer no longer depends on it (D-060), so that is
now an upstream note rather than a finding about this crate.

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

Re-counted 2026-08-11, after phase 7's additions and D-060's removal:
**293 third-party packages** including dev-dependencies, up from 241 at phase 1.
**No copyleft licence is imposed on anything here.**

| Count | Licence |
|---|---|
| 161 | `MIT OR Apache-2.0` |
| 44 | `MIT` |
| 25 | `Apache-2.0 OR MIT` |
| 18 | `Unicode-3.0` |
| 12 | `MIT/Apache-2.0` (deprecated SPDX slash syntax) |
| 5 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` |
| 3 | `Unlicense OR MIT` |
| 2 each | `ISC`, `Apache-2.0`, `Zlib`, `CDLA-Permissive-2.0`, `Apache-2.0 OR ISC OR MIT`, `Apache-2.0 OR BSL-1.0 OR MIT`, `BSD-2-Clause OR Apache-2.0 OR MIT`, `MIT OR Apache-2.0 OR LGPL-2.1-or-later` |
| 1 each | `Apache-2.0/MIT`, `Apache-2.0 AND ISC`, `Apache-2.0 OR BSL-1.0`, `BSD-3-Clause`, `MIT AND BSD-3-Clause`, `MIT AND Apache-2.0`, `MIT OR Apache-2.0 OR Zlib`, `Zlib OR Apache-2.0 OR MIT`, `(MIT OR Apache-2.0) AND Unicode-3.0` |
| 0 | **none declared** — see below |

**One nuance the phase 1 wording got wrong, and it is worth stating precisely.**
That earlier text claimed "no LGPL appears anywhere". Two packages — `r-efi`
5.3.0 and 6.0.0 — are `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. That is an `OR`,
so the LGPL is an *option we decline*: we take it under MIT, and no copyleft
obligation attaches. `cargo-deny` accepts it for the same reason. `r-efi` is UEFI
target support reached through `getrandom`'s target-specific dependencies and is
never compiled for `x86_64-unknown-linux-gnu`, so it is not in the shipped binary
either. Both facts are true; only the second is load-bearing, and the first is
enough on its own.

### The finding: two crates declared no licence, both in-house — now one

`cargo deny check licenses` initially failed on two, and both were ours:

- **`simmer` itself.** Fixed by `publish = false` in `Cargo.toml`, which is
  accurate — this is a service, not a library, and it never goes to crates.io.
  (Amusingly, `cargo-deny` had also picked up *this file* as a candidate licence
  text and scored it 0.03. `publish = false` settles that too.)
- **`hs-utils 0.31.2`**, which has no `license` field. It was the in-house shared
  crate consumed by git tag from a private repository, so this was a metadata gap
  rather than a licensing risk — but it meant an automated check could not
  distinguish "in-house, fine" from "unlicensed, not fine", and **every
  hikari-systems Rust service has the same hole**.

  ~~Handled in `deny.toml` by `[licenses.private]` with `ignore-sources` scoped
  to the one `hs-utils-rs` git URL.~~ **Resolved in phase 7 by removal.** D-060
  drops the dependency entirely: its one used module, `healthcheck`, is
  stdlib-only and now lives in `src/healthcheck.rs`. The `ignore-sources`
  exemption and the `allow-git` allow-list are both gone, so `unknown-git` now
  denies *every* git dependency rather than all but one, and the only crate in
  the graph without a `license` field is `simmer` itself.

The upstream gap is still real and still worth fixing: a `license` field in
`hs-utils-rs` is a one-line change that would benefit every other service in the
estate. It no longer affects this one.

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

### Phase 7

| Crate | Pinned | Licence | Versions checked | For |
|---|---|---|---|---|
| `metrics-exporter-prometheus` | 0.18.3 | `MIT` | all | §9.1's Prometheus exposition |
| `subtle` | 2.6.1 | `BSD-3-Clause` | all | §9.3's constant-time token compare |
| `tower` *(dev)* | 0.5.3 | `MIT` | all | driving the real router in tests |
| `http-body-util` *(dev)* | 0.1.4 | `MIT` | all | reading a test response body |

`metrics-exporter-prometheus` is the same `metrics-rs` family as `metrics`, which
has been a direct dependency since phase 2 under the same `MIT` terms.
`BSD-3-Clause` is already on `deny.toml`'s allow list and already in the graph;
`subtle` itself has been compiled since phase 2 as a transitive dependency of
`argon2`/`password-hash`, so taking it directly adds nothing to the build.

**`default-features = false` is load-bearing on the exporter**, and not only for
licence surface. The default feature set pulls `hyper`, `hyper-util`,
`hyper-rustls`, `http-body-util`, `ipnet` and a `prost`/`protobuf` push-gateway
client so the exporter can run its *own* HTTP listener — and axum already owns
the admin port. With no features it is a recorder plus
`PrometheusHandle::render()`.

What it does still bring in, all permissive and all verified 2026-08-10:

| Crate | Version | Licence |
|---|---|---|
| `metrics-util` | 0.20.4 | `MIT` |
| `quanta` | 0.12.6 | `MIT` |
| `sketches-ddsketch` | 0.3.1 | `Apache-2.0` |
| `hashbag` | 0.1.13 | `MIT OR Apache-2.0` |
| `evmap` | 11.0.0 | `MIT OR Apache-2.0` |
| `left-right` | 0.11.8 | `MIT OR Apache-2.0` |

`cargo deny check` reports `licenses ok` over the whole graph with these in it,
which is the check that actually enforces the table above.

`tower` and `http-body-util` are dev-dependencies only and are already in the
graph via `axum`, so they add nothing to the shipped image — which contains
neither, since it is built from `--target runtime`.

---

## 3. Direct dependencies, as resolved

| Crate | Version | Licence |
|---|---|---|
| `anyhow` | 1.0.104 | `MIT OR Apache-2.0` |
| `axum` | 0.8.9 | `MIT` |
| `chrono` | 0.4.45 | `MIT OR Apache-2.0` |
| `hmac` | 0.12.1 | `MIT OR Apache-2.0` |
| `ipnet` | 2.12.1 | `MIT OR Apache-2.0` |
| `metrics-exporter-prometheus` | 0.18.3 | `MIT` |
| `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `opentelemetry-appender-tracing` | 0.33.0 | `Apache-2.0` (§10) |
| `regex` | 1.13.1 | `MIT OR Apache-2.0` |
| `serde` | 1.0.229 | `MIT OR Apache-2.0` |
| `serde_json` | 1.0.151 | `MIT OR Apache-2.0` |
| `serde_yaml_ng` | 0.10.0 | `MIT` |
| `sha2` | 0.10.9 | `MIT OR Apache-2.0` |
| `sqlx` | 0.8.6 | `MIT OR Apache-2.0` |
| `tracing-opentelemetry` | 0.34.0 | `MIT` (§10) |
| `subtle` | 2.6.1 | `BSD-3-Clause` |
| `thiserror` | 2.0.19 | `MIT OR Apache-2.0` |
| `tokio` | 1.53.1 | `MIT` |
| `tracing` | 0.1.44 | `MIT` |
| `tracing-subscriber` | 0.3.23 | `MIT` |

Note `serde_yaml_ng` rather than `serde_yaml`: §12.1 names the latter, but it is
unmaintained and published as `0.9.34+deprecated`. `serde_yaml_ng` is the
maintained continuation and carries the same `MIT` terms.

---

## 5. Phase 8 — `hickory-resolver` (§6.7's DNS preflight)

§12.1 suggests it by name and requires the licence be checked **for every
published version**, because this corner of the ecosystem is where the AGPL
changes have happened. Checked against crates.io on 2026-08-11:

| | |
|---|---|
| Published versions | 21 |
| `MIT OR Apache-2.0` | 20 |
| `MIT/Apache-2.0` | 1 — `0.1.0`, the deprecated slash spelling of the same two licences |
| AGPL, GPL or LGPL in any version | **none** |

Adopted at `0.26`, `default-features = false` with the default set named
explicitly (`system-config`, `tokio`) so a future change to the crate's defaults
is a decision rather than a surprise. The TLS, QUIC, HTTPS and DNSSEC features are
deliberately off — see `DECISIONS.md` phase 8.

**It brings 34 packages into the lock file**, which is the largest single
dependency addition since phase 1 and worth recording as such. Every one is
permissive and Apache-2.0-compatible: 25 `MIT OR Apache-2.0`, 2 `MIT`, 1
`Apache-2.0 OR MIT`, 2 `MIT/Apache-2.0`, 3 offering `Unlicense` alongside MIT, and
`moka` at `(MIT OR Apache-2.0) AND Apache-2.0` — an `AND`, so Apache-2.0 applies
either way, which is inside the bar D-062 sets. A large part of the 34 never
compiles in the shipped image at all: `jni`/`jni-sys`/`ndk-context` are Android,
`ipconfig`/`winapi-util`/`windows-registry`/`widestring` are Windows, and
`system-configuration` is macOS. On Linux the resolver reads `/etc/resolv.conf`
via `resolv-conf`.

## 6. Phase 11 — inbound TLS and the test certificates

| Crate | Pinned | Licence | For |
|---|---|---|---|
| `rustls-webpki` | 0.103.13 | `ISC` | §5.1 — whether the certificate covers `server.hostname` (D-070) |
| `rcgen` | 0.14.10 | `MIT OR Apache-2.0` | **dev only** — per-test CAs and leaves |

**`rustls-webpki` adds nothing to the build.** It was already compiled as rustls's
own certificate verifier, at the version rustls pins; phase 11 names it as a direct
dependency only to call its name matching. `ISC` is on the allow-list and
permissive.

**Nothing parses PEM that was not already there.** The design named
`rustls-pemfile`; it has been archived since August 2025 and is
RUSTSEC-2025-0134 (unmaintained), which `cargo deny check`'s advisories would
refuse. `rustls-pki-types`, already in the graph, has carried the same parser since
1.9 and is what is used.

**`rcgen` is a dev-dependency and never reaches the shipped image.** With its
`ring` backend it compiles eight crates the build did not already have — `pem`
(`MIT`), and `base64` 0.23, `time`, `time-core`, `deranged`, `num-conv`, `powerfmt`
and `yasna` (all `MIT OR Apache-2.0`). The lockfile also lists `x509-parser` and its
ASN.1 tree, because Cargo records a crate's optional dependencies whether or not
they are enabled; `rcgen`'s `x509-parser` feature is off, and `cargo tree` confirms
none of them is compiled.

## 7. The test programme — `tikv-jemallocator` (finding F15)

The server binary's global allocator, adopted when the stress tier found glibc's
malloc holding argon2's AUTH blocks resident until the container was OOM-killed
(`DECISIONS.md` D-078). Checked against crates.io on 2026-09-13:

| | `tikv-jemallocator` | `tikv-jemalloc-sys` |
|---|---|---|
| Published versions | 8 | 13 |
| Licence | `MIT/Apache-2.0`, every version | `MIT/Apache-2.0`, every version |
| AGPL, GPL or LGPL in any version | **none** | **none** |

`MIT/Apache-2.0` is the deprecated slash spelling of `MIT OR Apache-2.0`, as
`hickory-resolver`'s `0.1.0` uses (§5); `cargo deny` reads it as such. The
jemalloc C library that `tikv-jemalloc-sys` vendors and builds at compile time is
`BSD-2-Clause`, already on the allow list. Its configure-and-make build needs
nothing the `rust:1-bookworm` builder image lacks. Adopted at `0.7` with default
features. Set in `src/main.rs` only: the library, the tests and the test binaries
keep the system allocator.

mimalloc (`MIT` in all 53 and 50 published versions of `mimalloc` and
`libmimalloc-sys`) was adopted first and replaced after the measured comparison in
D-078, before either was pushed. It is not in the graph.

## 8. D-083 — the link proxy

Adopted for the optional HTTP forwarder, on the instruction to use an
off-the-shelf library for the proxying. Checked against crates.io on 2026-09-17:

| Crate | Version | Licence | Role |
|---|---|---|---|
| `axum-reverse-proxy` | 2.2.0 | `MIT` | the forwarding: hop-by-hop, `X-Forwarded-*`, timeouts, body cap, `Via` |
| `hyper-rustls` | 0.27.9 | `Apache-2.0 OR ISC OR MIT` | the upstream connector, over the existing ring `ClientConfig` |
| `tokio-tungstenite`, `tungstenite` | 0.28 | `MIT` | non-optional dependencies of the proxy crate; compiled, never reached (D-083) |
| `rand`, `url`, `percent-encoding`, `futures-util` | — | `MIT OR Apache-2.0` | likewise transitive |

No AGPL, GPL or LGPL crate enters the graph; `cargo deny check` is the gate.
`hyper`, `hyper-util` and `http-body-util` were already in the graph via axum and
the dev-dependencies, and are now named directly for their features.

`axum-reverse-proxy` is taken with `default-features = false`. Its `tls` feature
would bring in `hyper-rustls` with *that* crate's defaults — `aws-lc-rs`, which
needs cmake and a C toolchain in the builder (the reason the `rustls` entry in
`Cargo.toml` chose ring), and a connector built on `webpki-roots` rather than
the platform store. `cargo tree -i aws-lc-rs` must stay empty.

Rejected: `tower-proxy` 0.10 (`MIT OR Apache-2.0`; no `X-Forwarded-*`, timeouts
or body cap), `pingora-proxy` 0.9 (`Apache-2.0`; a whole server framework and
runtime), `hyper-reverse-proxy` (last released 2022, hyper 0.14), and `sozu`
(AGPL).

## 9. D-084 — the SQL Server build (`--features mssql`)

These crates are in the `mssql` graph only. The default Postgres build does not
compile any of them, so its image is unchanged. Checked against crates.io
2026-09-19; `cargo deny --no-default-features --features mssql check` passes,
and CI runs it.

| Crate | Version | Licence | Why |
|---|---|---|---|
| `tiberius` | 0.12.3 | MIT/Apache-2.0 | the TDS client; sqlx has had no SQL Server driver since 0.7 |
| `bb8` | 0.9.1 | MIT | the connection pool |
| `native-tls` / `async-native-tls` | 0.2.18 / 0.4.0 | MIT OR Apache-2.0 | tiberius' TLS on Linux |
| `openssl` / `openssl-sys` | 0.10.81 / 0.9.117 | Apache-2.0 / MIT | beneath native-tls; links the base image's `libssl3` |
| `encoding_rs` | 0.8.41 | (Apache-2.0 OR MIT) AND BSD-3-Clause | legacy code pages in TDS |
| `connection-string` | 0.2.0 | MIT OR Apache-2.0 | ADO.NET / JDBC parsing |
| `asynchronous-codec`, `pretty-hex` | 0.6.2, 0.3.0 | MIT | tiberius internals |
| `enumflags2`, `multiversion`, `core_detect`, `simdutf8`, `foreign-types`, `openssl-macros`, `thiserror` 1.x | — | MIT OR Apache-2.0 | transitive |

**Why OpenSSL, when the rest of this crate is rustls.** tiberius' `rustls`
feature pins tokio-rustls 0.24, and so rustls-webpki 0.101. That version carries
RUSTSEC-2026-0098, -0099 and -0104, and no fixed 0.101 release exists. The
advisory gate fails on it, and it should. `native-tls` has no open advisories,
and the base image's security updates patch the library it links. D-084 has the
rest, including why neither a git dependency on tiberius' unreleased `main` nor
Microsoft's new `mssql-tds` 0.1.0 was taken.

**`argon2` gained its `std` feature** in the same change. It is not a new crate:
`hash-password` had only ever compiled because sqlx's feature unification
switched on `rand_core/getrandom`.

## 10. D-126 — the OTLP telemetry export

Compiled into both builds and used only when `telemetry:` is configured.
Checked against crates.io 2026-10-02. `cargo deny check` passes for both
feature sets, and `cargo tree -i aws-lc-rs` is empty in both.

| Crate | Version | Licence | Why |
|---|---|---|---|
| `opentelemetry` | 0.33.0 | Apache-2.0 | the API: spans, instruments, log records |
| `opentelemetry_sdk` | 0.33.0 | Apache-2.0 | providers, batch processors, periodic reader, sampler |
| `opentelemetry-otlp` | 0.33.0 | Apache-2.0 | the OTLP/gRPC exporter |
| `opentelemetry-proto` | 0.33.0 | Apache-2.0 | OTLP's protobuf types |
| `tracing-opentelemetry` | 0.34.0 | MIT | `tracing` spans into the trace provider; built against otel 0.33 |
| `opentelemetry-appender-tracing` | 0.33.0 | Apache-2.0 | `tracing` events into the log provider |
| `tonic`, `tonic-prost`, `tonic-types` | 0.14.6 | MIT | the gRPC client |
| `prost`, `prost-derive`, `prost-types` | 0.14.4 | Apache-2.0 | protobuf encoding |
| `hyper-timeout` | 0.5.2 | MIT OR Apache-2.0 | tonic's connect timeout |
| `pin-project`, `pin-project-internal`, `itertools`, `web-time` | — | MIT OR Apache-2.0 | transitive |

`h2` (MIT), `tower` and `rustls` were already in the graph. `http` (MIT OR
Apache-2.0) is now named directly, so that §4.2 parses endpoints and headers with
the same types tonic is handed. It was already compiled, via axum.

No AGPL, GPL or LGPL crate enters the graph.

**Features, and why.** `opentelemetry-otlp` has `default-features = false`. Its
defaults are the HTTP transport with a *blocking* `reqwest` client. With them
off it takes `grpc-tonic`, `tls-ring` and `tls-roots`: ring and the platform
root store, as §8.2 uses. `tls-aws-lc` would bring `aws-lc-rs` and a C
toolchain into the builder, for the reason given in the `rustls` entry in
`Cargo.toml`. `tracing-opentelemetry` also has its defaults off, which drops
its separate metrics path and the `log` bridge. `internal-logs` on the SDK and
exporter is what reports a failed export on stdout.

**Rejected:** `metrics-exporter-opentelemetry` 0.2.1 (MIT). Its licence is fine,
but it pins OpenTelemetry 0.31, which would put two OpenTelemetry versions in
the graph. `src/telemetry/metrics.rs` is the replacement.

## How to re-check

`cargo-deny` is the gate in CI:

```sh
cargo install cargo-deny      # once
cargo deny check licenses
cargo deny check bans sources advisories
```

The policy lives in `deny.toml`: permissive licences are allow-listed, anything
outside the list fails, and there is **no** allow-listed non-registry origin —
since D-060 every dependency comes from crates.io, so `unknown-git = "deny"`
denies all of them.

For the per-version history check that this file's section 1 records — the thing
`cargo-deny` cannot do, because it only sees the version you resolved — query
crates.io directly:

```sh
curl -s "https://crates.io/api/v1/crates/<name>/versions?per_page=100" \
  | jq -r '.versions[] | "\(.num)\t\(.license)"'
```

Do this before adopting any new mail-handling crate, not after.
