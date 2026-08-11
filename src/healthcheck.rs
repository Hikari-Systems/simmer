//! Stdlib-only TCP healthcheck, as a binary subcommand.
//!
//! This is what `HEALTHCHECK CMD ["/app/server", "healthcheck"]` invokes. The
//! §12.2 runtime image is `debian:bookworm-slim` with the binary, CA
//! certificates and a non-root user — no `curl`, no `wget` — so the container's
//! own health probe has to be the binary talking to itself.
//!
//! **Lifted verbatim from `hs_utils::healthcheck` at `v0.31.2`** when that
//! dependency was removed (D-060). The behaviour is deliberately identical: the
//! same CLI surface, the same `/healthcheck` path, the same four-second
//! timeouts, the same `HTTP/1.1 200` prefix test, the same exit codes. Every
//! hikari-systems service's Dockerfile invokes this the same way, and an
//! operator who knows one should not have to learn another. The only change is
//! that the argument parsing is split out so it can be tested — `check_subcommand`
//! itself ends in `process::exit`, which a test cannot survive.
//!
//! Uses only the standard library: no reqwest, no tokio, no extra dependency.
//! That is what makes it safe to call before the async runtime starts, which is
//! where `main` calls it.
//!
//! **Liveness by default, dependencies on request.** The bare form hits
//! `/healthcheck`, which is liveness only. `deps` hits `/healthcheck?deps=true`,
//! which also checks the database. The Dockerfile deliberately uses the bare
//! form: Docker reacts to an unhealthy container by restarting it, and a
//! database outage is not something a restart fixes — making the probe
//! dependency-aware would turn a dependency blip into a restart loop on top of
//! an outage. Operators and load balancers get the full picture from `/health`
//! (§9.2).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long to wait for the local HTTP exchange.
///
/// Four seconds against the Dockerfile's `--timeout=5s`, so the probe answers
/// rather than being killed — a killed probe and a failed one look the same to
/// Docker but only one of them tells you anything.
const IO_TIMEOUT: Duration = Duration::from_secs(4);

/// Open a raw TCP connection to `host:port`, send a minimal HTTP/1.1 GET to
/// `/healthcheck` (or `/healthcheck?deps=true` when `deps` is set), and return
/// `true` if the response starts with `HTTP/1.1 200`.
pub fn run(host: &str, port: u16, deps: bool) -> bool {
    let Ok(mut stream) = TcpStream::connect(format!("{host}:{port}")) else {
        return false;
    };
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let path = if deps {
        "/healthcheck?deps=true"
    } else {
        "/healthcheck"
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }

    response.starts_with("HTTP/1.1 200")
}

/// What a parsed `healthcheck` invocation asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub host: String,
    pub port: u16,
    pub deps: bool,
}

/// Parse the arguments *after* the binary name.
///
/// `None` means this is not a `healthcheck` invocation and the process should
/// carry on starting normally.
///
/// Accepts an optional `[host] [port]` and an optional `deps` / `--deps` token,
/// in any order:
///
/// ```text
/// healthcheck                       # localhost:<default_port>, liveness only
/// healthcheck deps                  # localhost:<default_port>, ?deps=true
/// healthcheck myhost 3000           # myhost:3000, liveness only
/// healthcheck myhost 3000 deps      # myhost:3000, ?deps=true
/// ```
///
/// A port that does not parse falls back to `default_port` rather than failing.
/// That is the inherited behaviour and it is the right one for a health probe:
/// the fallback is the port the service is actually listening on, so a typo in
/// an override still probes something real instead of reporting the container
/// unhealthy for a reason that has nothing to do with its health.
fn probe_from_args(args: impl Iterator<Item = String>, default_port: u16) -> Option<Probe> {
    let mut args = args;
    if args.next().as_deref() != Some("healthcheck") {
        return None;
    }

    // The `deps` flag may appear anywhere after the subcommand; the remaining
    // positional args are `[host] [port]`.
    let mut deps = false;
    let mut positional: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "deps" | "--deps" => deps = true,
            _ => positional.push(arg),
        }
    }

    Some(Probe {
        host: positional
            .first()
            .cloned()
            .unwrap_or_else(|| "localhost".to_string()),
        port: positional
            .get(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(default_port),
        deps,
    })
}

/// Handle the `healthcheck` CLI subcommand and exit if it is present.
///
/// `default_port` is used when the port is absent. Calls
/// `std::process::exit(0)` on success, `exit(1)` on failure.
///
/// This function is a **no-op** when `argv[1] != "healthcheck"`, so it can be
/// called unconditionally at the top of `main`, before the async runtime and
/// before anything that could fail on a broken configuration.
pub fn check_subcommand(default_port: u16) {
    let Some(probe) = probe_from_args(std::env::args().skip(1), default_port) else {
        return;
    };

    std::process::exit(if run(&probe.host, probe.port, probe.deps) {
        0
    } else {
        1
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    fn args(list: &[&str]) -> impl Iterator<Item = String> + use<> {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    // -- argument parsing --------------------------------------------------

    #[test]
    fn anything_that_is_not_the_subcommand_is_a_no_op() {
        // The whole reason this is safe to call unconditionally at the top of
        // `main`: an ordinary start must fall straight through it.
        assert_eq!(probe_from_args(args(&[]), 8080), None);
        assert_eq!(probe_from_args(args(&["serve"]), 8080), None);
        assert_eq!(probe_from_args(args(&["--help"]), 8080), None);
        // Not a prefix match, either.
        assert_eq!(probe_from_args(args(&["healthchecks"]), 8080), None);
    }

    #[test]
    fn the_bare_subcommand_probes_localhost_on_the_default_port() {
        assert_eq!(
            probe_from_args(args(&["healthcheck"]), 8080),
            Some(Probe {
                host: "localhost".into(),
                port: 8080,
                deps: false,
            })
        );
    }

    #[test]
    fn deps_is_opt_in_and_may_appear_anywhere() {
        // Both spellings, and in any position — the inherited surface.
        for form in [
            vec!["healthcheck", "deps"],
            vec!["healthcheck", "--deps"],
            vec!["healthcheck", "deps", "myhost", "3000"],
            vec!["healthcheck", "myhost", "deps", "3000"],
            vec!["healthcheck", "myhost", "3000", "deps"],
        ] {
            let probe = probe_from_args(args(&form), 8080).expect("a healthcheck invocation");
            assert!(probe.deps, "{form:?}");
        }
    }

    #[test]
    fn host_and_port_are_positional_after_the_flag_is_removed() {
        assert_eq!(
            probe_from_args(args(&["healthcheck", "myhost", "3000", "deps"]), 8080),
            Some(Probe {
                host: "myhost".into(),
                port: 3000,
                deps: true,
            })
        );
        // Host alone keeps the default port.
        assert_eq!(
            probe_from_args(args(&["healthcheck", "myhost"]), 8080),
            Some(Probe {
                host: "myhost".into(),
                port: 8080,
                deps: false,
            })
        );
    }

    #[test]
    fn an_unparseable_port_falls_back_rather_than_failing() {
        // Deliberate: the fallback is the port the service actually listens on,
        // so a typo still probes something real instead of reporting the
        // container unhealthy for a reason unrelated to its health.
        assert_eq!(
            probe_from_args(args(&["healthcheck", "myhost", "not-a-port"]), 8080).map(|p| p.port),
            Some(8080)
        );
        assert_eq!(
            probe_from_args(args(&["healthcheck", "myhost", "70000"]), 8080).map(|p| p.port),
            Some(8080),
            "out of u16 range"
        );
    }

    // -- the probe itself --------------------------------------------------

    /// A one-shot HTTP server that answers with `status`, and records the
    /// request line it was given.
    fn serve_once(status: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = std::io::BufReader::new(&stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");

            let mut stream = &stream;
            let _ = stream.write_all(
                format!("{status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            );
            request_line
        });

        (port, handle)
    }

    #[test]
    fn a_200_is_healthy() {
        let (port, server) = serve_once("HTTP/1.1 200 OK");
        assert!(run("127.0.0.1", port, false));
        assert_eq!(server.join().unwrap(), "GET /healthcheck HTTP/1.1\r\n");
    }

    #[test]
    fn deps_changes_the_path_it_asks_for() {
        let (port, server) = serve_once("HTTP/1.1 200 OK");
        assert!(run("127.0.0.1", port, true));
        assert_eq!(
            server.join().unwrap(),
            "GET /healthcheck?deps=true HTTP/1.1\r\n"
        );
    }

    #[test]
    fn anything_that_is_not_a_200_is_unhealthy() {
        // §9.2's degraded answer is a 503, which is the case that matters: with
        // `deps`, an unreachable database has to fail the probe.
        for status in [
            "HTTP/1.1 503 Service Unavailable",
            "HTTP/1.1 500 Internal Server Error",
            "HTTP/1.1 404 Not Found",
            "HTTP/1.1 204 No Content",
        ] {
            let (port, server) = serve_once(status);
            assert!(!run("127.0.0.1", port, false), "{status}");
            let _ = server.join();
        }
    }

    #[test]
    fn nothing_listening_is_unhealthy_rather_than_a_panic() {
        // Binding and dropping guarantees the port is free but unowned.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        assert!(!run("127.0.0.1", port, false));
    }

    #[test]
    fn an_unresolvable_host_is_unhealthy_rather_than_a_panic() {
        assert!(!run("no-such-host.invalid", 8080, false));
    }
}
