//! D-083 — an optional HTTP/1.x forwarder for tracking and unsubscribe links.
//!
//! Not in `SPEC.md`. Route rewrites point a message's links at a public name
//! (`click.newbrand.com`); a TLS-terminating load balancer sends that name's
//! traffic here; this sends every request, unchanged, to one upstream
//! (`https://link.esp.example`) and relays the response.
//!
//! The forwarding itself is `axum-reverse-proxy`'s: hop-by-hop stripping,
//! `Host` replaced with the upstream's, `X-Forwarded-For` appended,
//! `X-Forwarded-Host`/`-Proto` kept from the load balancer or set, the
//! response-header timeout (`504`), the body cap (`413`) and `Via` loop detection
//! (`508`). What is Simmer's:
//!
//! - **The listener.** hyper's HTTP/1 connection builder, so HTTP/2 is refused by
//!   construction and a header read has a deadline; `allowed_cidrs` checked
//!   before a byte is read; `max_connections` as a semaphore; §10.4's two-phase
//!   shutdown. Upgrades are never enabled, so no WebSocket or tunnel can form.
//! - **The response rewrites** in [`rewrite`], which no crate offers.
//! - **The §14.1 test, applied to HTTP.** Anything this proxy answers itself is
//!   `Cache-Control: no-store`: a browser or intermediary must not remember a
//!   `502` from a bad minute as the answer for a tracking link. And there are no
//!   retries — a one-click unsubscribe POST is not idempotent (D-068's reasoning).
//! - **Logging that does not leak.** Method, path, status, time. Never the query,
//!   cookies or body: a tracking token identifies a recipient, which is why §7.3
//!   hashes addresses.

pub mod rewrite;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, HeaderValue, Request, Response};
use axum_reverse_proxy::{ProxyError, ProxyPolicy, ReverseProxy};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use ipnet::IpNet;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::LinkProxy;
use crate::metrics;
use crate::smtp::Shutdown;
use rewrite::{PublicOrigin, Upstream};

/// The `Via` pseudonym (RFC 9110 §7.6.3). Also how a response is told apart
/// from one the crate answered itself: every forwarded response carries it, and
/// nothing the crate short-circuits does.
pub const VIA: &str = "simmer";

/// Request headers addressed to a proxy rather than through one. The crate
/// strips RFC 9110's hop-by-hop list; these are the two it does not.
const PROXY_ONLY_REQUEST_HEADERS: &[&str] = &["proxy-authorization", "upgrade"];

/// The forwarding service: the crate's proxy plus Simmer's policy around it.
#[derive(Clone)]
pub struct Proxy {
    inner: ReverseProxy<HttpsConnector<HttpConnector>>,
    upstream: Arc<Upstream>,
    public_scheme: &'static str,
}

impl Proxy {
    /// `tls` is `TlsConfigs::verifying()` in production — the platform roots and
    /// ring, as a `required_verify` route uses — and a test CA's in tests.
    ///
    /// `cfg` must have passed `config::validate`, which is what guarantees the
    /// crate's constructor, which panics on a target it cannot parse, will not.
    pub fn new(cfg: &LinkProxy, tls: rustls::ClientConfig) -> anyhow::Result<Self> {
        let upstream = Upstream::parse(&cfg.upstream)
            .ok_or_else(|| anyhow::anyhow!("link_proxy.upstream '{}' is invalid", cfg.upstream))?;

        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(cfg.timeouts.upstream_connect));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(cfg.timeouts.idle)
            .pool_timer(TokioTimer::new())
            // The client's own replay of a request that failed before it was
            // written stays off: see the module comment on retries.
            .retry_canceled_requests(false)
            .build(connector);

        let policy = ProxyPolicy::new()
            .with_public_scheme(cfg.public_scheme.as_str())
            .with_upstream_timeout(cfg.timeouts.upstream_response)
            .with_max_request_body_bytes(cfg.max_request_bytes)
            .with_via(VIA);

        Ok(Self {
            inner: ReverseProxy::new_with_client("/", cfg.upstream.clone(), client)
                .with_policy(policy),
            upstream: Arc::new(upstream),
            public_scheme: cfg.public_scheme.as_str(),
        })
    }

    /// Forward one request from `peer` and return what the client should see.
    pub async fn handle(&self, mut req: Request<Body>, peer: SocketAddr) -> Response<Body> {
        let started = Instant::now();
        let method = req.method().clone();
        // The path only: see the module comment on logging.
        let path = req.uri().path().to_string();

        for name in PROXY_ONLY_REQUEST_HEADERS {
            req.headers_mut().remove(*name);
        }
        let public = rewrite::public_origin(req.headers(), self.public_scheme);
        req.extensions_mut().insert(ConnectInfo(peer));

        let Ok(mut resp) = self.inner.proxy_request(req).await;

        let origin = if is_proxy_originated(&resp) {
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Some(err) = resp.extensions().get::<ProxyError>() {
                tracing::warn!(
                    %method, path, status = resp.status().as_u16(), connect = err.is_connect(),
                    error = err.message(), "link proxy could not reach the upstream"
                );
            }
            "proxy"
        } else {
            if let Some(public) = &public {
                rewrite_response_headers(resp.headers_mut(), &self.upstream, public);
            }
            "upstream"
        };

        let status = resp.status().as_u16();
        let seconds = started.elapsed().as_secs_f64();
        metrics::link_proxy_request(status, origin, seconds);
        tracing::info!(%method, path, status, origin, ms = seconds * 1000.0, "link proxy request");
        resp
    }
}

/// Whether the crate answered this itself (`413`, `501`, `502`, `504`, `508`)
/// rather than relaying the upstream's response.
fn is_proxy_originated(resp: &Response<Body>) -> bool {
    if resp.extensions().get::<ProxyError>().is_some() {
        return true;
    }
    !resp
        .headers()
        .get_all(header::VIA)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|element| element.split_whitespace().nth(1) == Some(VIA))
}

/// [`rewrite`]'s two rewrites over every header they apply to.
pub fn rewrite_response_headers(
    headers: &mut axum::http::HeaderMap,
    upstream: &Upstream,
    public: &PublicOrigin,
) {
    for name in [header::LOCATION, header::CONTENT_LOCATION] {
        let rewritten = headers
            .get(&name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| rewrite::rewrite_location(v, upstream, public))
            .and_then(|v| HeaderValue::from_str(&v).ok());
        if let Some(v) = rewritten {
            headers.insert(name, v);
        }
    }

    // `Set-Cookie` is the one header that may not be folded into a list, so each
    // is rewritten in place and the order is kept.
    let cookies: Vec<HeaderValue> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|s| rewrite::rewrite_set_cookie(s, upstream, public))
                .and_then(|s| HeaderValue::from_str(&s).ok())
                .unwrap_or_else(|| v.clone())
        })
        .collect();
    if !cookies.is_empty() {
        headers.remove(header::SET_COOKIE);
        for c in cookies {
            headers.append(header::SET_COOKIE, c);
        }
    }
}

/// The bound listener. Binding is separate from serving so that `main` can fail
/// startup on a port clash before announcing anything, as §5.1 does for SMTP.
pub struct Listener {
    listener: TcpListener,
    proxy: Proxy,
    allowed: Arc<Vec<IpNet>>,
    connections: Arc<Semaphore>,
    max_connections: usize,
    header_read: std::time::Duration,
}

impl Listener {
    pub async fn bind(cfg: &LinkProxy, tls: rustls::ClientConfig) -> anyhow::Result<Self> {
        let proxy = Proxy::new(cfg, tls)?;
        let listener = TcpListener::bind(&cfg.listen)
            .await
            .map_err(|e| anyhow::anyhow!("binding link proxy listener {}: {e}", cfg.listen))?;
        let allowed = cfg
            .allowed_cidrs
            .iter()
            .filter_map(|c| c.parse().ok())
            .collect();
        Ok(Self {
            listener,
            proxy,
            allowed: Arc::new(allowed),
            connections: Arc::new(Semaphore::new(cfg.max_connections)),
            max_connections: cfg.max_connections,
            header_read: cfg.timeouts.header_read,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept until `stop_accepting`, then let requests in flight finish until
    /// `hard_stop` (§10.4). Idle keep-alive connections close at once.
    pub async fn serve(self, stop_accepting: Shutdown, hard_stop: Shutdown) {
        let mut tasks = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop_accepting.cancelled() => break,
                a = self.listener.accept() => a,
                // Reap finished connections so the set does not grow for ever.
                Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
            };
            let (stream, peer) = match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "link proxy accept failed");
                    tokio::task::yield_now().await;
                    continue;
                }
            };
            if !self.allowed.iter().any(|net| net.contains(&peer.ip())) {
                tracing::warn!(peer = %peer, "link proxy connection from outside allowed_cidrs");
                metrics::link_proxy_connection_refused("cidr");
                continue;
            }
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                metrics::link_proxy_connection_refused("limit");
                tasks.spawn(refuse_busy(stream));
                continue;
            };
            let proxy = self.proxy.clone();
            let (stop, hard, header_read) =
                (stop_accepting.clone(), hard_stop.clone(), self.header_read);
            let (sem, max) = (Arc::clone(&self.connections), self.max_connections);
            tasks.spawn(async move {
                metrics::link_proxy_connections(max - sem.available_permits());
                serve_connection(stream, peer, proxy, header_read, stop, hard).await;
                drop(permit);
                metrics::link_proxy_connections(max - sem.available_permits());
            });
        }
        drop(self.listener);
        tracing::info!("link proxy stopped accepting");

        tokio::select! {
            _ = async { while tasks.join_next().await.is_some() {} } => {}
            _ = hard_stop.cancelled() => tasks.abort_all(),
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    proxy: Proxy,
    header_read: std::time::Duration,
    stop_accepting: Shutdown,
    hard_stop: Shutdown,
) {
    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let proxy = proxy.clone();
        async move { Ok::<_, Infallible>(proxy.handle(req.map(Body::new), peer).await) }
    });

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read)
        .keep_alive(true);
    // Deliberately no `.with_upgrades()`: see the module comment.
    let conn = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(conn);

    let result = tokio::select! {
        r = conn.as_mut() => r,
        _ = stop_accepting.cancelled() => {
            conn.as_mut().graceful_shutdown();
            tokio::select! {
                r = conn.as_mut() => r,
                _ = hard_stop.cancelled() => return,
            }
        }
    };
    if let Err(e) = result {
        // A client that went away, a malformed request, an HTTP/2 preface or a
        // header read past its deadline. Worth a debug line, not a warning: on
        // a public name these are background noise.
        tracing::debug!(peer = %peer, error = %e, "link proxy connection ended with an error");
    }
}

/// Over `max_connections`. Answered rather than dropped so the load balancer
/// reports a `503` rather than a reset, and `no-store` for the §14.1 reason.
async fn refuse_busy(mut stream: TcpStream) {
    let _ = stream
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\n\
              Cache-Control: no-store\r\n\
              Content-Length: 0\r\n\
              Connection: close\r\n\r\n",
        )
        .await;
    let _ = stream.shutdown().await;
}
