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

pub struct Stack {
    /// `--profile` values.
    profiles: &'static [&'static str],
    /// Override files layered on `docker-compose.yml`, in order. Profiles cannot
    /// change `app` (limits, ulimits, config mounts, a second instance); these can.
    overrides: &'static [&'static str],
    /// `SIMMER_CONFIG` as `app` sees it.
    config: &'static str,
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
        Stack {
            profiles,
            overrides,
            config,
            warmup_started: Mutex::new(String::new()),
        }
    }

    /// `docker compose` with this stack's files, profiles and environment.
    pub fn compose(&self) -> Command {
        let mut c = Command::new("docker");
        c.arg("compose");
        if !self.overrides.is_empty() {
            c.args(["-f", "docker-compose.yml"]);
            for file in self.overrides {
                c.args(["-f", file]);
            }
        }
        for profile in self.profiles {
            c.args(["--profile", profile]);
        }
        c.env("SIMMER_CONFIG", self.config);
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

    /// One SQL statement against the stack's Postgres, as unaligned tuples-only
    /// text (`psql -At`) — a single value comes back as that value.
    pub fn psql(&self, sql: &str) -> String {
        let out = self.run(&[
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Truncate the quota tables.
    ///
    /// Quota state lives in Postgres and outlives a container restart by design
    /// (§7.4), so a test that re-uses a simulated day another test has already
    /// spent finds the allowance gone and watches every message fall through to
    /// overflow — which looks precisely like a routing bug.
    pub fn reset_quota(&self) {
        self.psql("truncate quota_usage, quota_reservation, route_state;");
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
