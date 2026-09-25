//! The DNS leg of §6.7, behind a trait.
//!
//! The trait exists for the same reason §11's `QuotaStore` does: the interesting
//! logic is what Simmer concludes from a set of TXT records, and a test that
//! needed real DNS to reach it would be slow, flaky, and dependent on somebody
//! else's zone file staying the way it was the day it was written.
//!
//! ## One record, several strings
//!
//! A DNS TXT record is a *sequence* of character-strings, each capped at 255
//! bytes, and RFC 7208 §3.3 and RFC 6376 §3.6.2.2 both say the strings of a
//! single record are concatenated with nothing between them. This matters here
//! rather than being pedantry: a 2048-bit DKIM key does not fit in 255 bytes, so
//! **every** real DKIM record arrives split, and a resolver that returned the
//! strings separately would find `p=` truncated and report a working selector as
//! broken.

use async_trait::async_trait;

/// Whatever went wrong asking DNS. Rendered into a `CheckReport` detail, so it
/// wants to be short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError(pub String);

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResolveError {}

#[async_trait]
pub trait TxtResolver: Send + Sync {
    /// Every TXT record at `name`, each already concatenated from its
    /// character-strings.
    ///
    /// An empty vec is "the name resolved and has no TXT records", which is a
    /// different thing from `Err` — "we could not find out". §6.7 treats both as
    /// a failed check, but the operator reading the detail line deserves to know
    /// which one happened.
    async fn txt(&self, name: &str) -> Result<Vec<String>, ResolveError>;
}

/// One MX record: its preference and its exchange host, lowercased and without
/// the trailing root dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mx {
    pub preference: u16,
    pub exchange: String,
}

/// The MX records at a name, and how long the answer may be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MxAnswer {
    pub records: Vec<Mx>,
    /// The smallest TTL among the records; 0 when there are none, and the
    /// caller picks its own negative-cache lifetime.
    pub ttl_secs: u32,
}

/// The MX leg of §3.2 step 2 (D-100), behind a trait for the same reason
/// `TxtResolver` is.
#[async_trait]
pub trait MxResolver: Send + Sync {
    /// Every MX record at `domain`. An empty answer is "resolved, no MX" (and
    /// NXDOMAIN reads the same way); `Err` is "we could not find out".
    async fn mx(&self, domain: &str) -> Result<MxAnswer, ResolveError>;
}

// ---------------------------------------------------------------------------
// The real one
// ---------------------------------------------------------------------------

pub struct Hickory {
    inner: hickory_resolver::TokioResolver,
}

impl Hickory {
    /// Build from the system resolver configuration — `/etc/resolv.conf` in the
    /// container, which is what Docker populates.
    pub fn from_system() -> Result<Self, ResolveError> {
        let inner = hickory_resolver::TokioResolver::builder_tokio()
            .map_err(|e| ResolveError(format!("reading system resolver config: {e}")))?
            .build()
            .map_err(|e| ResolveError(format!("building resolver: {e}")))?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl TxtResolver for Hickory {
    async fn txt(&self, name: &str) -> Result<Vec<String>, ResolveError> {
        use hickory_resolver::proto::rr::{RData, RecordType};

        let lookup = match self.inner.lookup(name, RecordType::TXT).await {
            Ok(l) => l,
            Err(e) if e.is_no_records_found() => return Ok(Vec::new()),
            Err(e) => return Err(ResolveError(e.to_string())),
        };

        Ok(lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::TXT(txt) => Some(txt),
                _ => None,
            })
            .map(|txt| {
                // The concatenation the module comment is about.
                let joined: Vec<u8> = txt
                    .txt_data
                    .iter()
                    .flat_map(|s| s.iter().copied())
                    .collect();
                String::from_utf8_lossy(&joined).into_owned()
            })
            .collect())
    }
}

#[async_trait]
impl MxResolver for Hickory {
    async fn mx(&self, domain: &str) -> Result<MxAnswer, ResolveError> {
        use hickory_resolver::proto::rr::{RData, RecordType};

        // Fully qualified, so a resolv.conf `search` list cannot turn
        // `example.com` into `example.com.corp.internal`.
        let fqdn = format!("{}.", domain.trim_end_matches('.'));
        let lookup = match self.inner.lookup(fqdn.as_str(), RecordType::MX).await {
            Ok(l) => l,
            Err(e) if e.is_no_records_found() => {
                return Ok(MxAnswer {
                    records: Vec::new(),
                    ttl_secs: 0,
                })
            }
            Err(e) => return Err(ResolveError(e.to_string())),
        };

        let mut ttl_secs = u32::MAX;
        let mut records = Vec::new();
        for record in lookup.answers() {
            if let RData::MX(mx) = &record.data {
                ttl_secs = ttl_secs.min(record.ttl);
                records.push(Mx {
                    preference: mx.preference,
                    exchange: normalise_host(&mx.exchange.to_ascii()),
                });
            }
        }
        if records.is_empty() {
            ttl_secs = 0;
        }
        Ok(MxAnswer { records, ttl_secs })
    }
}

/// Lowercase, no trailing root dot: the form MX suffixes are compared in.
pub fn normalise_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// The test one
// ---------------------------------------------------------------------------

/// A resolver with a fixed answer per name, for tests.
///
/// Public rather than `#[cfg(test)]` because `tests/preflight.rs` drives the
/// chain walk through it, and an integration test compiles against the library
/// as an outside crate.
#[derive(Default)]
pub struct Fake {
    answers: std::collections::HashMap<String, Result<Vec<String>, ResolveError>>,
    mx_answers: std::collections::HashMap<String, Result<MxAnswer, ResolveError>>,
    /// How many MX lookups reached the fake — what a cache test counts.
    mx_lookups: std::sync::atomic::AtomicUsize,
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, records: &[&str]) -> Self {
        self.answers.insert(
            name.to_string(),
            Ok(records.iter().map(|s| s.to_string()).collect()),
        );
        self
    }

    /// A name that fails to resolve, as opposed to one with no TXT records.
    pub fn failing(mut self, name: &str, error: &str) -> Self {
        self.answers
            .insert(name.to_string(), Err(ResolveError(error.to_string())));
        self
    }

    /// MX records at `domain`, as `(preference, exchange)`, with a one-hour TTL.
    pub fn with_mx(mut self, domain: &str, records: &[(u16, &str)]) -> Self {
        self.mx_answers.insert(
            domain.to_string(),
            Ok(MxAnswer {
                records: records
                    .iter()
                    .map(|(preference, host)| Mx {
                        preference: *preference,
                        exchange: normalise_host(host),
                    })
                    .collect(),
                ttl_secs: if records.is_empty() { 0 } else { 3600 },
            }),
        );
        self
    }

    /// A domain whose MX lookup fails, as opposed to one with no MX records.
    pub fn failing_mx(mut self, domain: &str, error: &str) -> Self {
        self.mx_answers
            .insert(domain.to_string(), Err(ResolveError(error.to_string())));
        self
    }

    pub fn mx_lookups(&self) -> usize {
        self.mx_lookups.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl MxResolver for Fake {
    async fn mx(&self, domain: &str) -> Result<MxAnswer, ResolveError> {
        self.mx_lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.mx_answers.get(domain).cloned().unwrap_or(Ok(MxAnswer {
            records: Vec::new(),
            ttl_secs: 0,
        }))
    }
}

#[async_trait]
impl TxtResolver for Fake {
    async fn txt(&self, name: &str) -> Result<Vec<String>, ResolveError> {
        // An unconfigured name is NXDOMAIN-shaped: it resolved to nothing. A test
        // that wants the other failure uses `failing`.
        self.answers.get(name).cloned().unwrap_or(Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_fake_distinguishes_no_records_from_a_failure() {
        let r = Fake::new()
            .with("has.example", &["v=spf1 -all"])
            .failing("broken.example", "timed out");

        assert_eq!(r.txt("has.example").await.unwrap(), vec!["v=spf1 -all"]);
        assert_eq!(
            r.txt("empty.example").await.unwrap(),
            Vec::<String>::new(),
            "an unconfigured name resolved to nothing"
        );
        assert!(r.txt("broken.example").await.is_err());
    }
}
