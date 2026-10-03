//! S3 (and S3-compatible: MinIO, R2, Ceph) over Signature Version 4.
//!
//! The signer is tested against the worked examples in the S3 documentation
//! ("Examples: Signature Calculations in AWS Signature Version 4"), whose
//! inputs and signatures are published; a signer that reproduces them byte
//! for byte is a signer S3 accepts.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use http_body_util::Full;
use hyper::body::Bytes;
use sha2::{Digest, Sha256};

use super::{uri_encode, xml, Page};
use crate::config::ObjectStoreConfig;

type HmacSha256 = Hmac<Sha256>;

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone)]
pub struct S3 {
    bucket: String,
    region: String,
    /// `scheme://host[:port]`, no trailing slash.
    base: String,
    /// Path-style (`/bucket/key`) for an explicit endpoint, virtual-hosted
    /// (`bucket.s3.region.amazonaws.com/key`) for AWS itself.
    path_style: bool,
    creds: Credentials,
}

#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl S3 {
    pub fn new(cfg: &ObjectStoreConfig) -> anyhow::Result<Self> {
        let bucket = cfg
            .bucket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("spool.body_store.bucket is required for s3"))?;
        let region = cfg.region.clone().unwrap_or_else(|| "us-east-1".into());
        // Credentials from the config, else the standard environment variables.
        // Instance roles and SSO are not spoken (D-123).
        let creds = match (&cfg.access_key_id, &cfg.secret_access_key) {
            (Some(id), Some(secret)) => Credentials {
                access_key_id: id.clone(),
                secret_access_key: secret.clone(),
                session_token: None,
            },
            _ => Credentials {
                access_key_id: std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
                    anyhow::anyhow!(
                        "spool.body_store has no S3 credentials and AWS_ACCESS_KEY_ID is not set"
                    )
                })?,
                secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
                    anyhow::anyhow!(
                        "spool.body_store has no S3 credentials and AWS_SECRET_ACCESS_KEY is not set"
                    )
                })?,
                session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
            },
        };
        let (base, path_style) = match &cfg.endpoint {
            Some(e) => (e.trim_end_matches('/').to_string(), true),
            None => (format!("https://{bucket}.s3.{region}.amazonaws.com"), false),
        };
        Ok(Self {
            bucket,
            region,
            base,
            path_style,
            creds,
        })
    }

    pub fn describe(&self) -> String {
        format!("bucket {} at {}", self.bucket, self.base)
    }

    fn path(&self, key: &str) -> String {
        if self.path_style {
            format!(
                "/{}/{}",
                uri_encode(&self.bucket, false),
                uri_encode(key, true)
            )
        } else {
            format!("/{}", uri_encode(key, true))
        }
    }

    fn host(&self) -> String {
        self.base
            .split_once("://")
            .map_or(self.base.as_str(), |(_, h)| h)
            .to_string()
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: &[u8],
    ) -> Result<hyper::Request<Full<Bytes>>, String> {
        let payload_hash = hex(&Sha256::digest(body));
        let now = Utc::now();
        let mut headers = vec![
            ("host".to_string(), self.host()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date(now)),
        ];
        if let Some(t) = &self.creds.session_token {
            headers.push(("x-amz-security-token".to_string(), t.clone()));
        }
        let query = canonical_query(query);
        let auth = authorization(
            &self.creds,
            &self.region,
            "s3",
            method,
            path,
            &query,
            &headers,
            &payload_hash,
            now,
        );
        let uri = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let mut req = hyper::Request::builder().method(method).uri(uri);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        req.header("authorization", auth)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|e| e.to_string())
    }

    pub fn put(&self, key: &str, body: &[u8]) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request("PUT", &self.path(key), &[], body)
    }

    pub fn get(&self, key: &str) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request("GET", &self.path(key), &[], b"")
    }

    pub fn delete(&self, key: &str) -> Result<hyper::Request<Full<Bytes>>, String> {
        self.request("DELETE", &self.path(key), &[], b"")
    }

    pub fn create_bucket(&self) -> Result<hyper::Request<Full<Bytes>>, String> {
        let path = if self.path_style {
            format!("/{}", uri_encode(&self.bucket, false))
        } else {
            "/".to_string()
        };
        self.request("PUT", &path, &[], b"")
    }

    pub fn list(
        &self,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<hyper::Request<Full<Bytes>>, String> {
        let mut q = vec![("list-type", "2".to_string())];
        if !prefix.is_empty() {
            q.push(("prefix", prefix.to_string()));
        }
        if let Some(t) = token {
            q.push(("continuation-token", t.to_string()));
        }
        let path = if self.path_style {
            format!("/{}", uri_encode(&self.bucket, false))
        } else {
            "/".to_string()
        };
        self.request("GET", &path, &q, b"")
    }
}

/// `ListObjectsV2`'s page: `Contents/Key` and `LastModified`, and the token.
pub(crate) fn parse_list(body: &[u8]) -> Result<Page, String> {
    let doc = std::str::from_utf8(body).map_err(|e| format!("listing is not UTF-8: {e}"))?;
    let mut page = Page::default();
    for c in xml::blocks(doc, "Contents") {
        let (Some(key), Some(modified)) = (xml::first(c, "Key"), xml::first(c, "LastModified"))
        else {
            continue;
        };
        let modified = DateTime::parse_from_rfc3339(&modified)
            .map_err(|e| format!("LastModified '{modified}': {e}"))?
            .with_timezone(&Utc);
        page.objects.push((key, modified));
    }
    if xml::first(doc, "IsTruncated").as_deref() == Some("true") {
        page.next = xml::first(doc, "NextContinuationToken");
    }
    Ok(page)
}

pub fn amz_date(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Sorted by name then value, each encoded; SigV4's canonical query string.
pub fn canonical_query(q: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = q
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The `Authorization` header for a request whose headers (lowercase names,
/// `host` included) are all signed.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    creds: &Credentials,
    region: &str,
    service: &str,
    method: &str,
    path: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    now: DateTime<Utc>,
) -> String {
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    sorted.sort();
    let canonical_headers: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: String = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"
    );
    let date = now.format("%Y%m%d").to_string();
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        amz_date(now),
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn may_24_2013() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn signature(auth: &str) -> &str {
        auth.rsplit_once("Signature=").unwrap().1
    }

    #[test]
    fn the_published_get_object_example() {
        let headers = vec![
            ("host".into(), "examplebucket.s3.amazonaws.com".into()),
            ("range".into(), "bytes=0-9".into()),
            ("x-amz-content-sha256".into(), EMPTY_SHA256.into()),
            ("x-amz-date".into(), "20130524T000000Z".into()),
        ];
        let auth = authorization(
            &example_creds(),
            "us-east-1",
            "s3",
            "GET",
            "/test.txt",
            "",
            &headers,
            EMPTY_SHA256,
            may_24_2013(),
        );
        assert_eq!(
            signature(&auth),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(auth.contains(
            "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,"
        ));
    }

    #[test]
    fn the_published_put_object_example() {
        let body = b"Welcome to Amazon S3.";
        let hash = hex(&Sha256::digest(body));
        assert_eq!(
            hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let headers = vec![
            ("date".into(), "Fri, 24 May 2013 00:00:00 GMT".into()),
            ("host".into(), "examplebucket.s3.amazonaws.com".into()),
            ("x-amz-content-sha256".into(), hash.clone()),
            ("x-amz-date".into(), "20130524T000000Z".into()),
            ("x-amz-storage-class".into(), "REDUCED_REDUNDANCY".into()),
        ];
        let auth = authorization(
            &example_creds(),
            "us-east-1",
            "s3",
            "PUT",
            &format!("/{}", uri_encode("test$file.text", true)),
            "",
            &headers,
            &hash,
            may_24_2013(),
        );
        assert_eq!(
            signature(&auth),
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    #[test]
    fn the_published_list_objects_example() {
        let headers = vec![
            ("host".into(), "examplebucket.s3.amazonaws.com".into()),
            ("x-amz-content-sha256".into(), EMPTY_SHA256.into()),
            ("x-amz-date".into(), "20130524T000000Z".into()),
        ];
        let query = canonical_query(&[("prefix", "J".into()), ("max-keys", "2".into())]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let auth = authorization(
            &example_creds(),
            "us-east-1",
            "s3",
            "GET",
            "/",
            &query,
            &headers,
            EMPTY_SHA256,
            may_24_2013(),
        );
        assert_eq!(
            signature(&auth),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn a_listing_page_parses() {
        let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult><IsTruncated>true</IsTruncated>
<Contents><Key>spool/a.eml</Key><LastModified>2009-10-12T17:50:30.000Z</LastModified></Contents>
<Contents><Key>spool/b&amp;c.eml</Key><LastModified>2009-10-12T17:50:31.000Z</LastModified></Contents>
<NextContinuationToken>tok</NextContinuationToken></ListBucketResult>"#;
        let page = parse_list(doc).unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[1].0, "spool/b&c.eml");
        assert_eq!(page.next.as_deref(), Some("tok"));
    }
}
