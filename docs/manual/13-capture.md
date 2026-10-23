# Capture and replay

For debugging, Simmer can record every message it accepts to files on disk, and
`server replay` can send a recorded range again, to another Simmer. It is off
unless configured, and it is not a queue: nothing in the delivery path reads the
files, and they are not used to recover anything after a crash.
[`docs/CAPTURE.md`](../CAPTURE.md) has the full details.

## Before you turn it on

- **The directory is a mailbox.** It holds every recipient address and every
  message body in clear text: password reset links, one-time codes, anything your
  applications send. Treat the volume as sensitive, and delete it when the
  investigation is over. Simmer warns at startup every time it is on.
- **It uses disk.** Roughly 1 GiB an hour per instance at 10 messages a second of
  ordinary mail; a 25 MiB message becomes a 34 MiB line. `max_body_bytes` and
  `retention` limit it. Alert on `simmer_capture_disk_bytes`.
- **Replay delivers mail again, on purpose**, and uses the target's allowance to
  do it. Point it at a test instance, never at production.

## Turning it on

```yaml
capture:
  directory: "/var/lib/simmer/capture"
  max_body_bytes: 1048576      # default 1 MiB; above this, the body is not recorded
  retention: 24h               # default; at least 10m
  on_error: continue           # default; or defer
  queue_depth: 1024            # default
  max_queue_bytes: 67108864    # default 64 MiB
```

| Key | Default | Meaning |
|---|---|---|
| `directory` | required | An absolute path. Created with mode `0700` if needed; files are `0600`. Must be writable at startup |
| `max_body_bytes` | 1 MiB | Larger messages are recorded without their body |
| `retention` | `24h` | Files older than this are deleted. At least `10m`, one file's worth |
| `on_error` | `continue` | What a failed write does: `continue` relays the message anyway; `defer` answers `451` and relays nothing |
| `queue_depth`, `max_queue_bytes` | 1024, 64 MiB | How much may wait to be written. Beyond either, records are dropped and counted rather than slowing mail down. `max_queue_bytes` must be at least twice `server.max_message_bytes` |

Leave `on_error` at `continue` unless a gap in the recording would ruin what you
are investigating. With `defer`, a full disk stops mail.

**In Docker**, the container's filesystem is read-only and it runs as UID 1000,
so the directory must be a writable volume owned by that user. A new named
volume is owned by root; fix it once with
`docker run --rm -v simmer-capture:/c alpine chown 1000:1000 /c`.

## The files

One file per ten minutes, named for its start time in UTC:
`2026-09-20T14.10.jsonl`. Each line is one message as JSON, starting with the
fields a person looks for, so a file is readable without tools:

```sh
cut -c1-140 2026-09-20T14.10.jsonl
jq -r '[.at, .rcpt_to[0], .mail_from, .subject] | @tsv' 2026-09-20T14.10.jsonl
```

A record reaches its file within ten records or half a second of quiet, so
`tail -f` on the current file keeps up.

## Replay

```sh
# Always count first. This connects to nothing.
server replay --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host test-simmer --dry-run

# Then send.
SIMMER_REPLAY_PASSWORD_APP=... server replay --dir /var/lib/simmer/capture \
  --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
  --host test-simmer --confirm
```

It needs no configuration file. `--dir`, `--from`, `--to` and `--host` are
required, and nothing is sent without `--confirm`. Passwords come from
environment variables, never the command line: `SIMMER_REPLAY_PASSWORD_<USER>`
for each recorded user, or `SIMMER_REPLAY_PASSWORD`, or the variable named by
`--password-env`. `server replay --help` lists the options for TLS, pacing and
limits. In Docker, run it as
`docker run --rm -v simmer-capture:/var/lib/simmer/capture hikarisystems/simmer:latest replay ...`.

## Metrics

`simmer_capture_records_total`, `simmer_capture_bytes_total`,
`simmer_capture_dropped_total{reason}`, `simmer_capture_deferred_total`,
`simmer_capture_body_omitted_total`, `simmer_capture_files_swept_total`,
`simmer_capture_queue_depth`, `simmer_capture_queue_bytes` and
`simmer_capture_disk_bytes`.
