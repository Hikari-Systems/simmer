//! §7.7's object-store body stores (D-117, D-123): an S3-compatible bucket or
//! an Azure Blob container, spoken to directly over the spool's HTTPS client.
//!
//! Hand-written rather than a crate (D-123): the one maintained crate covering
//! both, `object_store`, turns on `aws-lc-rs` with either provider, and this
//! crate is `ring`-only. Four operations are needed — put, get, delete, list —
//! and the signing for each provider is a page of code with published vectors
//! to test it against (`s3.rs`).
//!
//! A body ref is the full object key, prefix included, so a listing's keys are
//! refs as they stand.

mod azure;
pub mod s3;
mod xml;

use std::time::{Duration, SystemTime};

use http_body_util::Full;
use hyper::body::Bytes;
use uuid::Uuid;

use super::body::BodyError;
use super::http::{self, HttpsClient};
use crate::config::{ObjectProvider, ObjectStoreConfig};

#[derive(Clone)]
pub struct ObjectStore {
    client: HttpsClient,
    timeout: Duration,
    prefix: String,
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    S3(s3::S3),
    Azure(azure::Azure),
}

impl std::fmt::Debug for ObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.backend {
            Backend::S3(s) => format!("s3 {}", s.describe()),
            Backend::Azure(a) => format!("azure {}", a.describe()),
        };
        f.debug_struct("ObjectStore")
            .field("backend", &kind)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ObjectStore {
    pub fn with_tls(cfg: &ObjectStoreConfig, tls: rustls::ClientConfig) -> anyhow::Result<Self> {
        let backend = match cfg.provider {
            ObjectProvider::S3 => Backend::S3(s3::S3::new(cfg)?),
            ObjectProvider::Azure => Backend::Azure(azure::Azure::new(cfg)?),
        };
        let prefix = cfg
            .prefix
            .as_deref()
            .map(|p| p.trim_matches('/'))
            .filter(|p| !p.is_empty())
            .map(|p| format!("{p}/"))
            .unwrap_or_default();
        Ok(Self {
            client: http::client(tls, cfg.timeout),
            timeout: cfg.timeout,
            prefix,
            backend,
        })
    }

    fn key_for(&self, id: Uuid) -> String {
        format!("{}{id}.eml", self.prefix)
    }

    async fn send(&self, req: hyper::Request<Full<Bytes>>) -> Result<http::Response, BodyError> {
        http::send(&self.client, req, self.timeout)
            .await
            .map_err(BodyError::Io)
    }

    pub async fn put(&self, id: Uuid, bytes: &[u8]) -> Result<String, BodyError> {
        let key = self.key_for(id);
        let req = match &self.backend {
            Backend::S3(s) => s.put(&key, bytes),
            Backend::Azure(a) => a.put(&key, bytes),
        }
        .map_err(BodyError::Io)?;
        let resp = self.send(req).await?;
        // A 2xx is the store's own word that the object is durable.
        if !(200..300).contains(&resp.status) {
            return Err(failure("put", &resp));
        }
        Ok(key)
    }

    pub async fn get(&self, body_ref: &str) -> Result<Vec<u8>, BodyError> {
        let req = match &self.backend {
            Backend::S3(s) => s.get(body_ref),
            Backend::Azure(a) => a.get(body_ref),
        }
        .map_err(BodyError::Io)?;
        let resp = self.send(req).await?;
        match resp.status {
            200..=299 => Ok(resp.body),
            404 => Err(BodyError::NotFound(body_ref.to_string())),
            _ => Err(failure("get", &resp)),
        }
    }

    pub async fn delete(&self, body_ref: &str) -> Result<(), BodyError> {
        let req = match &self.backend {
            Backend::S3(s) => s.delete(body_ref),
            Backend::Azure(a) => a.delete(body_ref),
        }
        .map_err(BodyError::Io)?;
        let resp = self.send(req).await?;
        match resp.status {
            200..=299 | 404 => Ok(()),
            _ => Err(failure("delete", &resp)),
        }
    }

    /// Create the bucket or container. For first-time setup and the tests;
    /// the server never calls it — creating storage is the operator's call.
    /// Already existing is success.
    pub async fn create_container(&self) -> Result<(), BodyError> {
        let req = match &self.backend {
            Backend::S3(s) => s.create_bucket(),
            Backend::Azure(a) => a.create_container(),
        }
        .map_err(BodyError::Io)?;
        let resp = self.send(req).await?;
        match resp.status {
            200..=299 | 409 => Ok(()),
            _ => Err(failure("create", &resp)),
        }
    }

    pub async fn list_older_than(&self, cutoff: SystemTime) -> Result<Vec<String>, BodyError> {
        let cutoff: chrono::DateTime<chrono::Utc> = cutoff.into();
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let req = match &self.backend {
                Backend::S3(s) => s.list(&self.prefix, token.as_deref()),
                Backend::Azure(a) => a.list(&self.prefix, token.as_deref()),
            }
            .map_err(BodyError::Io)?;
            let resp = self.send(req).await?;
            if !(200..300).contains(&resp.status) {
                return Err(failure("list", &resp));
            }
            let page = match &self.backend {
                Backend::S3(_) => s3::parse_list(&resp.body),
                Backend::Azure(_) => azure::parse_list(&resp.body),
            }
            .map_err(BodyError::Io)?;
            out.extend(
                page.objects
                    .into_iter()
                    .filter(|(key, modified)| key.ends_with(".eml") && *modified < cutoff)
                    .map(|(key, _)| key),
            );
            match page.next {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One page of a listing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Page {
    pub objects: Vec<(String, chrono::DateTime<chrono::Utc>)>,
    pub next: Option<String>,
}

fn failure(op: &str, resp: &http::Response) -> BodyError {
    // The provider's error document names the code; never the credentials.
    let snippet: String = String::from_utf8_lossy(&resp.body)
        .chars()
        .take(300)
        .collect();
    BodyError::Io(format!(
        "object store {op}: HTTP {}: {snippet}",
        resp.status
    ))
}

/// RFC 3986 percent-encoding of everything but the unreserved characters, and
/// `/` too unless `keep_slash` — SigV4's `UriEncode`, which Azure's paths also
/// accept.
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
