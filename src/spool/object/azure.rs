//! Azure Blob Storage over Shared Key authorisation (x-ms-version 2021-08-06).
//!
//! There are no published signature vectors for Shared Key as there are for
//! SigV4, so the string-to-sign is tested against one written out by hand from
//! the documented layout, and the whole client against Azurite when
//! `SIMMER_AZURITE` names one (`tests/spool_object.rs`).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use http_body_util::Full;
use hyper::body::Bytes;
use sha2::Sha256;

use super::{uri_encode, xml, Page};
use crate::config::ObjectStoreConfig;

const VERSION: &str = "2021-08-06";

#[derive(Clone)]
pub struct Azure {
    account: String,
    container: String,
    key: Vec<u8>,
    /// `scheme://host[:port]` plus any path before the container — for
    /// Azurite, `/devstoreaccount1`.
    base: String,
}

impl Azure {
    pub fn new(cfg: &ObjectStoreConfig) -> anyhow::Result<Self> {
        let account = cfg
            .account
            .clone()
            .ok_or_else(|| anyhow::anyhow!("spool.body_store.account is required for azure"))?;
        let container = cfg
            .container
            .clone()
            .ok_or_else(|| anyhow::anyhow!("spool.body_store.container is required for azure"))?;
        let key = cfg
            .access_key
            .clone()
            .or_else(|| std::env::var("AZURE_STORAGE_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "spool.body_store has no Azure access_key and AZURE_STORAGE_KEY is not set"
                )
            })?;
        let key = B64
            .decode(key.trim())
            .map_err(|e| anyhow::anyhow!("spool.body_store.access_key is not base64: {e}"))?;
        let base = match &cfg.endpoint {
            Some(e) => e.trim_end_matches('/').to_string(),
            None => format!("https://{account}.blob.core.windows.net"),
        };
        Ok(Self {
            account,
            container,
            key,
            base,
        })
    }

    pub fn describe(&self) -> String {
        format!(
            "container {} in {} at {}",
            self.container, self.account, self.base
        )
    }

    /// The base's own path (Azurite's `/account`), then the container.
    fn container_path(&self) -> String {
        let base_path = self
            .base
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or("");
        format!("{base_path}/{}", uri_encode(&self.container, false))
    }

    fn origin(&self) -> String {
        match self.base.split_once("://") {
            Some((scheme, rest)) => {
                let host = rest.split('/').next().unwrap_or(rest);
                format!("{scheme}://{host}")
            }
            None => self.base.clone(),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Result<hyper::Request<Full<Bytes>>, String> {
        let now = Utc::now();
        let date = now.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut ms: Vec<(String, String)> = vec![
            ("x-ms-date".into(), date),
            ("x-ms-version".into(), VERSION.into()),
        ];
        for (k, v) in extra {
            ms.push(((*k).to_string(), (*v).to_string()));
        }
        let length = if body.is_empty() {
            String::new()
        } else {
            body.len().to_string()
        };
        let to_sign = string_to_sign(method, &length, &ms, &self.account, path, query);
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(to_sign.as_bytes());
        let signature = B64.encode(mac.finalize().into_bytes());

        let mut q: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
            .collect();
        q.sort();
        let uri = if q.is_empty() {
            format!("{}{path}", self.origin())
        } else {
            format!("{}{path}?{}", self.origin(), q.join("&"))
        };
        let mut req = hyper::Request::builder().method(method).uri(uri);
        for (k, v) in &ms {
            req = req.header(k.as_str(), v.as_str());
        }
        req.header(
            "authorization",
            format!("SharedKey {}:{signature}", self.account),
        )
        .header("content-length", body.len().to_string())
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| e.to_string())
    }

    fn blob_path(&self, key: &str) -> String {
        format!("{}/{}", self.container_path(), uri_encode(key, true))
    }

    pub fn put(&self, key: &str, body: &[u8]) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request(
            "PUT",
            &self.blob_path(key),
            &[],
            &[("x-ms-blob-type", "BlockBlob")],
            body,
        )
    }

    pub fn get(&self, key: &str) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request("GET", &self.blob_path(key), &[], &[], b"")
    }

    pub fn delete(&self, key: &str) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request("DELETE", &self.blob_path(key), &[], &[], b"")
    }

    pub fn create_container(&self) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request(
            "PUT",
            &self.container_path(),
            &[("restype", "container".to_string())],
            &[],
            b"",
        )
    }

    pub fn list(
        &self,
        prefix: &str,
        marker: Option<&str>,
    ) -> Result<hyper::Request<Full<Bytes>>, String> {
        let mut q = vec![
            ("restype", "container".to_string()),
            ("comp", "list".to_string()),
        ];
        if !prefix.is_empty() {
            q.push(("prefix", prefix.to_string()));
        }
        if let Some(m) = marker {
            q.push(("marker", m.to_string()));
        }
        self.request("GET", &self.container_path(), &q, &[], b"")
    }
}

/// The Shared Key string-to-sign for a request with no standard headers but
/// `Content-Length` (blank when zero, per version 2015-02-21 and later).
pub(crate) fn string_to_sign(
    method: &str,
    content_length: &str,
    ms_headers: &[(String, String)],
    account: &str,
    path: &str,
    query: &[(&str, String)],
) -> String {
    let mut headers: Vec<(String, String)> = ms_headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    let mut params: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    params.sort();
    let mut resource = format!("/{account}{path}");
    for (k, v) in &params {
        resource.push_str(&format!("\n{k}:{v}"));
    }

    // VERB, Content-Encoding, Content-Language, Content-Length, Content-MD5,
    // Content-Type, Date, If-Modified-Since, If-Match, If-None-Match,
    // If-Unmodified-Since, Range — then the headers and the resource.
    format!("{method}\n\n\n{content_length}\n\n\n\n\n\n\n\n\n{canonical_headers}{resource}")
}

/// `List Blobs`: `Blob/Name` and `Properties/Last-Modified`, and the marker.
pub(crate) fn parse_list(body: &[u8]) -> Result<Page, String> {
    let doc = std::str::from_utf8(body).map_err(|e| format!("listing is not UTF-8: {e}"))?;
    // A UTF-8 BOM leads some responses.
    let doc = doc.trim_start_matches('\u{feff}');
    let mut page = Page::default();
    for b in xml::blocks(doc, "Blob") {
        let (Some(name), Some(modified)) = (xml::first(b, "Name"), xml::first(b, "Last-Modified"))
        else {
            continue;
        };
        let modified = DateTime::parse_from_rfc2822(&modified)
            .map_err(|e| format!("Last-Modified '{modified}': {e}"))?
            .with_timezone(&Utc);
        page.objects.push((name, modified));
    }
    page.next = xml::first(doc, "NextMarker").filter(|m| !m.is_empty());
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_string_to_sign_follows_the_documented_layout() {
        let ms = vec![
            ("x-ms-version".to_string(), "2021-08-06".to_string()),
            (
                "x-ms-date".to_string(),
                "Fri, 26 Jun 2015 23:39:12 GMT".to_string(),
            ),
            ("x-ms-blob-type".to_string(), "BlockBlob".to_string()),
        ];
        let s = string_to_sign(
            "PUT",
            "11",
            &ms,
            "myaccount",
            "/mycontainer/spool/a.eml",
            &[],
        );
        assert_eq!(
            s,
            "PUT\n\n\n11\n\n\n\n\n\n\n\n\n\
             x-ms-blob-type:BlockBlob\n\
             x-ms-date:Fri, 26 Jun 2015 23:39:12 GMT\n\
             x-ms-version:2021-08-06\n\
             /myaccount/mycontainer/spool/a.eml"
        );

        let list = string_to_sign(
            "GET",
            "",
            &ms[..2],
            "myaccount",
            "/mycontainer",
            &[
                ("restype", "container".into()),
                ("comp", "list".into()),
                ("prefix", "spool/".into()),
            ],
        );
        assert!(
            list.ends_with("/myaccount/mycontainer\ncomp:list\nprefix:spool/\nrestype:container")
        );
        assert!(list.starts_with("GET\n\n\n\n"), "a zero length is blank");
    }

    #[test]
    fn azurite_s_path_style_keeps_the_account_in_the_path() {
        let cfg: ObjectStoreConfig = serde_yaml_ng::from_str(
            "{ provider: azure, account: devstoreaccount1, container: c, \
               access_key: \"a2V5\", endpoint: \"http://127.0.0.1:10000/devstoreaccount1\", \
               allow_http: true }",
        )
        .unwrap();
        let a = Azure::new(&cfg).unwrap();
        let req = a.get("spool/x.eml").unwrap();
        assert_eq!(
            req.uri().to_string(),
            "http://127.0.0.1:10000/devstoreaccount1/c/spool/x.eml"
        );
    }

    #[test]
    fn a_listing_page_parses() {
        let doc = "\u{feff}<?xml version=\"1.0\"?><EnumerationResults><Blobs>\
            <Blob><Name>spool/a.eml</Name><Properties>\
            <Last-Modified>Mon, 12 Oct 2009 17:50:30 GMT</Last-Modified></Properties></Blob>\
            </Blobs><NextMarker>m2</NextMarker></EnumerationResults>";
        let page = parse_list(doc.as_bytes()).unwrap();
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].0, "spool/a.eml");
        assert_eq!(page.next.as_deref(), Some("m2"));
    }
}
