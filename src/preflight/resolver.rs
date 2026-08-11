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
