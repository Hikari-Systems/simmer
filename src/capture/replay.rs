//! `server replay` — read a range out of a capture directory and send it again.
//!
//! # This delivers mail. Twice.
//!
//! Replay exists to re-send messages a downstream has already accepted. That is
//! not a side effect, it is the function, and it is the hazard §10.2 and D-068
//! spend three load-bearing conditions avoiding by accident. So it is gated:
//! [`ReplayArgs::confirm`] is required and has no default, `--host` is required
//! and has no default — there is no "accidentally localhost" — and `--dry-run`
//! is the safe form that connects to nothing.
//!
//! The second cost is §7.4. A replayed message reserves and commits on the
//! target exactly as a real one does, because a replay that bypassed quota would
//! not be exercising the thing under test. Replaying a day's traffic into a
//! production instance therefore corrupts that instance's ramp. **The target is
//! a test instance.**
//!
//! # It adds nothing to the message
//!
//! No `X-Simmer-Replay`, no stamp, no marker. `loadgen --stamp` is opt-in for
//! exactly this reason: an extra header breaks the §1.1 byte-equality property,
//! which is the only property that makes a replay worth running. A replayed
//! message is identifiable from the target's own capture — a different `peer`,
//! a different `id` — never from its content.
//!
//! # It needs no configuration
//!
//! Everything is on the command line, so a capture directory can be copied off
//! a host and replayed from a machine that has no `simmer.yaml` at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::bucket;
use super::client::{self, AuthMech, Credentials, Submission, Target, TlsMode};
use super::record::{ParseError, Record};

/// The environment variable consulted when a record's user has no more specific
/// one. Never a flag: `hash-password` reads its password from stdin rather than
/// argv because an argument lands in shell history, `ps` output and the
/// container's recorded command line, and the same is true here.
const SHARED_PASSWORD_VAR: &str = "SIMMER_REPLAY_PASSWORD";

/// How many buckets either side of the range to read, by default.
///
/// A record is filed under its own timestamp, always — that is the writer's
/// invariant. But a clock that stepped backwards puts records in a bucket whose
/// name no longer brackets the range a reader would compute, so one bucket of
/// slack is cheap insurance. It costs I/O and never correctness: the record's
/// own `at` is what decides.
const DEFAULT_PAD_BUCKETS: i64 = 1;

pub const USAGE: &str = "\
usage: server replay --dir <path> --from <ts> --to <ts> --host <host> [options]

  Reads captured messages (D-085) in [--from, --to) and sends them again.

  THIS DELIVERS MAIL. The target receives messages it may already have; it
  spends its warm-up quota (SPEC.md §7.4) doing so. Point it at a test
  instance, run --dry-run first, and pass --confirm when you mean it.

required
  --dir <path>          the capture directory
  --from <ts>           inclusive lower bound, RFC 3339 or a bucket name
  --to <ts>             exclusive upper bound
  --host <host>         the target simmer
  --confirm             actually send. Without it nothing is sent

target
  --port <n>            default 25
  --tls off|starttls|implicit      default off
  --ca <pem|os>         trust anchor for the TLS modes. Default os
  --tls-name <name>     SNI and verification name. Default --host
  --helo <name>         override each record's own EHLO name

credentials (never on argv)
  --user <name>         present this user for every record, instead of the
                        record's own auth_user
  --password-env <VAR>  read the password from this variable. Otherwise
                        SIMMER_REPLAY_PASSWORD_<USER>, then SIMMER_REPLAY_PASSWORD
  --auth plain|login    default plain

selection and pacing
  --rate <n>            messages per second. Default: as fast as accepted
  --limit <n>           stop after this many
  --pad-buckets <n>     widen the file scan. Default 1
  --dry-run             read, select and report. Connect to nothing

exit codes
  0 everything sent was accepted     3 --confirm absent, nothing sent
  1 something was not accepted       4 a credential could not be resolved
  2 usage error                      6 the capture directory is unreadable
";

/// A parsed invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayArgs {
    pub dir: PathBuf,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    pub ca: String,
    pub tls_name: Option<String>,
    pub helo: Option<String>,
    pub user: Option<String>,
    pub password_env: Option<String>,
    pub auth: AuthMech,
    pub rate: Option<f64>,
    pub limit: Option<usize>,
    pub pad_buckets: i64,
    pub dry_run: bool,
    pub confirm: bool,
}

impl ReplayArgs {
    fn target(&self) -> Target {
        Target {
            host: self.host.clone(),
            port: self.port,
            tls: self.tls,
            tls_name: self.tls_name.clone().unwrap_or_else(|| self.host.clone()),
        }
    }
}

/// What a run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub selected: usize,
    pub sent: usize,
    pub accepted: usize,
    pub deferred: usize,
    pub rejected: usize,
    pub transport_errors: usize,
    pub skipped_no_body: usize,
    pub malformed_lines: usize,
    pub unknown_version: usize,
    pub files_read: usize,
}

impl Summary {
    /// 0 when everything that was sent was accepted, 1 otherwise.
    pub fn exit_code(&self) -> u8 {
        u8::from(self.sent != self.accepted)
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse `args` (already past argv[0]).
///
/// `None` means this is not a replay invocation and the process should carry on
/// starting normally. `Some(Err)` is a usage error. Split from
/// [`check_subcommand`] so the grammar is testable — `check_subcommand` ends in
/// `process::exit`, which a test cannot survive. The
/// `healthcheck::probe_from_args` precedent.
///
/// `now` is a parameter so relative forms are testable against a fixed instant.
pub fn args_from(
    args: impl Iterator<Item = String>,
    now: DateTime<Utc>,
) -> Option<Result<ReplayArgs, String>> {
    let argv: Vec<String> = args.collect();
    if argv.first().map(String::as_str) != Some("replay") {
        return None;
    }
    Some(parse(&argv[1..], now))
}

fn parse(argv: &[String], now: DateTime<Utc>) -> Result<ReplayArgs, String> {
    let mut dir: Option<PathBuf> = None;
    let mut from: Option<DateTime<Utc>> = None;
    let mut to: Option<DateTime<Utc>> = None;
    let mut host: Option<String> = None;
    let mut port = 25u16;
    let mut tls = TlsMode::Off;
    let mut ca = "os".to_string();
    let mut tls_name = None;
    let mut helo = None;
    let mut user = None;
    let mut password_env = None;
    let mut auth = AuthMech::Plain;
    let mut rate = None;
    let mut limit = None;
    let mut pad_buckets = DEFAULT_PAD_BUCKETS;
    let mut dry_run = false;
    let mut confirm = false;

    let mut i = 0usize;
    while i < argv.len() {
        match argv[i].as_str() {
            "--dry-run" => {
                dry_run = true;
                i += 1;
                continue;
            }
            "--confirm" => {
                confirm = true;
                i += 1;
                continue;
            }
            "-h" | "--help" => return Err(String::new()),
            // Named explicitly so the message says why, rather than "unknown
            // argument". A password on argv lands in shell history, `ps` output
            // and the container's recorded command line.
            "--password" | "-p" => {
                return Err(
                    "--password is not accepted: a password on the command line lands in \
                     shell history, `ps` output and the container's recorded command line. \
                     Use --password-env, or SIMMER_REPLAY_PASSWORD"
                        .to_string(),
                )
            }
            _ => {}
        }

        let value = argv
            .get(i + 1)
            .ok_or_else(|| format!("{} needs a value", argv[i]))?;

        match argv[i].as_str() {
            "--dir" => dir = Some(PathBuf::from(value)),
            "--from" => from = Some(parse_when(value, now).map_err(|e| format!("--from: {e}"))?),
            "--to" => to = Some(parse_when(value, now).map_err(|e| format!("--to: {e}"))?),
            "--host" => host = Some(value.clone()),
            "--port" => {
                port = value
                    .parse()
                    .map_err(|_| format!("--port: '{value}' is not a port number"))?
            }
            "--tls" => {
                tls = TlsMode::parse(value)
                    .ok_or_else(|| format!("--tls: '{value}' is not off, starttls or implicit"))?
            }
            "--ca" => ca = value.clone(),
            "--tls-name" => tls_name = Some(value.clone()),
            "--helo" => helo = Some(value.clone()),
            "--user" => user = Some(value.clone()),
            "--password-env" => password_env = Some(value.clone()),
            "--auth" => {
                auth = AuthMech::parse(value)
                    .ok_or_else(|| format!("--auth: '{value}' is not plain or login"))?
            }
            "--rate" => {
                let r: f64 = value
                    .parse()
                    .map_err(|_| format!("--rate: '{value}' is not a number"))?;
                if r <= 0.0 {
                    return Err("--rate must be greater than zero".to_string());
                }
                rate = Some(r);
            }
            "--limit" => {
                limit = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--limit: '{value}' is not a number"))?,
                )
            }
            "--pad-buckets" => {
                pad_buckets = value
                    .parse()
                    .map_err(|_| format!("--pad-buckets: '{value}' is not a number"))?
            }
            other => return Err(format!("unknown argument {other}")),
        }
        i += 2;
    }

    let dir = dir.ok_or("--dir is required")?;
    let from = from.ok_or("--from is required")?;
    let to = to.ok_or("--to is required")?;
    // No default host. A replay that could be run without naming its target is
    // one that can be run at the wrong one by accident.
    let host = host.ok_or("--host is required")?;

    if to <= from {
        return Err(format!(
            "--from ({}) is not before --to ({}); the range is half-open, [from, to)",
            from.to_rfc3339(),
            to.to_rfc3339()
        ));
    }
    if pad_buckets < 0 {
        return Err("--pad-buckets cannot be negative".to_string());
    }

    Ok(ReplayArgs {
        dir,
        from,
        to,
        host,
        port,
        tls,
        ca,
        tls_name,
        helo,
        user,
        password_env,
        auth,
        rate,
        limit,
        pad_buckets,
        dry_run,
        confirm,
    })
}

/// Four accepted forms, all resolving to an instant in UTC.
///
/// A bare local-looking timestamp is read as **UTC**, never as the process's
/// timezone: the filenames are UTC and the records are UTC, and a silently-local
/// `--from` is the kind of bug found only after the replay has been run.
pub fn parse_when(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    // 1. `now-<duration>`, using the config's own duration grammar.
    if let Some(rest) = s.strip_prefix("now-") {
        let d = crate::config::duration::parse(rest)?;
        let d = chrono::Duration::from_std(d).map_err(|_| format!("'{rest}' is too large"))?;
        return Ok(now - d);
    }
    if s == "now" {
        return Ok(now);
    }
    // 2. RFC 3339, with an offset.
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    // 3. A bucket filename, so one can be pasted straight off `ls`.
    if let Some(t) = bucket::parse(s).or_else(|| bucket::parse(&format!("{s}{}", bucket::EXT))) {
        return Ok(t);
    }
    // 4. `YYYY-MM-DDTHH:MM[:SS]` with no offset, read as UTC.
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, format) {
            return Ok(t.and_utc());
        }
    }
    Err(format!(
        "'{s}' is not an RFC 3339 instant (2026-09-20T14:20:00Z), a bucket name \
         (2026-09-20T14.20), or now-<duration>"
    ))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The records in `[from, to)`, in timestamp order, and what could not be read.
///
/// Sorted across every file rather than trusted in file order: the writer files
/// a record under its own timestamp even when it arrives late, so lines within
/// one file are not guaranteed ordered. Ties break on `id` so two runs of the
/// same replay send in the same order, which is what makes a byte-equality
/// comparison between them reproducible.
pub fn read_range(args: &ReplayArgs) -> Result<(Vec<Record>, Summary), String> {
    let pad = chrono::Duration::seconds(args.pad_buckets * bucket::BUCKET_SECS);
    let scan_from = args.from - pad;
    let scan_to = args.to + pad;

    let entries = std::fs::read_dir(&args.dir)
        .map_err(|e| format!("reading the capture directory {}: {e}", args.dir.display()))?;

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Not ours unless the name round-trips, so a README or a .gz in the same
        // directory is skipped rather than parsed and reported as malformed.
        let Some(start) = bucket::parse(name) else {
            continue;
        };
        if bucket::intersects(start, scan_from, scan_to) {
            files.push(entry.path());
        }
    }
    files.sort();

    let mut summary = Summary {
        files_read: files.len(),
        ..Summary::default()
    };
    let mut out = Vec::new();

    for path in &files {
        let text = match std::fs::read(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("replay: reading {}: {e}", path.display());
                continue;
            }
        };
        for line in text.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            match Record::from_line(line) {
                Ok(r) => {
                    // The filename decides which files to open; the record's own
                    // timestamp decides what is in range.
                    if r.at >= args.from && r.at < args.to {
                        out.push(r);
                    }
                }
                Err(ParseError::Version(v)) => {
                    summary.unknown_version += 1;
                    eprintln!(
                        "replay: {}: a record written by schema version {v}; this build \
                         understands {}",
                        path.display(),
                        super::record::VERSION
                    );
                }
                Err(ParseError::Malformed(e)) => {
                    // A file truncated by a crash ends this way, and everything
                    // before the truncation is still perfectly replayable.
                    summary.malformed_lines += 1;
                    eprintln!("replay: {}: {e}", path.display());
                }
            }
        }
    }

    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    if let Some(n) = args.limit {
        out.truncate(n);
    }
    summary.selected = out.len();
    Ok((out, summary))
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Resolve a password for every user in `records`, before anything is sent.
///
/// Up front, deliberately: discovering a missing credential after four thousand
/// of ten thousand messages have been delivered is the failure to avoid, and it
/// is not one you can undo.
pub fn resolve_credentials(
    args: &ReplayArgs,
    records: &[Record],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, Credentials>, String> {
    let mut users: BTreeSet<String> = BTreeSet::new();
    for r in records {
        match (&args.user, &r.auth_user) {
            (Some(u), _) => users.insert(u.clone()),
            (None, Some(u)) => users.insert(u.clone()),
            // An unauthenticated record replays unauthenticated. If the target
            // requires AUTH it answers 530, and that is recorded faithfully
            // rather than papered over.
            (None, None) => false,
        };
    }

    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for user in users {
        let mut tried = Vec::new();
        let mut password = None;
        if let Some(var) = &args.password_env {
            tried.push(var.clone());
            password = lookup(var);
        }
        if password.is_none() {
            let var = per_user_var(&user);
            tried.push(var.clone());
            password = lookup(&var);
        }
        if password.is_none() {
            tried.push(SHARED_PASSWORD_VAR.to_string());
            password = lookup(SHARED_PASSWORD_VAR);
        }
        match password {
            Some(p) => {
                out.insert(
                    user.clone(),
                    Credentials {
                        user,
                        password: p,
                        mech: args.auth,
                    },
                );
            }
            None => missing.push(format!("  {user}: tried {}", tried.join(", "))),
        }
    }

    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "no password for {} user(s), and nothing has been sent:\n{}",
            missing.len(),
            missing.join("\n")
        ))
    }
}

/// `SIMMER_REPLAY_PASSWORD_<USER>`, uppercased with everything outside
/// `[A-Z0-9]` replaced by `_` — so `marketing-eu` becomes
/// `SIMMER_REPLAY_PASSWORD_MARKETING_EU`.
pub fn per_user_var(user: &str) -> String {
    let mut out = String::from(SHARED_PASSWORD_VAR);
    out.push('_');
    for c in user.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Send every record, reporting one JSON object per message on stdout.
pub async fn run(
    args: &ReplayArgs,
    records: &[Record],
    credentials: &BTreeMap<String, Credentials>,
    mut summary: Summary,
) -> Summary {
    let tls = match args.tls {
        TlsMode::Off => None,
        _ => match client::connector(&args.ca) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("replay: {e}");
                return summary;
            }
        },
    };
    let target = args.target();
    let started = tokio::time::Instant::now();

    for (n, record) in records.iter().enumerate() {
        // There is nothing to send, and synthesising a body would send bytes
        // that never arrived — destroying the one property a replay has.
        let Some(body) = decode_body(record, &mut summary) else {
            continue;
        };

        if let Some(rate) = args.rate {
            let due = started + std::time::Duration::from_secs_f64(n as f64 / rate);
            tokio::time::sleep_until(due).await;
        }

        let user = args.user.as_deref().or(record.auth_user.as_deref());
        let creds = user.and_then(|u| credentials.get(u));
        let helo = args.helo.as_deref().unwrap_or(&record.helo);

        let at = std::time::Instant::now();
        let outcome = client::send_one(
            &target,
            tls.as_ref(),
            creds,
            &Submission {
                helo,
                mail_from: record.mail_from.as_deref(),
                rcpt_to: &record.rcpt_to,
                body: &body,
                smtputf8: record.params.smtputf8,
                body_8bitmime: record.params.body_8bitmime,
            },
        )
        .await;

        summary.sent += 1;
        match outcome.code {
            200..=299 => summary.accepted += 1,
            400..=499 => summary.deferred += 1,
            500..=599 => summary.rejected += 1,
            _ => summary.transport_errors += 1,
        }

        println!(
            "{}",
            serde_json::json!({
                "id": record.id,
                "at": record.at.to_rfc3339(),
                "recipient": record.rcpt_to.first(),
                "code": outcome.code,
                "stage": outcome.stage.as_str(),
                "text": outcome.text,
                "latency_ms": at.elapsed().as_secs_f64() * 1000.0,
            })
        );
    }

    summary
}

/// The record's body, counting and reporting the two ways it can be unusable.
fn decode_body(record: &Record, summary: &mut Summary) -> Option<Vec<u8>> {
    match record.body() {
        Ok(Some(b)) => Some(b),
        Ok(None) => {
            summary.skipped_no_body += 1;
            println!(
                "{}",
                serde_json::json!({
                    "id": record.id,
                    "at": record.at.to_rfc3339(),
                    "skipped": "body_omitted",
                })
            );
            None
        }
        Err(e) => {
            summary.malformed_lines += 1;
            eprintln!("replay: {}: {e}", record.id);
            None
        }
    }
}

/// Print the summary, always to stderr so `--jsonl`-style stdout stays clean.
pub fn report(args: &ReplayArgs, s: &Summary) {
    eprintln!(
        "replay: {} → {}  target {}:{}\n  \
         files {}  selected {}  sent {}  accepted {}  deferred {}  rejected {}  \
         transport {}  no-body {}  malformed {}  unknown-version {}",
        args.from.to_rfc3339(),
        args.to.to_rfc3339(),
        args.host,
        args.port,
        s.files_read,
        s.selected,
        s.sent,
        s.accepted,
        s.deferred,
        s.rejected,
        s.transport_errors,
        s.skipped_no_body,
        s.malformed_lines,
        s.unknown_version,
    );
}

/// No-op unless argv[1] is `replay`. Otherwise it runs and exits.
pub async fn check_subcommand() {
    let Some(parsed) = args_from(std::env::args().skip(1), Utc::now()) else {
        return;
    };
    let args = match parsed {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("replay: {e}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    let (records, summary) = match read_range(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("replay: {e}");
            std::process::exit(6);
        }
    };

    if args.dry_run {
        report(&args, &summary);
        eprintln!("replay: --dry-run, so nothing was sent");
        std::process::exit(0);
    }

    // Resolved before the gate below, so `--confirm`'s report is not followed by
    // a credential failure on the next run.
    let credentials = match resolve_credentials(&args, &records, &|v| std::env::var(v).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("replay: {e}");
            std::process::exit(4);
        }
    };

    if !args.confirm {
        report(&args, &summary);
        eprintln!(
            "replay: --confirm was not given, so nothing was sent.\n\
             replay: this would deliver {} message(s) to {}:{}, spending that instance's \
             warm-up quota (SPEC.md §7.4). Add --confirm when you mean it.",
            summary.selected, args.host, args.port
        );
        std::process::exit(3);
    }

    let summary = run(&args, &records, &credentials, summary).await;
    report(&args, &summary);
    std::process::exit(i32::from(summary.exit_code()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-20T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn ok(s: &str) -> ReplayArgs {
        args_from(argv(s).into_iter(), now())
            .expect("a replay invocation")
            .unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn err(s: &str) -> String {
        args_from(argv(s).into_iter(), now())
            .expect("a replay invocation")
            .expect_err("should be a usage error")
    }

    const MIN: &str = "replay --dir /cap --from 2026-09-20T14:00:00Z --to 2026-09-20T15:00:00Z \
                       --host app2";

    #[test]
    fn anything_that_is_not_the_subcommand_is_a_no_op() {
        // The `healthcheck` precedent: no prefix matching.
        for argv0 in ["healthcheck", "replays", "re", "", "hash-password"] {
            assert!(
                args_from(vec![argv0.to_string()].into_iter(), now()).is_none(),
                "{argv0}"
            );
        }
        assert!(args_from(std::iter::empty(), now()).is_none());
    }

    #[test]
    fn the_minimal_invocation_defaults_the_rest() {
        let a = ok(MIN);
        assert_eq!(a.dir, PathBuf::from("/cap"));
        assert_eq!(a.host, "app2");
        assert_eq!(a.port, 25);
        assert_eq!(a.tls, TlsMode::Off);
        assert_eq!(a.ca, "os");
        assert_eq!(a.auth, AuthMech::Plain);
        assert_eq!(a.pad_buckets, 1);
        assert!(!a.dry_run);
        // The gate defaults closed.
        assert!(!a.confirm, "--confirm must never default to true");
    }

    #[test]
    fn every_required_argument_is_required() {
        for (missing, flag) in [
            ("--dir /cap", "--dir"),
            ("--from 2026-09-20T14:00:00Z", "--from"),
            ("--to 2026-09-20T15:00:00Z", "--to"),
            ("--host app2", "--host"),
        ] {
            let without = MIN.replace(missing, "");
            assert!(err(&without).contains(flag), "{flag} should be required");
        }
    }

    #[test]
    fn a_password_on_the_command_line_is_refused_by_name() {
        // Not "unknown argument": the message has to say why.
        let e = err(&format!("{MIN} --password hunter2"));
        assert!(e.contains("shell history"), "{e}");
        assert!(e.contains("--password-env"), "{e}");
    }

    #[test]
    fn an_inverted_or_empty_range_is_a_usage_error() {
        let e = err(
            "replay --dir /cap --from 2026-09-20T15:00:00Z --to 2026-09-20T14:00:00Z \
                     --host a",
        );
        assert!(e.contains("half-open"), "{e}");
        let e = err(
            "replay --dir /cap --from 2026-09-20T15:00:00Z --to 2026-09-20T15:00:00Z \
                     --host a",
        );
        assert!(e.contains("not before"), "{e}");
    }

    #[test]
    fn an_unknown_flag_and_a_flag_with_no_value_are_usage_errors() {
        assert!(err(&format!("{MIN} --turbo")).contains("--turbo"));
        assert!(err(&format!("{MIN} --port")).contains("needs a value"));
    }

    #[test]
    fn bad_enumerations_and_numbers_say_what_was_expected() {
        assert!(err(&format!("{MIN} --tls yes")).contains("starttls"));
        assert!(err(&format!("{MIN} --auth cram-md5")).contains("plain"));
        assert!(err(&format!("{MIN} --port http")).contains("port number"));
        assert!(err(&format!("{MIN} --rate 0")).contains("greater than zero"));
        assert!(err(&format!("{MIN} --pad-buckets -1")).contains("negative"));
    }

    #[test]
    fn the_four_timestamp_forms_all_resolve_to_utc() {
        let want = DateTime::parse_from_rfc3339("2026-09-20T14:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // RFC 3339, with Z and with an offset.
        assert_eq!(parse_when("2026-09-20T14:20:00Z", now()).unwrap(), want);
        assert_eq!(
            parse_when("2026-09-20T15:20:00+01:00", now()).unwrap(),
            want
        );
        // A bucket name, with and without the extension, so one can be pasted
        // straight off `ls`.
        assert_eq!(parse_when("2026-09-20T14.20", now()).unwrap(), want);
        assert_eq!(parse_when("2026-09-20T14.20.jsonl", now()).unwrap(), want);
        // Offset-free, read as UTC and never as local time.
        assert_eq!(parse_when("2026-09-20T14:20:00", now()).unwrap(), want);
        assert_eq!(parse_when("2026-09-20T14:20", now()).unwrap(), want);
        // Relative, against the injected `now`.
        assert_eq!(parse_when("now", now()).unwrap(), now());
        assert_eq!(
            parse_when("now-40m", now()).unwrap(),
            DateTime::parse_from_rfc3339("2026-09-20T14:20:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn an_unparseable_timestamp_says_what_it_accepts() {
        let e = parse_when("yesterday", now()).unwrap_err();
        assert!(e.contains("RFC 3339"), "{e}");
        assert!(e.contains("bucket name"), "{e}");
        // The duration grammar's own rule: a bare integer has no unit.
        assert!(parse_when("now-3600", now()).is_err());
    }

    #[test]
    fn the_per_user_variable_is_the_username_flattened() {
        assert_eq!(per_user_var("cfapp"), "SIMMER_REPLAY_PASSWORD_CFAPP");
        assert_eq!(
            per_user_var("marketing-eu"),
            "SIMMER_REPLAY_PASSWORD_MARKETING_EU"
        );
        assert_eq!(per_user_var("a.b@c"), "SIMMER_REPLAY_PASSWORD_A_B_C");
        assert_eq!(per_user_var(""), "SIMMER_REPLAY_PASSWORD_");
    }

    fn record_for(user: Option<&str>) -> Record {
        Record {
            v: 1,
            id: "id".into(),
            at: now(),
            peer: "127.0.0.1:1".into(),
            helo: "h".into(),
            tls: false,
            auth_user: user.map(str::to_string),
            mail_from: Some("a@b".into()),
            rcpt_to: vec!["c@d".into()],
            subject: String::new(),
            params: super::super::record::Params {
                size: None,
                body_8bitmime: false,
                smtputf8: false,
                auth_identity: None,
            },
            size: 0,
            sha256: String::new(),
            body_omitted: false,
            body_b64: Some(String::new()),
        }
    }

    #[test]
    fn credentials_resolve_per_user_then_shared() {
        let args = ok(MIN);
        let recs = vec![record_for(Some("cfapp")), record_for(Some("other"))];
        let env = |v: &str| match v {
            "SIMMER_REPLAY_PASSWORD_CFAPP" => Some("specific".to_string()),
            "SIMMER_REPLAY_PASSWORD" => Some("shared".to_string()),
            _ => None,
        };
        let out = resolve_credentials(&args, &recs, &env).unwrap();
        assert_eq!(out["cfapp"].password, "specific");
        assert_eq!(
            out["other"].password, "shared",
            "falls back to the shared one"
        );
    }

    #[test]
    fn password_env_overrides_both() {
        let args = ok(&format!("{MIN} --password-env MY_VAR"));
        let recs = vec![record_for(Some("cfapp"))];
        let env = |v: &str| match v {
            "MY_VAR" => Some("explicit".to_string()),
            "SIMMER_REPLAY_PASSWORD_CFAPP" => Some("specific".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_credentials(&args, &recs, &env).unwrap()["cfapp"].password,
            "explicit"
        );
    }

    #[test]
    fn a_missing_credential_fails_before_anything_is_sent_and_names_what_it_tried() {
        let args = ok(MIN);
        let recs = vec![record_for(Some("cfapp"))];
        let e = resolve_credentials(&args, &recs, &|_| None).unwrap_err();
        assert!(e.contains("nothing has been sent"), "{e}");
        assert!(e.contains("SIMMER_REPLAY_PASSWORD_CFAPP"), "{e}");
        assert!(e.contains("SIMMER_REPLAY_PASSWORD"), "{e}");
    }

    #[test]
    fn an_unauthenticated_record_needs_no_credential() {
        let args = ok(MIN);
        let recs = vec![record_for(None)];
        assert!(resolve_credentials(&args, &recs, &|_| None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_user_override_replaces_every_records_own() {
        let args = ok(&format!("{MIN} --user everyone"));
        let recs = vec![record_for(Some("cfapp")), record_for(None)];
        let env = |v: &str| (v == "SIMMER_REPLAY_PASSWORD_EVERYONE").then(|| "p".to_string());
        let out = resolve_credentials(&args, &recs, &env).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("everyone"));
    }

    #[test]
    fn a_summary_exits_nonzero_when_something_was_not_accepted() {
        let clean = Summary {
            sent: 5,
            accepted: 5,
            ..Summary::default()
        };
        assert_eq!(clean.exit_code(), 0);
        let deferred = Summary {
            sent: 5,
            accepted: 4,
            deferred: 1,
            ..Summary::default()
        };
        assert_eq!(deferred.exit_code(), 1);
        // Nothing sent is nothing failed.
        assert_eq!(Summary::default().exit_code(), 0);
    }
}
