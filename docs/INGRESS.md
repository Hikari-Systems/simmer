# Inbound listeners, TLS, and the sender ACL — design

**Status: built in phase 11** (2026-09-11) as `DECISIONS.md` D-070 (listeners and
TLS) and D-071 (the ACL). This is the design as approved; the phase 11 entries
record where the build departed from it — `rustls-pki-types` rather than
`rustls-pemfile`, the `530`/`538` split, no `454` after a failed handshake — and
§8's five questions are answered there. §5's defect was fixed separately, in phase
10 (D-066). §6's pre-authentication limits were not built (D-072).

This is new capability, not a divergence in how something already specified gets
implemented. `SPEC.md` rules all three of these out explicitly, so §1 below is the
part to read before anything else. Recorded as `DECISIONS.md` D-033.

---

## 1. This reverses four things SPEC.md says

`SPEC.md` is authoritative and is not amended silently. These are the passages
this design contradicts, quoted so the conflict is visible rather than implied:

| § | Says |
|---|---|
| **2.2** | "**No inbound TLS.** See §5.1." |
| **5.1** | "Plaintext TCP, default port 25. **No STARTTLS, no implicit TLS, no ACME.**" |
| **2.3** | "The listener is plaintext and accepts plaintext AUTH. The container must not be exposed to an untrusted network." |
| **5.2** | "`EHLO` advertises **exactly**: `PIPELINING`, `8BITMIME`, `SMTPUTF8`, `SIZE`, and `AUTH` … **Nothing else.**" |

And one that inverts rather than merely extends — §4.2 currently makes it a
**startup violation** for `allow_insecure_auth` to be false:

> `allow_insecure_auth` is false (there is no inbound TLS, so AUTH would be
> unusable).

Under this design that rule has to become its mirror image: plaintext AUTH is
refused *unless* a listener deliberately permits it.

**This needs the spec's author to decide**, and it is the first open question in
§8. Either `SPEC.md` §2.2/§2.3/§5.1/§5.2/§4.2 are amended by whoever owns them, or
D-033 stands as an override large enough that the spec no longer describes the
component. A divergence entry is the right home for "we did X instead of Y"; it is
a poor home for "the four places the spec says never are now sometimes".

**What does not change.** §2.3's substance survives: TLS and authentication do not
turn Simmer into a public MX. There is no inbound mail handling, no bounce
processing, no reputation defence, no rate limiting against hostile peers. It
remains a submission relay for our own applications that can now sit on a segment
where cleartext credentials are unacceptable — which is a smaller claim than
"internet-facing", and the README must keep saying so.

---

## 2. Listeners

`server.listen` becomes `server.listeners`, a list. Each carries its own security
policy, because 25, 465 and 587 have genuinely different rules and a single global
switch cannot express them.

```yaml
server:
  hostname: "simmer.internal"          # also the name the certificate must cover
  listeners:
    - address: "0.0.0.0:25"
      tls: starttls                    # advertised, not required
      auth: optional
    - address: "0.0.0.0:587"           # RFC 6409 submission
      tls: starttls_required
      auth: required
    - address: "0.0.0.0:465"           # RFC 8314 submissions, implicit TLS
      tls: implicit
      auth: required
  tls:
    certificate: "/etc/simmer/tls/fullchain.pem"
    private_key: "/etc/simmer/tls/privkey.pem"
```

| `tls` | Behaviour |
|---|---|
| `off` | Plaintext only. `STARTTLS` is not advertised. Today's behaviour |
| `starttls` | Advertised and accepted; a client may decline and continue in plaintext |
| `starttls_required` | Advertised; `MAIL FROM` and `AUTH` are refused until the handshake completes |
| `implicit` | TLS from the first byte. No `STARTTLS` — RFC 8314 forbids advertising it on such a port |

| `auth` | Behaviour |
|---|---|
| `disabled` | `AUTH` not advertised, `AUTH` command is `503` |
| `optional` | Advertised; unauthenticated sessions may still send |
| `required` | Advertised; `MAIL FROM` before a successful `AUTH` is `530 5.7.0` |

Per-port defaults follow the RFCs rather than being invented: 25 is transfer
(RFC 5321), 587 is submission (RFC 6409 — auth expected), 465 is submissions with
implicit TLS (RFC 8314, which also states the preference for implicit over
`STARTTLS`). Configuration can override any of it; the defaults exist so that the
common arrangement is not a puzzle.

### Certificates

One certificate and key, PEM, read from disk at startup. **No ACME** — that part of
§5.1 stands, and §2.2's "no hot config reload" means rotation is a restart.

Startup validation should check that the certificate covers `server.hostname`,
warn (not fail) when it does not, and log the not-after date. A relay whose
certificate expired quietly is a relay that stops accepting submissions at 3am for
a reason nobody will guess quickly.

The container runs as UID 1000 (`Dockerfile`), so a key mounted root-only is a
startup failure with a confusing message unless the mount permits it. Worth a
named check.

---

## 3. STARTTLS mechanics that will bite

**The buffering attack is the one that matters.** After `220 Ready to start TLS`
and before the handshake, any bytes already in the read buffer were sent in
cleartext by something that is not the TLS peer. Accepting them lets an attacker
inject commands that appear to have arrived over the encrypted channel — the
plaintext-command-injection class (CVE-2011-0411 and its descendants). The
connection must be dropped, not drained.

Simmer already does exactly this check on the **outbound** side, in
`downstream/client.rs`:

```rust
let buffered = self.reader.buffer().len();
if buffered > 0 {
    return Err(RelayError::Protocol(Stage::StartTls, ...));
}
```

The inbound implementation should be the mirror of it, and should say so, because
the two are the same defect seen from opposite ends.

**Session state resets at the handshake.** RFC 3207 §4.2 requires the server to
discard everything learned before `STARTTLS`; the client re-issues `EHLO`. In
`Session` that means clearing `greeted`, `esmtp`, `authenticated` and
`transaction`. It must **not** clear `auth_failures` — §5.3's three-strike budget
is per connection, and resetting it would hand an attacker unlimited attempts for
the cost of one extra round trip.

**`STARTTLS` disappears from `EHLO` once TLS is active**, or a client may loop.

**Swapping the stream mid-session.** `Session<S>` is already generic over
`AsyncRead + AsyncWrite + Unpin`, so a TLS stream fits. Replacing the stream
in place needs the same move-out-and-back that `downstream::stream::Stream` solves
with its `Taken` variant; an inbound analogue should reuse that shape rather than
inventing a second one.

**`538 5.7.11`** is the reply for `AUTH` attempted on a connection that has not
yet negotiated TLS where the listener requires it. Not `530`, which means "you have
not authenticated"; the client needs to know encryption is the missing thing.

---

## 4. The ACL, modelled on Slater

Slater (`/home/rickk/git/hs/slater`) already solves the shape of this. What is
adopted, and the two places we deliberately differ:

| Slater | Simmer |
|---|---|
| `users` keyed by username | already have `server.auth.users` |
| `passwordArgon2id` — a `$argon2id$…` PHC string | already `password_hash`, same argon2id parameters |
| `grants: { <resource>: [<capability>…] }` | `grants: { send_as: [<pattern>…] }` |
| A resource absent from grants is invisible | an identity outside the grant is refused. **Default deny** |
| Failure limits are per *connection*, never per account, "so it cannot be abused to lock a user out" | §5.3's three strikes already work this way. Agreement, not adoption |
| `slater hash-password` mints a hash | add `server hash-password` |
| Unrecognised permission strings grant nothing | **differ** — reject at startup (below) |
| ACL is a separate `acl.json` on shared storage | **differ** — stays in `simmer.yaml` |

Keeping the ACL in the main config is deliberate and was asked for. Slater's file
is separate because it is reloaded on generation hot-swap and lives on shared
storage; Simmer has neither property — §2.2 rules out hot reload, so an ACL in a
second file would buy a second thing to mount and nothing else.

```yaml
server:
  auth:
    required: true
    allow_insecure_auth: false         # meaning inverted; see §1
    mechanisms: [PLAIN, LOGIN]
    users:
      - username: "cfapp"
        password_hash: "${SIMMER_CFAPP_HASH}"
        grants:
          send_as:
            - "oldbrand.com"           # §5.4's pattern grammar, unchanged
            - "*.oldbrand.com"
            - "marketing@newbrand.com"
```

**The pattern grammar is §5.4's, reused verbatim** — exact domain, `*.subdomain`,
full address, matched case-insensitively. `routing::sender_match::Pattern` already
implements it and is thoroughly tested, operators already know it, and a second
grammar for the same kind of thing is a second grammar to get wrong.

**Unrecognised keys are rejected, where Slater ignores them.** Slater's choice is
right for a file reloaded at runtime, where a bad edit must not take the server
down. Simmer's D-013 argues the opposite for a config read once at startup: a
silently ignored `send_as` typo is a grant that quietly does nothing, and "the
limit you configured was ignored" is the worst failure mode this component has.
`deny_unknown_fields` already covers it.

### The §5.3 collision, which is the important part

§5.3 is unambiguous:

> Authentication is **authentication only**. The authenticated username plays no
> part in route selection.

An ACL does not violate this **provided it gates acceptance and never routing**.
The distinction is not pedantry:

- *Permitted*: user `cfapp` may present `oldbrand.com`; if it presents anything
  else the message is refused. The sender identity then selects a chain by §5.4,
  exactly as it does today.
- *Forbidden*: user `cfapp` routes via chain X. That would make the outbound
  identity depend on **who authenticated**, which is not expressible as
  application-side configuration — so it breaks the cutover invariant (§1.1)
  outright, not just §5.3.

Two different users permitted to send as `oldbrand.com` must produce byte-identical
output. That should be an explicit test, not an assumed property.

**Denial reply: `550 5.7.1 sender not permitted`.** §10.3 already carves out
exactly this shape for `strict_senders` — "a policy statement about the *sender*,
[which] will not trigger recipient suppression, and should be loud because it
indicates misconfiguration". §14.1 is satisfied: nothing permanent is being said
about a recipient.

**Which identity is checked, and when.** The envelope sender at `MAIL FROM`, where
it is cheap and the body has not been transferred. The `From:` header at the final
dot, where it first exists — the same split §5.4 already lives with. Both must be
inside the grant; a user permitted to send envelope-as `oldbrand.com` but
presenting a `From:` of someone else's domain is the case the ACL exists to stop.

---

## 5. A defect this exercise found in shipped code

Slater's `equalisation_hash` (`crates/slater/src/acl.rs`, their HIK-222) documents
a bug **Simmer currently has**.

`smtp/auth.rs` verifies an unknown username against a fixed decoy hash minted at
`m=19456,t=2,p=1` so that both paths cost the same and the username cannot be
found by timing. But argon2 verification is **parameter-agnostic**:
`PasswordHash::new` reads `m`/`t`/`p` out of the *stored* string and re-derives at
those. The moment an operator mints a hash with anything other than the §4.1
example's parameters, the real path and the decoy path diverge in cost and
username enumeration by timing is back — silently, with the mitigation still
apparently in place. Simmer's own comment concedes it: "the decoy costs *roughly*
what a real verification costs".

Slater's fix is to borrow the costliest hash the ACL actually holds, so the
unknown-user path runs the very same derivation a real login runs. It is exact when
parameters are uniform (the normal case) and degrades in the safe direction when
they are not.

**This is a live defect in phase 2 code, independent of everything else in this
document.** It should be fixed on its own, not bundled into a feature.

---

## 6. Pre-authentication limits

Slater bounds what an unauthenticated peer may consume — `maxPreAuthConnections`,
`maxPreAuthBytes`. Once Simmer listens on a less-trusted segment the same idea
applies, and it is largely already there in pieces: `max_concurrent_sessions`
(§5.1), the 4096-octet command line and 65536-octet data line caps (D-020), and
the `timeouts.command` budget.

What is missing is the *pre-auth* framing: a separate, tighter session cap and a
byte ceiling that apply until `AUTH` succeeds, so a peer that never authenticates
cannot occupy the whole session pool. Worth adding with the listeners rather than
after.

---

## 7. Where this goes in the build order

`SPEC.md` §13 has ten phases and this is in none of them. Proposed: **a new phase
11**, after the ten.

**Not before phase 4.** Simmer does not yet rewrite anything, which means it does
not yet do the job it exists for. Adding ports and certificates to a component that
does not warm anything is decoration, and it delays the one phase that makes the
rest meaningful.

**Unless the deployment forces it earlier** — if Simmer has to sit somewhere that
cleartext credentials are unacceptable before the warm-up is finished, this moves
ahead of phase 4 and the rewriting engine waits. That is a deployment question, not
an engineering one, and it is open question 2 below.

Rough shape once it starts: listeners and per-listener policy first (mechanical,
and it makes the config change visible early), then implicit TLS on 465 (simpler
than `STARTTLS` — no state reset, no injection window), then `STARTTLS`, then the
ACL. The ACL is last because it is the only part with no protocol risk in it.

New dependency: `rustls-pemfile` (`Apache-2.0 OR ISC OR MIT`, every published
version — checked). `rustls` and `tokio-rustls` are already present for §8.2's
outbound TLS and gain the server-side feature.

---

## 8. Open questions

1. **Does `SPEC.md` get amended, or does D-033 override it?** §1 is the whole of
   this question. Four passages say never; this says sometimes. Whoever owns the
   spec should decide, because "read the spec, then read the four places
   DECISIONS.md reverses it" is not a specification.
2. **Does this go before or after phase 4?** Engineering says after (§7).
   Deployment may say otherwise.
3. **What replaces `server.listen`?** Cleanest is to remove it and require
   `listeners`, since nothing is deployed and D-013 already rejects unknown keys —
   a stale `listen` would fail loudly rather than being ignored. The alternative is
   accepting it as shorthand for a single plaintext listener, which is kinder to an
   existing config file and one more thing to test.
4. **Should the ACL grant anything besides `send_as`?** Recipient restrictions and
   per-user size or rate limits are the obvious candidates, and Slater's
   capability-list shape accommodates them without a schema change. Nothing needs
   them today, and adding a capability nobody uses is a capability nobody tests.
5. **Does `auth: optional` on port 25 earn its place?** It is the RFC-shaped
   default, but a submission relay that accepts unauthenticated mail on a segment
   worth encrypting is a strange combination. Possibly 25 should default to
   `auth: required` too, and the RFC default is the wrong instinct here.
