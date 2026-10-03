//! One small HTTPS client for the spool's outbound HTTP: the dead-letter
//! webhook (D-120) and the object stores (D-123). The same hyper-rustls stack
//! as the link proxy, verifying with the platform roots `TlsConfigs` loaded.

use std::time::Duration;

use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

pub type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub fn client(tls: rustls::ClientConfig, connect_timeout: Duration) -> HttpsClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(connect_timeout));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_timer(TokioTimer::new())
        .build(connector)
}

/// A response, read whole and bounded.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Vec<u8>,
}

/// The largest response body read back. A listing page is well under it; a
/// body `get` is bounded by the store, not by this — see `object.rs`.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Send `req` and read the whole response within `timeout`.
pub async fn send(
    client: &HttpsClient,
    req: hyper::Request<Full<Bytes>>,
    timeout: Duration,
) -> Result<Response, String> {
    let fut = async {
        let resp = client.request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let limited = http_body_util::Limited::new(resp.into_body(), MAX_RESPONSE_BYTES);
        let body = limited
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes()
            .to_vec();
        Ok(Response {
            status,
            headers,
            body,
        })
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
}
