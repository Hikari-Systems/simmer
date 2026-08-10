//! `simmer` — an SMTP relay facade that applies a domain reputation warm-up ramp.
//!
//! See `docs/SPEC.md`. The constraint that shapes everything is the cutover
//! invariant (§1.1): Simmer is temporary infrastructure, so its output must
//! always be exactly expressible as application-side configuration, and it must
//! never write permanent state into the systems around it.
//!
//! Phases 1–3 (§13). No rewriting yet: a message is forwarded byte for byte,
//! under the identity it arrived with.
//!
//! Everything of substance lives in the library; this is startup wiring and
//! shutdown ordering only.

use std::process::ExitCode;
use std::sync::Arc;

use simmer::{admin, config, db, logging, quota, relay, smtp};
use tracing::{error, info, warn};

/// Where the §4 YAML lives. Overridable so the compose stack can mount an
/// environment-specific file without rebuilding the image.
const DEFAULT_CONFIG_PATH: &str = "simmer.yaml";
const CONFIG_PATH_ENV: &str = "SIMMER_CONFIG";

fn config_path() -> String {
    std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    // No-op unless argv[1] == "healthcheck". Must run before anything else: it is
    // what `HEALTHCHECK CMD ["/app/server","healthcheck"]` invokes, and the
    // runtime image has no curl. Falls back to the §4.1 default admin port when
    // the config cannot be read, so a broken config still fails the healthcheck
    // rather than panicking inside it.
    hs_utils::healthcheck::check_subcommand(
        config::load(config_path())
            .ok()
            .and_then(|c| c.admin.listen.parse::<std::net::SocketAddr>().ok())
            .map_or(8080, |a| a.port()),
    );

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Startup failures are reported before the subscriber is guaranteed
            // to exist, and a validation report is for a human at a terminal, so
            // it goes to stderr in full rather than as a JSON log line.
            eprintln!("simmer: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let path = config_path();

    // §4.2: the process refuses to start on any violation, and reports all of
    // them. `config::load` is the only way to obtain a `Config`, so this cannot
    // be skipped later.
    let config = Arc::new(config::load(&path)?);

    logging::init(&config.logging.level, config.logging.format);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %path,
        routes = config.routes.len(),
        senders = config.senders.len(),
        domain_groups = config.domain_groups.len(),
        listen = %config.server.listen,
        admin = %config.admin.listen,
        strict_senders = config.strict_senders,
        "starting simmer"
    );

    // Non-fatal conditions worth a human's attention: migration-only headers
    // (§6.6), a future warm-up start (§7.2), recipient templates that force
    // splitting (§6.3), and the §14.2 unmatched-sender caveat.
    for w in config::validate::warnings(&config) {
        warn!(path = %w.path, "{}", w.message);
    }

    let pool = db::build_pool(&config.database)?;

    // §11: migrations are versioned and applied at startup.
    db::migrate(&pool).await?;
    info!("migrations applied");

    // §7.5: an unreachable database is not a startup failure — it means `451` on
    // every message while the listener stays up. Report it and carry on.
    if db::is_reachable(&pool).await {
        info!("database reachable");
    } else {
        warn!(
            fail_closed = config.database.fail_closed,
            "database is not reachable at startup; messages will be answered 451 until it is"
        );
    }

    // §8.2 — built once and shared. Loading the platform root store is a
    // filesystem walk and building a ClientConfig derives key schedules; doing
    // either per message would put both on the latency path of every relay.
    let (tls, roots) = simmer::downstream::TlsConfigs::load()?;
    if roots == 0 {
        // Every `required_verify` route will fail. Discovering that from a 451
        // storm rather than from this line is a bad afternoon.
        warn!("no platform root certificates loaded; tls: required_verify routes will fail");
    } else {
        info!(roots, "platform root certificates loaded");
    }

    // §11 — the storage layer behind its trait. `PgQuotaStore` is the one
    // implementation; the trait exists so the §7.4 protocol can be reasoned
    // about and tested without a database in the way.
    let quota: Arc<dyn quota::QuotaStore> = Arc::new(quota::PgQuotaStore::new(pool.clone()));

    // §6 — templates compiled once. Infallible here: `config::load` has already
    // run §4.2, which compiles every one of them to check §6.6's property, so a
    // failure at this point would mean validation and the relay disagree about
    // what the configuration says.
    let rewriters = simmer::rewrite::Rewriters::compile(&config)
        .map_err(|errors| anyhow::anyhow!("rewrite templates failed to compile: {errors:?}"))?;

    // §7.3 — the recipient-hash salt. Resolved lazily, because §7.5 says an
    // unreachable database keeps the listener up rather than stopping the
    // process; this is only a best-effort warm so that the first message does not
    // pay for it and so an operator can see it happened.
    let frequency = Arc::new(simmer::frequency::Frequency::new());
    if simmer::frequency::any_configured(&config) {
        match frequency.keyer(quota.as_ref()).await {
            Ok(_) => info!("recipient-frequency salt loaded"),
            Err(e) => warn!(
                error = %e,
                "recipient-frequency salt could not be loaded yet; it will be resolved \
                 on the first message that needs it"
            ),
        }
    }

    let engine = relay::Engine {
        config: Arc::clone(&config),
        tls: Arc::new(tls),
        quota: Arc::clone(&quota),
        registry: quota::ReservationRegistry::new(),
        rewriters: Arc::new(rewriters),
        frequency,
    };

    // §5.1 — bind before announcing readiness, so a port clash is a startup
    // failure rather than a service that is up but deaf.
    let smtp = smtp::Listener::bind(engine.clone()).await?;
    let sessions = smtp.sessions();
    info!(addr = %smtp.local_addr()?, "SMTP listener bound");

    let admin_state = admin::AdminState {
        config: Arc::clone(&config),
        pool: pool.clone(),
    };
    let admin_listener = tokio::net::TcpListener::bind(&config.admin.listen)
        .await
        .map_err(|e| anyhow::anyhow!("binding admin listener {}: {e}", config.admin.listen))?;
    info!(addr = %config.admin.listen, "admin listener bound");

    // §10.4 is two-phase: `stop_accepting` breaks both accept loops, then after
    // the grace period `hard_stop` makes any session still running emit `421`.
    let stop_accepting = smtp::Shutdown::new();
    let hard_stop = smtp::Shutdown::new();

    let smtp_task = tokio::spawn(smtp.serve(stop_accepting.clone(), hard_stop.clone()));

    // §7.4 — release reservations stranded by a crash mid-send. In steady state
    // it should sweep nothing; a nonzero rate is the signal §7.4 asks for.
    let sweeper = tokio::spawn(quota::sweeper::run(
        Arc::clone(&quota),
        stop_accepting.clone(),
    ));

    // §7.3 — "a sweeper evicts rows older than the longest configured window plus
    // a margin, on an interval". Not started at all when no route declares a
    // constraint: nothing writes `recipient_event` then, so there is nothing to
    // evict and no reason to wake up hourly to discover that.
    let frequency_sweeper = simmer::frequency::retention(&config).map(|retention| {
        tokio::spawn(simmer::frequency::sweeper::run(
            Arc::clone(&quota),
            retention,
            stop_accepting.clone(),
        ))
    });

    let admin_task = {
        let stop = stop_accepting.clone();
        tokio::spawn(async move {
            axum::serve(admin_listener, admin::router(admin_state))
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await
        })
    };

    shutdown_signal().await;
    stop_accepting.cancel();

    // §10.4: "allow in-flight sessions to complete up to a grace period (default
    // 30s)". Acquiring every session permit is exactly the condition "no session
    // is in flight", so there is nothing else to track.
    let max = u32::try_from(config.server.max_concurrent_sessions).unwrap_or(u32::MAX);
    match tokio::time::timeout(SHUTDOWN_GRACE, sessions.acquire_many(max)).await {
        Ok(_) => info!("all sessions drained"),
        Err(_) => warn!(
            grace_secs = SHUTDOWN_GRACE.as_secs(),
            "grace period expired with sessions in flight; sending 421"
        ),
    }
    hard_stop.cancel();

    // §10.4 — "release any reservations still outstanding". Sessions cut off by
    // the grace period never reached their own commit-or-release, so this is the
    // only thing that gives their headroom back before `expires_at`.
    relay::release_outstanding(&engine).await;

    let _ = smtp_task.await;
    let _ = sweeper.await;
    if let Some(task) = frequency_sweeper {
        let _ = task.await;
    }
    let _ = admin_task.await;

    info!("shutdown complete");
    Ok(())
}

/// §10.4 — "allow in-flight sessions to complete up to a grace period (default
/// 30s)". Not in the §4.1 schema, so not configurable.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            error!("cannot listen for SIGTERM: {e}");
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = term.recv() => info!("SIGTERM received, shutting down"),
    }
}
