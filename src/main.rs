//! `simmer` — an SMTP relay facade that applies a domain reputation warm-up ramp.
//!
//! See `docs/SPEC.md`. The constraint that shapes everything is the cutover
//! invariant (§1.1): Simmer is temporary infrastructure, so its output must
//! always be exactly expressible as application-side configuration, and it must
//! never write permanent state into the systems around it.
//!
//! Phase 1 (§13.1) — config loading, full validation, structured logging,
//! container skeleton. No SMTP listener, no rewriting, no quota.
//!
//! Everything of substance lives in the library; this is startup wiring and
//! shutdown ordering only.

use std::process::ExitCode;
use std::sync::Arc;

use simmer::{admin, config, db, logging};
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
        single_recipient_only = config.server.single_recipient_only,
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

    let admin_state = admin::AdminState {
        config: Arc::clone(&config),
        pool: pool.clone(),
    };
    let listener = tokio::net::TcpListener::bind(&config.admin.listen)
        .await
        .map_err(|e| anyhow::anyhow!("binding admin listener {}: {e}", config.admin.listen))?;
    info!(addr = %config.admin.listen, "admin listener bound");

    // §10.4: on SIGTERM, stop accepting and shut down cleanly. Phase 1 has only
    // the admin server to drain; the SMTP listener, sweepers and pool draining
    // join this the same way in later phases, which is why `main` owns the tasks
    // rather than handing its lifetime to a server helper.
    axum::serve(listener, admin::router(admin_state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!("admin server: {e}"))?;

    info!("shutdown complete");
    Ok(())
}

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
