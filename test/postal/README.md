# Postal v3 as a test downstream — spike

**Status: feasible, not yet wired into the suite.** This directory is the working
result of the test programme's Postal spike (2026-09-12). Step 8 of the programme
turns it into the `postal` entry of the T2 server matrix; until then it runs on its
own.

Postal is the production downstream, which is why it is worth its weight here: the
matrix's other servers prove Simmer speaks SMTP correctly, and this one proves it
speaks to the server it will actually meet.

## Running it

```sh
./run.sh           # from nothing: start, provision, send through Postal, verify in Mailpit, tear down
./run.sh --keep    # leave the stack up afterwards
```

It uses compose project `postalspike`, so it cannot collide with the Simmer stacks.
No bind mounts and no published ports: configuration is environment variables and
inline `configs:`, keys live in a named volume, and `run.sh` reaches the stack by
joining its network. That is deliberate — it is the shape that works in sandboxed CI
as well as on a laptop.

| File | What |
|---|---|
| `compose.yml` | Postal 3.3.7 (`smtp-server`, `worker`, one-shot `postal-init`), MariaDB 10.11, Mailpit. All pinned by digest |
| `gen-keys.rb` | Postal's signing key and a self-signed STARTTLS cert, written only if absent |
| `provision.rb` | Headless provisioning via `rails runner`: user, organisation, a **Live** server, a verified domain, an SMTP credential with a known key. Idempotent |
| `run.sh` | The end-to-end check |
| `smtp_probe.py` | Stdlib-only probe: EHLO, AUTH mechanisms, reply codes, delivery into Mailpit |
| `probe-results.json` | What the probe observed on the spike run — the evidence for everything below |

`compose.yml` embeds copies of `gen-keys.rb` and `provision.rb` as inline configs,
because bind mounts are unavailable in the sandbox it was built in. Edit both, or
generate one from the other, when step 8 folds this into the matrix.

## What Postal does that the matrix has to know

Observed on the spike run (`probe-results.json`), not read from documentation:

- **EHLO:** `STARTTLS` and `AUTH CRAM-MD5 PLAIN LOGIN`. **No `SIZE`, `8BITMIME`,
  `SMTPUTF8` or `PIPELINING`.** It nevertheless answers `250` to `MAIL FROM … BODY=8BITMIME`
  and to `SMTPUTF8`, and pipelined commands work.
- **Simmer consequence — finding F13.** Simmer's outbound client refuses to send a
  `BODY=8BITMIME` message to a downstream that does not advertise `8BITMIME`
  (`MissingCapability` → `451 4.3.5`, `src/downstream/client.rs`). Against Postal
  that defers every such message, on every retry.
- **Postal replaces the envelope sender** with `<server-token>@<return-path domain>`,
  whatever the client sent. Assert on headers, never on the envelope Mailpit sees.
- It adds `X-Postal-MsgID`, `Resent-Sender` (off with
  `POSTAL_USE_RESENT_SENDER_HEADER=false`) and a `DKIM-Signature` under its own
  return-path domain. The client's `Message-ID` survives.
- **The sender check is on the `From:` header**, against a verified domain, and it
  happens after DATA: `530 From/Sender name is not valid`. With no `SIZE`, an
  oversized message is likewise refused only after its body: `552`.
- Replies: `235 Granted for <org>/<server>`; `535 Invalid credential`;
  `530 Authentication required`; `250 OK` at the final dot, with no queue id.
- Cold start about 20 s to healthy once images are pulled; about 420 MiB for the
  whole stack.
