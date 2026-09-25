//! `simmer` — an SMTP relay facade that applies a domain reputation warm-up ramp.
//!
//! See `docs/SPEC.md`. The constraint that shapes everything is the cutover
//! invariant (§1.1): Simmer is temporary infrastructure, so its output must
//! always be exactly expressible as application-side configuration, and it must
//! never write permanent state into the systems around it.
//!
//! Everything of substance lives in the library; this is startup wiring and
//! shutdown ordering only.

use std::process::ExitCode;
use std::sync::Arc;

use simmer::{admin, config, db, hash_password, healthcheck, logging, quota, relay, smtp};
use tracing::{error, info, warn};

/// D-078 (finding F15): not glibc's malloc, which kept every AUTH's 19 MiB
/// argon2 block resident in a per-thread arena until the container hit its
/// memory limit.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Where the §4 YAML lives. Overridable so the compose stack can mount an
/// environment-specific file without rebuilding the image.
const DEFAULT_CONFIG_PATH: &str = "simmer.yaml";
const CONFIG_PATH_ENV: &str = "SIMMER_CONFIG";

fn config_path() -> String {
    std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    // No-op unless argv[1] == "hash-password" (D-071). First, and before the
    // config is read: minting a credential must not need a valid config, since
    // the config is what needs the credential.
    hash_password::check_subcommand();

    // No-op unless argv[1] == "replay" (D-085). Before the config is read, like
    // the two above and for the same kind of reason: a replay is told everything
    // on the command line, so it must work from a machine that has a capture
    // directory and no `simmer.yaml` at all. Unlike them it needs the runtime,
    // which `main` already is.
    simmer::capture::replay::check_subcommand().await;

    // No-op unless argv[1] == "healthcheck". Must run before anything else: it is
    // what `HEALTHCHECK CMD ["/app/server","healthcheck"]` invokes, and the
    // runtime image has no curl. Falls back to the §4.1 default admin port when
    // the config cannot be read, so a broken config still fails the healthcheck
    // rather than panicking inside it.
    healthcheck::check_subcommand(
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

    // §9.1 — install the Prometheus recorder before anything that counts. Every
    // `metrics::` call before this point is a no-op against a null recorder
    // (D-021), and there are none: nothing has relayed a message yet. A failure
    // here means a recorder is already installed, which cannot happen in a
    // process with one `main`, so it is reported and the service carries on
    // without an exporter rather than refusing to relay mail over it.
    //
    // D-093: only when `admin.metrics` enables it. Otherwise no recorder is
    // installed, every `metrics::` call stays a no-op, nothing is held for a
    // scrape that will never come, and `/metrics` is not served.
    let metrics_handle = if !config.admin.metrics().enabled {
        info!("admin.metrics is off: no recorder, and /metrics is not served (D-093)");
        None
    } else {
        match simmer::metrics::install(config.admin.metrics().idle_timeout) {
            Ok(handle) => Some(handle),
            Err(e) => {
                warn!(error = %e, "could not install the Prometheus recorder; /metrics will be empty");
                None
            }
        }
    };

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %path,
        routes = config.routes.len(),
        senders = config.senders.len(),
        domain_groups = config.domain_groups.len(),
        listeners = config.server.listeners.len(),
        admin = %config.admin.listen,
        strict_senders = config.strict_senders,
        "starting simmer"
    );

    // Non-fatal conditions worth a human's attention: migration-only headers
    // (§6.6), a future warm-up start (§7.2), recipient templates that force
    // splitting (§6.3), the §14.2 unmatched-sender caveat, and an unset
    // `admin.metrics`, which since v0.7.0 means /metrics is off (D-093).
    for w in config::validate::warnings(&config) {
        warn!(path = %w.path, "{}", w.message);
    }

    // §11: migrations are versioned and applied at startup. Which database is a
    // property of the build, not the config (D-084).
    let backend = db::open(&config.database).await?;
    info!(backend = db::BACKEND, "migrations applied");

    // §7.5: an unreachable database is not a startup failure — it means `451` on
    // every message while the listener stays up. Report it and carry on.
    if backend.store.is_available().await {
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

    // §11 — the storage layer behind its trait: `PgQuotaStore`, or
    // `MssqlQuotaStore` in the `-mssql` build (D-084). The trait is also what
    // lets the §7.4 protocol be reasoned about and tested without a database.
    let quota: Arc<dyn quota::QuotaStore> = Arc::clone(&backend.store);

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

    // §6.7 — "checks run at startup and on an interval". The startup pass is
    // here, before the listener binds, so that a `strict` route is never selected
    // on the strength of an answer nobody has looked up yet.
    //
    // Everything about this is non-blocking by construction: a resolver that
    // cannot even be built is a WARN and an empty registry, and an empty registry
    // blocks nothing (§6.7 — "a DNS blip would otherwise become an outage").
    let preflight = Arc::new(simmer::preflight::Registry::new());
    let preflight_plans = simmer::preflight::plan(&config);
    let mut preflight_resolver = None;
    if preflight_plans.is_empty() {
        info!("no route has a checkable preflight block; DNS preflight not started");
    } else {
        match simmer::preflight::resolver::Hickory::from_system() {
            Ok(r) => {
                let r: Arc<dyn simmer::preflight::resolver::TxtResolver> = Arc::new(r);
                simmer::preflight::check_once(&preflight_plans, r.as_ref(), &preflight).await;
                preflight_resolver = Some(r);
            }
            Err(e) => warn!(
                error = %e,
                "DNS resolver unavailable; preflight will not run. Routes are unaffected \
                 — a route with no preflight result is eligible (§6.7)"
            ),
        }
    }

    // §3.2 step 2, D-100 — MX grouping, only when some group asks for it. A
    // resolver that cannot be built is a WARN, not a refusal: every domain not
    // listed literally then lands in the catch-all, which is what it did before
    // D-100.
    let wants_mx = config.domain_groups.iter().any(|g| !g.mx.is_empty());
    let groups = if wants_mx {
        match simmer::preflight::resolver::Hickory::from_system() {
            Ok(r) => {
                info!("MX domain grouping enabled (D-100)");
                simmer::routing::domain_group::Grouper::new(Arc::new(r))
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "DNS resolver unavailable; MX domain grouping disabled. Domains not \
                     listed literally in a group go to the catch-all (D-100)"
                );
                simmer::routing::domain_group::Grouper::literal()
            }
        }
    } else {
        simmer::routing::domain_group::Grouper::literal()
    };

    // §8.3 — one pool per route, built from the same `Config` the engine holds.
    // Opening nothing yet: a pool is a bound and a set of idle sockets, and there
    // is no reason to dial a downstream before a message needs one.
    let pools = Arc::new(simmer::downstream::Pool::build(&config));

    // D-085 — the optional debugging capture. Started before the engine so the
    // handle can be moved into it, and a failure here refuses to start: a
    // capture that is configured but silently writing nothing is the worst
    // outcome available, because the operator would find out only when they went
    // looking for the records. `config::validate` has already checked the
    // directory, so this is the second line of defence, not the first.
    let (capture, capture_writer) = match &config.capture {
        Some(cfg) => {
            let (handle, task) = simmer::capture::Capture::start(cfg)?;
            (Some(handle), Some(task))
        }
        None => (None, None),
    };

    let engine = relay::Engine {
        config: Arc::clone(&config),
        tls: Arc::new(tls),
        pools: Arc::clone(&pools),
        quota: Arc::clone(&quota),
        registry: quota::ReservationRegistry::new(),
        rewriters: Arc::new(rewriters),
        frequency,
        preflight: Arc::clone(&preflight),
        groups: Arc::new(groups),
        capture: capture.clone(),
    };

    // §5.1 — bind before announcing readiness, so a port clash is a startup
    // failure rather than a service that is up but deaf.
    let smtp = smtp::Listener::bind(engine.clone()).await?;
    let sessions = smtp.sessions();
    for (addr, tls, auth) in smtp.local_addrs()? {
        // The *effective* policy, defaults resolved: a listener that names only
        // an address takes its port's RFC defaults (D-070), and that is exactly
        // the thing an operator needs to see rather than infer.
        info!(%addr, tls = tls.as_str(), auth = auth.as_str(), "SMTP listener bound");
    }

    // §5.1 — "log the not-after date", and warn about what would otherwise be
    // discovered from refused handshakes: an expired certificate, one about to
    // expire, one for a different name. Warnings, not failures — refusing to
    // start would take the plaintext listeners down with the TLS ones.
    if let Some(cert) = smtp.certificate() {
        info!(
            not_after = cert
                .not_after
                .map_or_else(|| "unknown".to_string(), |t| t.to_rfc3339()),
            hostname = %config.server.hostname,
            "TLS certificate loaded"
        );
        for w in smtp::tls::advisories(cert, &config.server.hostname, chrono::Utc::now()) {
            warn!("{w}");
        }
    }

    let admin_state = admin::AdminState {
        // The same engine the SMTP listener has. §9.4's dry run is only worth
        // having if what it reports is what would actually happen, and sharing
        // the engine makes that true by construction.
        engine: engine.clone(),
        metrics: metrics_handle.clone(),
        sessions: Some(Arc::clone(&sessions)),
        db: Some(Arc::clone(&backend.gauge)),
    };
    let admin_listener = tokio::net::TcpListener::bind(&config.admin.listen)
        .await
        .map_err(|e| anyhow::anyhow!("binding admin listener {}: {e}", config.admin.listen))?;
    info!(addr = %config.admin.listen, "admin listener bound");

    // D-083 — the optional link proxy. Bound here, with the other listeners and
    // before readiness, so a port clash is a startup failure. It dials its
    // upstream with the same verifying TLS configuration as a `required_verify`
    // route.
    let link_proxy = match &config.link_proxy {
        Some(cfg) => {
            let listener =
                simmer::link_proxy::Listener::bind(cfg, (*engine.tls.verifying()).clone()).await?;
            info!(
                addr = %listener.local_addr()?,
                upstream = %cfg.upstream,
                "link proxy listener bound"
            );
            Some(listener)
        }
        None => None,
    };

    // §10.4 is two-phase: `stop_accepting` breaks both accept loops, then after
    // the grace period `hard_stop` makes any session still running emit `421`.
    let stop_accepting = smtp::Shutdown::new();
    let hard_stop = smtp::Shutdown::new();

    let smtp_task = tokio::spawn(smtp.serve(stop_accepting.clone(), hard_stop.clone()));
    let mut link_proxy_task =
        link_proxy.map(|l| tokio::spawn(l.serve(stop_accepting.clone(), hard_stop.clone())));

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

    // D-085's retention. Not started when capture is off: nothing writes bucket
    // files then, so there is nothing to evict — the frequency sweeper's
    // precedent, for the same reason.
    let capture_sweeper = config.capture.as_ref().map(|cfg| {
        tokio::spawn(simmer::capture::sweeper::run(
            std::path::PathBuf::from(&cfg.directory),
            cfg.retention,
            stop_accepting.clone(),
        ))
    });

    // §6.7's interval. Not started when nothing is checkable, or when the
    // resolver could not be built — the phase 6 sweeper's precedent.
    let preflight_task = preflight_resolver.map(|resolver| {
        tokio::spawn(simmer::preflight::run(
            preflight_plans,
            resolver,
            Arc::clone(&preflight),
            stop_accepting.clone(),
        ))
    });

    // D-076 (finding F8). Histogram samples accumulate in the exporter until
    // `run_upkeep` drains them, and `install_recorder` starts no task to call it
    // — only a scrape did. An instance nobody scrapes therefore grew with every
    // message. This is the task the exporter's own `install()` would have
    // started; `/metrics` still calls it too, which is harmless.
    let upkeep_task = metrics_handle.map(|handle| {
        let stop = stop_accepting.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(METRICS_UPKEEP);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = every.tick() => handle.run_upkeep(),
                }
            }
        })
    });

    // D-092 — the soak tier's allocator counters. Off unless the build has
    // `alloc-stats` AND the environment names a file; neither is true of any
    // published image.
    let alloc_stats_task = match simmer::alloc_stats::configured() {
        Some(path) if simmer::alloc_stats::compiled_in() => {
            info!(path = %path.display(), "writing allocator stats (D-092)");
            Some(tokio::spawn(simmer::alloc_stats::run(
                path,
                stop_accepting.clone(),
            )))
        }
        Some(path) => {
            warn!(
                path = %path.display(),
                "{} is set but this build has no alloc-stats feature; nothing will be written (D-092)",
                simmer::alloc_stats::ENV
            );
            None
        }
        None => None,
    };

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
    //
    // D-083: the link proxy's requests in flight get the same grace. Its task
    // returns once they have, so waiting on it is that condition.
    let drained = async {
        let _ = sessions.acquire_many(max).await;
        if let Some(task) = link_proxy_task.take() {
            let _ = task.await;
        }
    };
    match tokio::time::timeout(SHUTDOWN_GRACE, drained).await {
        Ok(()) => info!("all sessions drained"),
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

    // §10.4 — "drain pools". Last of the four clauses, and in this order for a
    // reason: a connection still checked out by a session that is finishing is
    // not idle, so draining before the grace period would leave exactly the
    // connections a drain is for.
    pools.drain().await;

    let _ = smtp_task.await;
    if let Some(task) = link_proxy_task {
        let _ = task.await;
    }
    let _ = sweeper.await;
    if let Some(task) = frequency_sweeper {
        let _ = task.await;
    }
    if let Some(task) = preflight_task {
        let _ = task.await;
    }
    if let Some(task) = upkeep_task {
        let _ = task.await;
    }
    if let Some(task) = capture_sweeper {
        let _ = task.await;
    }
    if let Some(task) = alloc_stats_task {
        let _ = task.await;
    }
    let _ = admin_task.await;

    // D-085 — the capture writer outlives every session on purpose: a session
    // finishing inside the §10.4 grace period still offers a record, and a
    // capture with a hole in exactly the messages that were in flight at
    // shutdown is worse than no capture.
    //
    // It listens to no shutdown token. Its channel closes when the last
    // `Capture` handle is dropped, and "no handle exists" is precisely "no
    // session can offer another record" — a condition the type system already
    // tracks. So drop the two this process holds, in this order, and wait.
    drop(engine);
    drop(capture);
    if let Some(task) = capture_writer {
        match tokio::time::timeout(CAPTURE_FLUSH_GRACE, task).await {
            Ok(_) => info!("capture writer flushed"),
            Err(_) => warn!(
                secs = CAPTURE_FLUSH_GRACE.as_secs(),
                "the capture writer did not finish flushing; the last records may be missing"
            ),
        }
    }

    info!("shutdown complete");
    Ok(())
}

/// §10.4 — "allow in-flight sessions to complete up to a grace period (default
/// 30s)". Not in the §4.1 schema, so not configurable.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// D-085 — how long to wait for the capture writer to flush after every session
/// has ended. A backstop against a task wedged on a hung filesystem, not a
/// tuning knob; not in the §4.1 schema, like `SHUTDOWN_GRACE`.
const CAPTURE_FLUSH_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// D-076 — how often the metrics exporter's buffered samples are drained. The
/// exporter's own `install()` default.
const METRICS_UPKEEP: std::time::Duration = std::time::Duration::from_secs(5);

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
