# Capture and replay (D-085, D-086)

An optional debugging mode. Simmer appends every accepted message to a JSONL file
rotated every ten minutes, and `server replay` reads a range back out and sends
it again to another Simmer.

> **This is a debugging mode, not a spool.** It is off unless configured, nothing
> in the delivery path reads it, it recovers nothing after a crash, and a record
> structurally cannot carry a delivery outcome. `SPEC.md` §2.2 is untouched. What
> it *does* do is accumulate, in plaintext, every recipient address and every
> body that passes through — which is precisely what §7.3 hashes recipients to
> avoid. Turn it off when the investigation is over.

---

## 1. Before you enable it

| | |
|---|---|
| **It is a mailbox on disk.** | Bodies and recipients in the clear. Password-reset links, one-time codes, session tokens — whatever your application sends. The directory is `0700` and the files `0600`, and that is the containment. Put it on a volume you are willing to treat as sensitive, and delete it afterwards. |
| **It costs disk.** | A 25 MiB message is a ~34 MiB line. At 10 msg/s of 1 MiB bodies that is ~4.8 GiB an hour. `max_body_bytes` (default 1 MiB) caps the pathological case and `retention` (default 24h) bounds the rest. Alert on `simmer_capture_disk_bytes`. |
| **It costs a little CPU on the message path.** | base64 and SHA-256 over the body, on the session's own task. An instance with capture on is not the instance you were measuring a latency problem on. |
| **Replay delivers mail twice.** | See §5. |

---

## 2. Turning it on

```yaml
capture:
  directory: "/var/lib/simmer/capture"
  max_body_bytes: 1048576      # above this, the envelope is kept and the body is not
  retention: 24h               # whole buckets deleted past this; minimum 10m
  on_error: continue           # or `defer` — see below
  queue_depth: 1024
  max_queue_bytes: 67108864
```

Absent means off: no directory, no task, no metric. Every key but `directory`
has a default.

Startup validation checks the directory really exists and is really writable by
probing it, rather than reading mode bits — the container runs as UID 1000
against whatever volume was mounted, and a read-only filesystem has perfectly
permissive modes. A capture that is configured but silently writing nothing is
the worst outcome available, so this is a startup failure, not a surprise at
3am.

Every boot logs a `WARN` naming §7.3 and the directory. That is deliberate and
unconditional.

### `on_error`

- **`continue`** (default) — a failed write is logged at `ERROR`, counted on
  `simmer_capture_dropped_total`, and the message is relayed anyway. A debugging
  aid must not be able to stop mail.
- **`defer`** — the message is answered `451 4.3.0 message capture unavailable`
  and **nothing is relayed**. Use it when a gap would invalidate the run. A full
  disk then stops mail, and startup warns about that too.

`defer` is only coherent because the capture happens *before* the downstream
conversation. Raised afterwards, that `451` would defer a message the downstream
had already accepted, and the client's retry would deliver it twice — §10.2's
hazard, which D-068 spends three conditions avoiding by accident.

### In the container

`docker-compose.yml`'s `app` service is `read_only: true` and runs as UID 1000,
so the directory needs a writable **named volume** — a host bind mount is
unlikely to be owned correctly. The mount and the `capture:` block are both
commented out and belong together; uncomment both.

A fresh named volume is created root-owned, so the first start refuses:

```
simmer: configuration is invalid:
  - capture.directory: '/var/lib/simmer/capture' is not writable: Permission denied (os error 13)
```

That is the right failure — the alternative is a capture that silently records
nothing — and the fix is a one-off:

```sh
docker run --rm -v simmer_capture:/d alpine chown 1000:1000 /d
```

---

## 3. The files

```
/var/lib/simmer/capture/
  2026-09-20T14.00.jsonl
  2026-09-20T14.10.jsonl
  2026-09-20T14.20.jsonl      <- being written
```

`%Y-%m-%dT%H.%M.jsonl`, UTC, the minute floored to a multiple of ten and
zero-padded so lexical order is time order. A file whose name does not round-trip
is not ours, and neither the sweeper nor the reader will touch it — so a `README`
or a `.gz` you made in the same directory is safe.

**A record's timestamp is always inside the window of the bucket its file names.**
That is the invariant a replay's file selection rests on, and it holds even when
records arrive at the writer out of order or the wall clock steps backwards — the
writer re-opens the earlier bucket rather than filing the record somewhere
convenient. The cost is that lines within one file are not guaranteed ordered,
which is why the reader sorts.

A restart inside the same ten minutes appends to the existing file. The name is a
pure function of the bucket, so there are no sequence numbers and no pids.

---

## 4. A record

One JSON object per line. Wrapped here; one line on disk.

```json
{"at":"2026-09-20T14:23:07.412Z",
 "rcpt_to":["alice@example.com"],
 "mail_from":"news@oldbrand.com",
 "subject":"Your September statement",
 "v":1,
 "id":"7b2f4a6c-1d9e-4b21-9f0a-3c5e8d1a2b44",
 "peer":"10.0.3.17:52344",
 "helo":"app-7.internal",
 "tls":true,
 "auth_user":"marketing",
 "params":{"size":9421,"body_8bitmime":false,"smtputf8":false,"auth_identity":null},
 "size":9421,
 "sha256":"e3b0c442...",
 "body_omitted":false,
 "body_b64":"RnJvbTogbmV3c0BvbGRicmFuZC5jb20NCg..."}
```

**The first four fields are for you, not for a parser.** *When, to whom, from
whom, about what* — the four things that identify a message to a person — come
before any of the machinery, so a bucket file is scannable with the body pushed
off the right-hand edge:

```sh
cut -c1-160 2026-09-20T14.10.jsonl
jq -r '[.at, .rcpt_to[0], .mail_from, .subject] | @tsv' 2026-09-20T14.10.jsonl
```

JSON object order means nothing to a parser, so nothing depends on it; it is
declaration order in `Record`, and there is a test on the bytes.

| field | |
|---|---|
| `at` | the final dot. Buckets the record; `--from`/`--to` filter on it. |
| `rcpt_to` | a list because that is the wire form, though D-047 pins it at one. |
| `mail_from` | the envelope sender. `""` is the null sender `<>`; `null` means none was given. |
| `subject` | RFC 2047-decoded, unfolded, and cut to 200 characters with a `…`. **`""` when there is none — never `null`**, so the row of four stays uniform. A label for a human: it is not byte-faithful and nothing reads it back. |
| `v` | schema version. A reader refuses one it does not know rather than guessing. |
| `id` | the session's `correlation_id` — the **join key to the log stream**, which is where the reply lives. |
| `params` | not cosmetic — a replay that does not re-present `SMTPUTF8` and `BODY=8BITMIME` is not replaying the same transaction. |
| `size`, `sha256` | over the raw body, present **even when the body is not**, so an omitted record still names its message. |
| `body_b64` | standard base64, no line breaks. Absent exactly when `body_omitted`. |

`subject` is the only field that is a projection of the body rather than
something the transaction carried. It earns that twice: it is what makes the file
scannable, and when `body_omitted` is true it is the only human handle the record
has left.

### What is deliberately not there

**No `reply`, no `code`, no `route`, no `domain_group`, no `attempts`, no
`state`.** The record is built before the route walk and before the downstream
conversation, so none of them exists yet. A spool's minimum schema is "the
message, and what we still owe it"; this has only the first half, and that is
what keeps a capture directory from being mistaken for a queue. There is a test
asserting the emitted key set exactly.

**No credential, in any form** — no password, no AUTH blob, no mechanism, no
failed attempt. Only the username that resulted.

### What `body_b64` decodes to

The §8.1 buffer's contents: the **unstuffed, CRLF-normalised** message.
Transparency dots are already removed and a bare `LF` has already been promoted
to `CRLF`. That is the canonical RFC 5322 message — exactly what the rewrite
engine parsed and what the downstream received — but it means a message sent with
bare `LF` endings replays as `CRLF`. "Byte-identical" is a claim about the
canonical message, not about the bytes that crossed the socket.

### What is not captured

- A message the D-071 `From:`-header ACL refused. It never reached a route.
- A message refused before the final dot — oversize, a `DATA` timeout, a dropped
  connection. There is no complete message to record.

---

## 5. Replaying

> **This delivers mail.** The target receives messages it may already have, and
> spends its warm-up quota (§7.4) doing so. Replaying a day's traffic into a
> production instance corrupts that instance's ramp. **Point it at a test
> instance.**

```sh
server replay --dir <path> --from <ts> --to <ts> --host <target> [options]
```

Always count first:

```sh
server replay --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host app2 --dry-run
```

Then mean it:

```sh
SIMMER_REPLAY_PASSWORD_CFAPP=... server replay \
  --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host app2 --port 25 --confirm
```

`--confirm` is required and has no default. `--host` is required and has no
default, so there is no "accidentally localhost".

### Timestamps

Four forms, all resolving to **UTC** — never the process's timezone, because the
filenames and the records are UTC and a silently-local `--from` is a bug you find
after the replay:

```
2026-09-20T14:20:00Z        RFC 3339
2026-09-20T15:20:00+01:00   RFC 3339 with an offset
2026-09-20T14.20            a bucket name, pasted straight off `ls`
now-90m                     relative, using the config's duration grammar
```

The range is `[from, to)` — half-open, so two adjacent replays cover a range
exactly once.

### Credentials, never on argv

Per record, the username is `--user` if given, else the record's own `auth_user`;
a record with no `auth_user` replays unauthenticated. The password is looked up
in this order:

1. `--password-env VAR`
2. `SIMMER_REPLAY_PASSWORD_<USER>` — uppercased, everything outside `[A-Z0-9]`
   replaced by `_`, so `marketing-eu` → `SIMMER_REPLAY_PASSWORD_MARKETING_EU`
3. `SIMMER_REPLAY_PASSWORD`

**Every user in the range is resolved before anything is sent.** A missing
password exits 4 having sent nothing, naming the variables it looked for —
discovering it after four thousand of ten thousand messages have gone out is not
a failure you can undo.

There is no `--password` flag. Passing one is refused by name, with the reason: a
password on the command line lands in shell history, `ps` output and the
container's recorded command line.

### It adds nothing to the message

No marker header, no stamp. An extra header would break the §1.1 byte-equality
property, which is the only property that makes a replay worth running — the same
reason `loadgen --stamp` is opt-in. A replayed message is identifiable from the
target's own capture (a different `peer`, a different `id`), never from its
content.

### Output and exit codes

One JSON object per message on stdout; the summary on stderr, so stdout stays
machine-readable.

| code | |
|---|---|
| 0 | everything sent was accepted |
| 1 | something was not accepted (4xx, 5xx or transport) |
| 2 | usage error |
| 3 | `--confirm` absent — nothing sent, the count printed |
| 4 | a credential could not be resolved — nothing sent |
| 6 | the capture directory could not be read |

0 and 1 are split by reply class so `server replay … && echo ok` means something.

### Records it will not send

- `body_omitted: true` — there is nothing to send, and synthesising a body would
  send bytes that never arrived, destroying the one property a replay has. Counted
  as `skipped_no_body`.
- A line that does not parse — a file truncated by a crash ends this way, and
  everything before the cut is still perfectly replayable. Counted as
  `malformed_lines`.

### Selecting a subset

Because a record carries no outcome, replay cannot filter on "the ones that were
delivered". That is deliberate: keeping the outcome out of the format is what
keeps the format from becoming a spool's journal. Join `id` against the log
stream instead — `id` is the `correlation_id` that §9.5 puts on every line,
including the one with the downstream reply code.

---

## 6. Metrics

All unlabelled by anything about the message. The capture runs before route
selection and does not know a route; a label naming a sender, recipient or domain
would put in `/metrics` exactly what §7.3 keeps out of the database.

| | |
|---|---|
| `simmer_capture_records_total`, `_bytes_total` | what was written |
| `simmer_capture_dropped_total{reason}` | `queue_full`, `queue_bytes`, `write_error`, `open_error`, `shutdown`. Under `continue` these are gaps in the capture and nothing else |
| `simmer_capture_deferred_total` | **alert on this** — mail is being stopped for a debugging feature |
| `simmer_capture_body_omitted_total` | over `max_body_bytes` |
| `simmer_capture_late_writes_total` | records that arrived out of order. Harmless |
| `simmer_capture_clock_regressions_total` | the wall clock stepped backwards; a replay of that range may need a wider `--pad-buckets` |
| `simmer_capture_files_swept_total` | deleted past retention |
| `simmer_capture_queue_depth`, `_queue_bytes` | the writer's backlog |
| `simmer_capture_disk_bytes` | **what tells you a capture left on will fill the volume** |

There is **no admin API surface**, deliberately. `tests/admin_api.rs` asserts
that no read endpoint emits so much as an `@`, and a capture browser on the
control plane is exactly what would turn a debugging mode into a permanent one.

---

## 7. Turning it off

Remove the `capture:` block and restart (§2.2 — there is no hot reload). Then
delete the directory:

```sh
docker compose down
docker volume rm simmer_capture
```
