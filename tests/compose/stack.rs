//! One compose stack, and every command run against it.
//!
//! **Every command goes through [`Stack::compose`]**, and that is the point of the
//! type. Compose re-renders the whole file on every invocation, and a command that
//! renders `app` differently from the running container recreates it. So a
//! command that forgets an override file, the config path or the warm-up instant
//! does not fail — it silently restarts `app` as something else, and the test goes
//! on to measure the wrong thing while appearing to pass. D-042 recorded that trap
//! for the acceptance suite's `SIMMER_WARMUP_STARTED`; with override files per tier
//! it has more ways to spring, so the only way to build a command is this one.

use std::process::{Command, Output};
use std::sync::Mutex;

/// Which storage backend the stack's `app` is built against (D-084).
///
/// It changes two things here and nothing else: which container holds the
/// database, and which dialect a statement against it is written in. Everything
/// a tier asks of a stack — the instances, the sink, the loadgen — is the same
/// either way, which is the point of `QuotaStore`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    Mssql,
}

/// D-085's capture overlay: the per-instance volumes and `SIMMER_CAPTURE_DIR`.
/// Layered onto any stack when `SIMMER_CAPTURE=on`.
const CAPTURE_OVERRIDE: &str = "test/compose/capture.yml";

/// Is the capture on for this run? Deliberately not named for any tier: the same
/// variable turns it on for the stress stack, the soak, or anything else that
/// reads its config from the config volume.
pub fn capture_on() -> bool {
    std::env::var("SIMMER_CAPTURE").is_ok_and(|v| v == "on")
}

/// The SA login the `mssql` stacks use. Local development only — never a real
/// credential; `docker-compose.yml`'s `simmer-mssql-db` carries the same literal.
const MSSQL_SA_PASSWORD: &str = "Simmer-dev-1!";

pub struct Stack {
    /// `--profile` values.
    profiles: &'static [&'static str],
    /// Override files layered on `docker-compose.yml`, in order. Profiles cannot
    /// change `app` (limits, ulimits, config mounts, a second instance); these can.
    overrides: &'static [&'static str],
    /// `SIMMER_CONFIG` as `app` sees it.
    config: &'static str,
    /// The database behind it, and so the dialect [`Stack::sql`] speaks.
    backend: Backend,
    /// The `warmup.started` `app` is running with, once a test has chosen one.
    warmup_started: Mutex<String>,
}

/// The §12.3 acceptance stack (`docs/ACCEPTANCE.md`), with its override: the
/// test CA in `app`'s and `loadgen`'s OS trust store.
pub static ACCEPTANCE: Stack = Stack::new(
    &["acceptance"],
    &["test/compose/acceptance.yml"],
    "/app/simmer.acceptance.yaml",
);

/// The T2 server matrix (`test/compose/matrix.yml`): the acceptance stack plus
/// five Postfix variants and a Mailpit trap they all deliver to, with `app`
/// running `test/config/simmer.matrix.yaml`.
pub static MATRIX: Stack = Stack::new(
    &["acceptance", "matrix"],
    &["test/compose/acceptance.yml", "test/compose/matrix.yml"],
    "/config/simmer.matrix.yaml",
);

/// The T3 stress stack (`test/compose/stress.yml`): the acceptance stack with
/// `app` at 2 CPUs and 1 GiB running `test/config/simmer.stress.yaml`, and the
/// counting `sink` as both routes' downstream.
pub static STRESS: Stack = Stack::new(
    &["acceptance", "stress"],
    &["test/compose/acceptance.yml", "test/compose/stress.yml"],
    "/config/simmer.stress.yaml",
);

/// The T4 soak stack (`docs/TESTING.md`, step 5).
///
/// The same files, profiles and services as [`STRESS`] — `app`, `app2`, the
/// counting sink and the results volume are all exactly what a soak needs — and
/// differs only in which config `app` reads. `test/config/Dockerfile` copies the
/// whole directory into the shared volume, so `simmer.soak.yaml` is already
/// mounted; no separate compose file is needed until the soak wants a service the
/// stress tier does not have.
pub static SOAK: Stack = Stack::new(
    &["acceptance", "stress"],
    &["test/compose/acceptance.yml", "test/compose/stress.yml"],
    "/config/simmer.soak.yaml",
);

/// The soak stack against the `mssql` build and SQL Server Express
/// (`test/compose/mssql.yml`).
pub static SOAK_MSSQL: Stack = Stack::with_backend(
    &["acceptance", "stress", "mssql"],
    &[
        "test/compose/acceptance.yml",
        "test/compose/stress.yml",
        "test/compose/mssql.yml",
    ],
    "/config/simmer.soak.yaml",
    Backend::Mssql,
);

/// The same stack *without* its override, so without the test CA in any OS
/// trust store — for negative controls only, and only with `run --no-deps`. A
/// command that reconciled `app` through this stack would recreate it untrusted
/// and every later test would measure the wrong thing.
pub static ACCEPTANCE_UNTRUSTED: Stack =
    Stack::new(&["acceptance"], &[], "/app/simmer.acceptance.yaml");

impl Stack {
    pub const fn new(
        profiles: &'static [&'static str],
        overrides: &'static [&'static str],
        config: &'static str,
    ) -> Stack {
        Stack::with_backend(profiles, overrides, config, Backend::Postgres)
    }

    /// As [`Stack::new`], naming the backend. Only the `mssql` stacks need it;
    /// everything else is Postgres, which is what the default build is.
    pub const fn with_backend(
        profiles: &'static [&'static str],
        overrides: &'static [&'static str],
        config: &'static str,
        backend: Backend,
    ) -> Stack {
        Stack {
            profiles,
            overrides,
            config,
            backend,
            warmup_started: Mutex::new(String::new()),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// `SIMMER_CONFIG` for this run: the tier's config, or its capture twin.
    ///
    /// The twins are generated into the config volume by `test/config/Dockerfile`
    /// from one `capture.block.yaml`, so there is nothing here to keep in step.
    /// A config served from the image rather than that volume has no twin, and
    /// saying so is much better than running without the capture that was asked
    /// for — D-085's rule that a capture configured and silently not writing is
    /// the worst outcome available, applied to the harness.
    fn config_path(&self) -> String {
        if !capture_on() {
            return self.config.to_string();
        }
        assert!(
            self.config.starts_with("/config/"),
            "SIMMER_CAPTURE=on, but this stack reads {} from the image rather than \
             the config volume, so it has no generated capture twin. Only the tiers \
             served by test/config/Dockerfile can be captured this way.",
            self.config
        );
        self.config
            .strip_suffix(".yaml")
            .map(|base| format!("{base}.capture.yaml"))
            .expect("a tier config ends in .yaml")
    }

    /// `docker compose` with this stack's files, profiles and environment.
    ///
    /// D-085's capture is layered on here rather than being a stack of its own,
    /// because it belongs to no tier: `SIMMER_CAPTURE=on` adds the overlay, its
    /// profile and the config twin to **whatever** stack is running. That is why
    /// there is no `SOAK_CAPTURE` and no capture variant of each `Stack` — the
    /// capture is a property of a run, not a kind of stack.
    pub fn compose(&self) -> Command {
        let mut c = Command::new("docker");
        c.arg("compose");
        if !self.overrides.is_empty() {
            c.args(["-f", "docker-compose.yml"]);
            for file in self.overrides {
                c.args(["-f", file]);
            }
            if capture_on() {
                c.args(["-f", CAPTURE_OVERRIDE]);
            }
        }
        for profile in self.profiles {
            c.args(["--profile", profile]);
        }
        if capture_on() {
            c.args(["--profile", "capture"]);
        }
        c.env("SIMMER_CONFIG", self.config_path());
        let started = self.warmup_started.lock().expect("lock").clone();
        if !started.is_empty() {
            c.env("SIMMER_WARMUP_STARTED", started);
        }
        c
    }

    /// Run a compose subcommand and return its output, failing on a non-zero exit.
    pub fn run(&self, args: &[&str]) -> Output {
        let out = self.compose().args(args).output().expect("docker compose");
        assert!(
            out.status.success(),
            "docker compose {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// Move `warmup.started` back by `day` days and re-create `app`.
    ///
    /// The `- 1h` is not decoration: landing exactly on a day boundary makes the
    /// test a race against its own clock.
    pub fn restart_app_at_day(&self, day: usize) {
        let started =
            chrono::Utc::now() - chrono::Duration::days(day as i64) - chrono::Duration::hours(1);
        *self.warmup_started.lock().expect("lock") =
            started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let status = self
            .compose()
            .args(["up", "-d", "--force-recreate", "--wait", "app"])
            .status()
            .expect("docker compose up");
        assert!(status.success(), "failed to restart app at day {day}");
    }

    /// One SQL statement against the stack's database, as a command ready to run
    /// but **not** run.
    ///
    /// Separate from [`Stack::sql`] because a sampler that ran every thirty
    /// seconds for an hour must not panic on one slow query: the soak's ledger
    /// sampler takes the command, runs it itself, and treats a failure as a
    /// sample it did not get rather than as the end of the run.
    ///
    /// Both dialects are asked for one unadorned row with `|` between the
    /// columns, so a caller parses one shape whichever backend it is talking to:
    /// `psql -At` is that by definition, and `sqlcmd` is talked into it with
    /// `-h -1 -W -s '|'` and a `SET NOCOUNT ON` that suppresses the trailing
    /// "(1 rows affected)".
    pub fn sql_command(&self, sql: &str) -> Command {
        let mut c = self.compose();
        match self.backend {
            Backend::Postgres => {
                c.args([
                    "exec",
                    "-T",
                    "simmer-db",
                    "psql",
                    "-U",
                    "simmer",
                    "-d",
                    "simmer",
                    "-q",
                    "-At",
                    "-c",
                    sql,
                ]);
            }
            Backend::Mssql => {
                // `-C` trusts the container's self-signed certificate, as every
                // other connection to it does; `-b` makes a T-SQL error a
                // non-zero exit, which is what `run`'s assertion reads.
                let batch = format!("SET NOCOUNT ON; {sql}");
                c.args([
                    "exec",
                    "-T",
                    "simmer-mssql-db",
                    "/opt/mssql-tools18/bin/sqlcmd",
                    "-C",
                    "-S",
                    "localhost",
                    "-U",
                    "sa",
                    "-P",
                    MSSQL_SA_PASSWORD,
                    "-d",
                    "simmer",
                    "-b",
                    "-h",
                    "-1",
                    "-W",
                    "-s",
                    "|",
                    "-Q",
                    &batch,
                ]);
            }
        }
        c
    }

    /// One SQL statement against the stack's database, as one line of `|`
    /// separated values — a single value comes back as that value.
    ///
    /// Panics on a non-zero exit, like every other command here.
    pub fn sql(&self, sql: &str) -> String {
        let out = self.sql_command(sql).output().expect("docker compose exec");
        assert!(
            out.status.success(),
            "sql failed:\n{sql}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// One SQL statement against the stack's Postgres. Postgres-only by name and
    /// by dialect; the tiers that call it are all Postgres. [`Stack::sql`] is the
    /// backend-agnostic one.
    pub fn psql(&self, sql: &str) -> String {
        assert!(
            self.backend == Backend::Postgres,
            "psql on a stack whose backend is not Postgres: use sql()"
        );
        self.sql(sql)
    }

    /// Truncate the quota tables.
    ///
    /// Quota state lives in the database and outlives a container restart by
    /// design (§7.4), so a test that re-uses a simulated day another test has
    /// already spent finds the allowance gone and watches every message fall
    /// through to overflow — which looks precisely like a routing bug.
    ///
    /// T-SQL has no multi-table `TRUNCATE`, and `TRUNCATE` there is per statement;
    /// the tables carry no foreign keys either way, so the order is free.
    pub fn reset_quota(&self) {
        match self.backend {
            Backend::Postgres => {
                self.sql("truncate quota_usage, quota_reservation, route_state;");
            }
            Backend::Mssql => {
                self.sql(
                    "TRUNCATE TABLE dbo.quota_usage; \
                     TRUNCATE TABLE dbo.quota_reservation; \
                     TRUNCATE TABLE dbo.route_state;",
                );
            }
        }
    }

    /// Every service's log, for a failure report.
    pub fn logs(&self) -> String {
        let out = self
            .compose()
            .args(["logs", "--no-color", "--timestamps"])
            .output()
            .expect("docker compose logs");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// A guard that prints [`Stack::logs`] if the test holding it panics.
    ///
    /// A compose test that fails with only its own assertion message has thrown
    /// away the evidence: the reason a message was deferred is in `app`'s log,
    /// not in the reply code.
    pub fn logs_on_failure(&self) -> LogsOnFailure<'_> {
        LogsOnFailure(self)
    }
}

pub struct LogsOnFailure<'a>(&'a Stack);

impl Drop for LogsOnFailure<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "---- compose logs (test failed) ----\n{}\n---- end compose logs ----",
                self.0.logs()
            );
        }
    }
}
