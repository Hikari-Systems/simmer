//! D-083 — the link proxy end to end: a real listener, a real `axum-reverse-proxy`
//! client, and an upstream that records the exact bytes it received.
//!
//! The client and the upstream both speak raw HTTP over TCP rather than through
//! an HTTP library, so what is asserted is what was on the wire — including the
//! requests a library would refuse to send (an HTTP/2 preface, `CONNECT`, an
//! absolute-form target, a header read that never finishes).

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use simmer::config::LinkProxy;
use simmer::link_proxy::Listener;
use simmer::smtp::Shutdown;
use support::TestPki;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// The upstream
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Behaviour {
    /// Write these bytes and close.
    Respond(String),
    /// Read the request, never answer.
    Stall,
    /// Write these bytes, then hold the connection open without finishing.
    PartialThenStall(String),
}

struct Upstream {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    async fn start(behaviour: Behaviour, tls: Option<&TestPki>) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let acceptor = tls.map(|p| p.acceptor());
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let behaviour = behaviour.clone();
                let recorded = Arc::clone(&recorded);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(a) => {
                            if let Ok(s) = a.accept(stream).await {
                                serve_upstream(s, behaviour, recorded).await;
                            }
                        }
                        None => serve_upstream(stream, behaviour, recorded).await,
                    }
                });
            }
        });
        Upstream { addr, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve_upstream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    behaviour: Behaviour,
    recorded: Arc<Mutex<Vec<String>>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    recorded.lock().unwrap().push(request);
    match behaviour {
        Behaviour::Respond(bytes) => {
            let _ = stream.write_all(bytes.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        Behaviour::Stall => tokio::time::sleep(Duration::from_secs(60)).await,
        Behaviour::PartialThenStall(bytes) => {
            let _ = stream.write_all(bytes.as_bytes()).await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

/// Headers, then a `Content-Length` or chunked body.
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let header = |name: &str| {
        head.lines().find_map(|l| {
            l.strip_prefix(&format!("{name}:"))
                .map(|v| v.trim().to_string())
        })
    };
    if let Some(len) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.ok()?;
        buf.extend(body);
    } else if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        while !buf.ends_with(b"0\r\n\r\n") {
            if stream.read(&mut byte).await.ok()? == 0 {
                break;
            }
            buf.push(byte[0]);
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn ok_response(extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ---------------------------------------------------------------------------
// The proxy and the client
// ---------------------------------------------------------------------------

struct Proxy {
    addr: SocketAddr,
    stop_accepting: Shutdown,
    hard_stop: Shutdown,
    task: tokio::task::JoinHandle<()>,
}

fn config(upstream: &str, extra: &str) -> LinkProxy {
    let yaml = format!(
        "listen: \"127.0.0.1:0\"\n\
         upstream: \"{upstream}\"\n\
         allowed_cidrs: [\"127.0.0.0/8\"]\n\
         {extra}"
    );
    serde_yaml_ng::from_str(&yaml).expect("link_proxy yaml")
}

async fn start_proxy(cfg: LinkProxy, tls: rustls::ClientConfig) -> Proxy {
    let listener = Listener::bind(&cfg, tls).await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let stop_accepting = Shutdown::new();
    let hard_stop = Shutdown::new();
    let task = tokio::spawn(listener.serve(stop_accepting.clone(), hard_stop.clone()));
    Proxy {
        addr,
        stop_accepting,
        hard_stop,
        task,
    }
}

/// A client config that trusts nothing: for tests whose upstream is plain HTTP.
fn no_roots() -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth()
}

async fn plain(behaviour: Behaviour, upstream_path: &str, extra: &str) -> (Upstream, Proxy) {
    let upstream = Upstream::start(behaviour, None).await;
    let proxy = start_proxy(
        config(&format!("http://{}{upstream_path}", upstream.addr), extra),
        no_roots(),
    )
    .await;
    (upstream, proxy)
}

/// Send raw bytes and read until the proxy closes, or `within` elapses.
async fn exchange(addr: SocketAddr, request: &[u8], within: Duration) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn get(addr: SocketAddr, request: &str) -> String {
    exchange(addr, request.as_bytes(), Duration::from_secs(10)).await
}

fn status(response: &str) -> &str {
    response.split(' ').nth(1).unwrap_or("")
}

fn header<'a>(response: &'a str, name: &str) -> Vec<&'a str> {
    let head = response.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
        .collect()
}

fn request_line(request: &str) -> &str {
    request.lines().next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn method_path_query_cookies_and_body_arrive_unchanged_with_the_upstreams_host() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "clicked")), "", "").await;

    let response = get(
        proxy.addr,
        "POST /test?abc=123&x=%2F HTTP/1.1\r\n\
         Host: domain1.com\r\n\
         Cookie: a=1; b=2\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: 26\r\n\
         Connection: close\r\n\r\n\
         List-Unsubscribe=One-Click",
    )
    .await;

    assert_eq!(status(&response), "200", "{response}");
    assert!(response.ends_with("clicked"), "{response}");
    let seen = upstream.requests();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    assert_eq!(request_line(req), "POST /test?abc=123&x=%2F HTTP/1.1");
    assert_eq!(header(req, "host"), vec![upstream.addr.to_string()]);
    assert_eq!(header(req, "cookie"), vec!["a=1; b=2"]);
    assert!(req.ends_with("\r\n\r\nList-Unsubscribe=One-Click"), "{req}");
    // Hop-by-hop headers are the connection's, not the request's.
    assert!(
        header(req, "connection")
            .iter()
            .all(|v| !v.contains("close")),
        "{req}"
    );
}

#[tokio::test]
async fn a_path_prefix_on_the_upstream_prefixes_every_forwarded_path() {
    for (configured, requested, expected) in [
        ("/tracking", "/test?abc=123", "/tracking/test?abc=123"),
        ("/tracking/", "/test?abc=123", "/tracking/test?abc=123"),
        ("/a/b", "/", "/a/b"),
        ("/a/b/", "/", "/a/b/"),
        ("/tracking", "/?q=1", "/tracking?q=1"),
    ] {
        let (upstream, proxy) =
            plain(Behaviour::Respond(ok_response("", "")), configured, "").await;
        let response = get(
            proxy.addr,
            &format!("GET {requested} HTTP/1.1\r\nHost: domain1.com\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(
            status(&response),
            "200",
            "{configured} {requested}: {response}"
        );
        assert_eq!(
            request_line(&upstream.requests()[0]),
            format!("GET {expected} HTTP/1.1"),
            "{configured} + {requested}"
        );
    }
}

#[tokio::test]
async fn the_load_balancers_forwarded_headers_are_kept_and_its_address_appended() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "")), "", "").await;

    get(
        proxy.addr,
        "GET /c HTTP/1.1\r\nHost: 10.0.0.5\r\n\
         X-Forwarded-For: 203.0.113.9\r\n\
         X-Forwarded-Host: click.domain1.com\r\n\
         X-Forwarded-Proto: https\r\n\
         Connection: close\r\n\r\n",
    )
    .await;

    let req = &upstream.requests()[0];
    assert_eq!(
        header(req, "x-forwarded-for"),
        vec!["203.0.113.9, 127.0.0.1"]
    );
    assert_eq!(header(req, "x-forwarded-host"), vec!["click.domain1.com"]);
    assert_eq!(header(req, "x-forwarded-proto"), vec!["https"]);
    assert_eq!(header(req, "via"), vec!["1.1 simmer"]);
}

#[tokio::test]
async fn without_a_load_balancer_the_forwarded_headers_come_from_this_hop() {
    let (upstream, proxy) = plain(
        Behaviour::Respond(ok_response("", "")),
        "",
        "public_scheme: http\n",
    )
    .await;

    get(
        proxy.addr,
        "GET /c HTTP/1.1\r\nHost: domain1.com\r\nConnection: close\r\n\r\n",
    )
    .await;

    let req = &upstream.requests()[0];
    assert_eq!(header(req, "x-forwarded-for"), vec!["127.0.0.1"]);
    assert_eq!(header(req, "x-forwarded-host"), vec!["domain1.com"]);
    assert_eq!(header(req, "x-forwarded-proto"), vec!["http"]);
}

#[tokio::test]
async fn http_1_0_is_forwarded() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "ten")), "", "").await;
    let response = get(proxy.addr, "GET /old HTTP/1.0\r\nHost: domain1.com\r\n\r\n").await;
    assert_eq!(status(&response), "200", "{response}");
    assert!(response.ends_with("ten"));
    assert_eq!(request_line(&upstream.requests()[0]), "GET /old HTTP/1.1");
}

#[tokio::test]
async fn https_upstreams_are_verified() {
    let pki = TestPki::new(&["localhost"]);
    let upstream = Upstream::start(Behaviour::Respond(ok_response("", "secure")), Some(&pki)).await;
    let url = format!("https://localhost:{}/p", upstream.addr.port());

    let trusted = start_proxy(config(&url, ""), pki.client_config()).await;
    let response = get(
        trusted.addr,
        "GET /x HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "200", "{response}");
    assert!(response.ends_with("secure"));
    assert_eq!(request_line(&upstream.requests()[0]), "GET /p/x HTTP/1.1");

    // A certificate the proxy does not trust is a 502, never a plaintext retry.
    let untrusted = start_proxy(config(&url, ""), no_roots()).await;
    let response = get(
        untrusted.addr,
        "GET /x HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "502", "{response}");
    assert_eq!(upstream.requests().len(), 1);
}

// ---------------------------------------------------------------------------
// Response rewrites
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_to_the_upstream_is_rewritten_and_never_followed() {
    let upstream_addr = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    // The upstream's own authority has to be known before it starts, so bind
    // twice: the second listener takes the port the first released.
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{upstream_addr}/tracking/landing?u=1\r\n\
         Set-Cookie: sid=9; Domain=127.0.0.1; Path=/tracking/u; HttpOnly\r\n\
         Set-Cookie: other=1; Path=/\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let listener = TcpListener::bind(upstream_addr).await.unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            serve_upstream(
                s,
                Behaviour::Respond(redirect.clone()),
                Arc::clone(&recorded),
            )
            .await;
        }
    });

    let proxy = start_proxy(
        config(&format!("http://{upstream_addr}/tracking"), ""),
        no_roots(),
    )
    .await;
    let response = get(
        proxy.addr,
        "GET /c/1 HTTP/1.1\r\nHost: 10.0.0.5\r\nX-Forwarded-Host: click.domain1.com\r\n\
         X-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert_eq!(status(&response), "302", "{response}");
    assert_eq!(
        header(&response, "location"),
        vec!["https://click.domain1.com/landing?u=1"]
    );
    assert_eq!(
        header(&response, "set-cookie"),
        vec![
            "sid=9; Domain=click.domain1.com; Path=/u; HttpOnly",
            "other=1; Path=/"
        ]
    );
    assert!(header(&response, "cache-control").is_empty(), "{response}");
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "the redirect was followed"
    );
}

#[tokio::test]
async fn a_redirect_to_the_real_destination_passes_through() {
    let (upstream, proxy) = plain(
        Behaviour::Respond(
            "HTTP/1.1 302 Found\r\nLocation: https://shop.example/landing?utm=1\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ),
        "",
        "",
    )
    .await;
    let response = get(
        proxy.addr,
        "GET /c HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        header(&response, "location"),
        vec!["https://shop.example/landing?utm=1"]
    );
    assert_eq!(upstream.requests().len(), 1);
}

#[tokio::test]
async fn a_response_body_streams_rather_than_being_buffered() {
    let (_upstream, proxy) = plain(
        Behaviour::PartialThenStall(
            "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nfirst-bytes".to_string(),
        ),
        "",
        "",
    )
    .await;
    let response = exchange(
        proxy.addr,
        b"GET /big HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(status(&response), "200", "{response}");
    assert!(response.ends_with("first-bytes"), "{response}");
}

// ---------------------------------------------------------------------------
// What it refuses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_absolute_form_target_cannot_aim_the_proxy_elsewhere() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "")), "/t", "").await;
    get(
        proxy.addr,
        "GET http://evil.example:8080/steal?x=1 HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    let req = &upstream.requests()[0];
    assert_eq!(request_line(req), "GET /t/steal?x=1 HTTP/1.1");
    assert_eq!(header(req, "host"), vec![upstream.addr.to_string()]);
}

#[tokio::test]
async fn connect_is_refused_and_not_remembered() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "")), "", "").await;
    let response = get(
        proxy.addr,
        "CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example:443\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "501", "{response}");
    assert_eq!(header(&response, "cache-control"), vec!["no-store"]);
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_websocket_upgrade_is_forwarded_as_a_plain_request() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "plain")), "", "").await;
    let response = get(
        proxy.addr,
        "GET /ws HTTP/1.1\r\nHost: d1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "200", "{response}");
    let req = &upstream.requests()[0];
    assert!(header(req, "upgrade").is_empty(), "{req}");
}

#[tokio::test]
async fn an_http2_preface_gets_no_forwarded_request() {
    let (upstream, proxy) = plain(Behaviour::Respond(ok_response("", "")), "", "").await;
    let response = exchange(
        proxy.addr,
        b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        Duration::from_secs(5),
    )
    .await;
    assert!(!response.starts_with("HTTP/1.1 2"), "{response}");
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_peer_outside_allowed_cidrs_is_dropped_before_a_byte_is_read() {
    let upstream = Upstream::start(Behaviour::Respond(ok_response("", "")), None).await;
    let cfg: LinkProxy = serde_yaml_ng::from_str(&format!(
        "listen: \"127.0.0.1:0\"\nupstream: \"http://{}\"\nallowed_cidrs: [\"10.0.0.0/8\"]\n",
        upstream.addr
    ))
    .unwrap();
    let proxy = start_proxy(cfg, no_roots()).await;
    let response = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: d1\r\n\r\n",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(response, "");
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_body_over_the_limit_is_413_and_never_sent() {
    let (upstream, proxy) = plain(
        Behaviour::Respond(ok_response("", "")),
        "",
        "max_request_bytes: 10\n",
    )
    .await;
    let response = get(
        proxy.addr,
        "POST /u HTTP/1.1\r\nHost: d1\r\nContent-Length: 11\r\nConnection: close\r\n\r\n01234567890",
    )
    .await;
    assert_eq!(status(&response), "413", "{response}");
    assert_eq!(header(&response, "cache-control"), vec!["no-store"]);
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn a_dead_upstream_is_a_502_nobody_caches() {
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let proxy = start_proxy(config(&format!("http://{dead}"), ""), no_roots()).await;
    let response = get(
        proxy.addr,
        "GET /c HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "502", "{response}");
    assert_eq!(header(&response, "cache-control"), vec!["no-store"]);
}

#[tokio::test]
async fn a_stalled_upstream_is_a_504_nobody_caches() {
    let (upstream, proxy) = plain(
        Behaviour::Stall,
        "",
        "timeouts: { upstream_response: 300ms }\n",
    )
    .await;
    let response = get(
        proxy.addr,
        "GET /c HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&response), "504", "{response}");
    assert_eq!(header(&response, "cache-control"), vec!["no-store"]);
    assert_eq!(
        upstream.requests().len(),
        1,
        "a timed-out request is not retried"
    );
}

#[tokio::test]
async fn a_header_read_that_never_finishes_is_closed() {
    let (upstream, proxy) = plain(
        Behaviour::Respond(ok_response("", "")),
        "",
        "timeouts: { header_read: 300ms }\n",
    )
    .await;
    let started = std::time::Instant::now();
    let _ = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: d1\r\nX-Slow: ",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(upstream.requests().is_empty());
}

#[tokio::test]
async fn over_max_connections_is_503() {
    let (_upstream, proxy) = plain(Behaviour::Stall, "", "max_connections: 1\n").await;
    // Hold the one slot with a request the stalled upstream never answers.
    let mut held = TcpStream::connect(proxy.addr).await.unwrap();
    held.write_all(b"GET /a HTTP/1.1\r\nHost: d1\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let response = get(proxy.addr, "GET /b HTTP/1.1\r\nHost: d1\r\n\r\n").await;
    assert_eq!(status(&response), "503", "{response}");
    assert_eq!(header(&response, "cache-control"), vec!["no-store"]);
}

// ---------------------------------------------------------------------------
// §10.4
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_lets_a_request_in_flight_finish_and_accepts_no_more() {
    let (upstream, proxy) = plain(
        Behaviour::Stall,
        "",
        "timeouts: { upstream_response: 1s }\n",
    )
    .await;
    let addr = proxy.addr;
    let in_flight = tokio::spawn(async move {
        get(
            addr,
            "GET /slow HTTP/1.1\r\nHost: d1\r\nConnection: close\r\n\r\n",
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(upstream.requests().len(), 1);

    proxy.stop_accepting.cancel();
    // The request in flight is answered (here by the upstream timeout), not cut.
    let response = in_flight.await.unwrap();
    assert_eq!(status(&response), "504", "{response}");
    tokio::time::timeout(Duration::from_secs(5), proxy.task)
        .await
        .expect("serve returned once drained")
        .unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "still accepting");
    proxy.hard_stop.cancel();
}
