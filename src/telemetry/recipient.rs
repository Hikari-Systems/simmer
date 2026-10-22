//! D-114 — what of a message's recipient the exported spans may carry.
//!
//! `telemetry.recipients` is the one switch, and [`Exposure`] is the one place
//! that reads it. Every call site hands it the address and a span; what comes
//! out is decided here and nowhere else.
//!
//! **Attributes are set on the OpenTelemetry span, never as `tracing` fields.**
//! The stdout JSON layer prints the current span's fields on every line inside
//! it (`with_current_span(true)`, `src/logging.rs`), so a recipient recorded as
//! a field would put the address on every INFO line of the transaction, which
//! §9.5 forbids for stdout in every mode. `OpenTelemetrySpanExt` writes to the
//! span's OTel data directly: the trace exporter sees it, and stdout, the log
//! bridge and the §9.1 metrics cannot.
//!
//! The hash is keyed by `recipient_hash_key`, **not** §7.3's salt. Under the
//! salt and the same normalisation, the exported value would be the
//! `recipient_events` key itself, and the export would become an index into
//! that table.

use hmac::{Hmac, Mac};
use opentelemetry::KeyValue;
use sha2::Sha256;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::config::{Config, FrequencyMode, RecipientExposure};
use crate::downstream::outcome::redact_addresses;
use crate::frequency;
use crate::smtp::reply::sanitise_downstream_text;

/// The span event a refused `RCPT TO` adds to `smtp.transaction`.
pub const RCPT_REFUSED: &str = "smtp.rcpt.refused";

/// The configured exposure, borrowed from the config. `None` from
/// [`Exposure::from_config`] is the whole of "off".
#[derive(Debug, Clone, Copy)]
pub struct Exposure<'a> {
    mode: RecipientExposure,
    key: Option<&'a [u8]>,
    dot_insensitive: &'a [String],
}

impl<'a> Exposure<'a> {
    /// Off unless telemetry is on, exports traces, and names a mode other than
    /// `none`.
    pub fn from_config(cfg: &'a Config) -> Option<Self> {
        let t = cfg.telemetry().filter(|t| t.traces)?;
        Self::new(
            t.recipients,
            t.recipient_hash_key.as_deref().map(str::as_bytes),
            &cfg.dot_insensitive_domains,
        )
    }

    /// `None` for `none`, and for `hashed` with no key — which §4.2 refuses,
    /// so the second only happens to a caller that built one by hand.
    pub fn new(
        mode: RecipientExposure,
        key: Option<&'a [u8]>,
        dot_insensitive: &'a [String],
    ) -> Option<Self> {
        match mode {
            RecipientExposure::None => None,
            RecipientExposure::Hashed if key.is_none() => None,
            _ => Some(Self {
                mode,
                key,
                dot_insensitive,
            }),
        }
    }

    /// The recipient as span attributes, cumulatively by mode.
    pub fn attributes(&self, address: &str) -> Vec<KeyValue> {
        let mut out = Vec::with_capacity(3);
        if let Some((_, domain)) = address.trim().rsplit_once('@') {
            out.push(KeyValue::new(
                "simmer.recipient.domain",
                domain.to_ascii_lowercase(),
            ));
        }
        if self.mode >= RecipientExposure::Hashed {
            if let Some(key) = self.key {
                out.push(KeyValue::new(
                    "simmer.recipient.hash",
                    hash(key, address, self.dot_insensitive),
                ));
            }
        }
        if self.mode >= RecipientExposure::Address {
            out.push(KeyValue::new(
                "simmer.recipient.address",
                address.trim().to_string(),
            ));
        }
        out
    }

    /// Set [`attributes`](Self::attributes) on `span`.
    pub fn annotate(&self, span: &tracing::Span, address: &str) {
        for kv in self.attributes(address) {
            span.set_attribute(kv.key, kv.value);
        }
    }

    /// Add a [`RCPT_REFUSED`] event to `span`: the recipient, and the reply it
    /// was refused with.
    pub fn refused(&self, span: &tracing::Span, address: &str, code: u16) {
        let mut attrs = self.attributes(address);
        attrs.push(KeyValue::new("smtp.reply.code", i64::from(code)));
        span.add_event(RCPT_REFUSED, attrs);
    }

    /// A downstream's reply text as the export may carry it: sanitised as
    /// §10.1 sanitises it for the client, and with every address's local part
    /// masked (§9.5) unless the mode is `address`.
    pub fn reply_text(&self, text: &str) -> String {
        let clean = sanitise_downstream_text(text);
        if self.mode >= RecipientExposure::Address {
            clean
        } else {
            redact_addresses(&clean)
        }
    }
}

/// HMAC-SHA256 under `key` of the §7.3 `to_address` normalisation, truncated to
/// 16 bytes and hex-encoded. Normalised so `Bob.Smith+news@gmail.com` and
/// `bobsmith@gmail.com` hash alike, as they count alike in §7.3.
fn hash(key: &[u8], address: &str, dot_insensitive: &[String]) -> String {
    let normalised = frequency::normalise(address, FrequencyMode::ToAddress, dot_insensitive);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(normalised.as_bytes());
    mac.finalize().into_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef";

    fn dots() -> Vec<String> {
        vec!["gmail.com".to_string()]
    }

    fn keys(e: &Exposure<'_>, address: &str) -> Vec<String> {
        e.attributes(address)
            .into_iter()
            .map(|kv| kv.key.as_str().to_string())
            .collect()
    }

    #[test]
    fn none_is_off() {
        assert!(Exposure::new(RecipientExposure::None, Some(KEY), &[]).is_none());
    }

    #[test]
    fn hashed_without_a_key_is_off_rather_than_unkeyed() {
        assert!(Exposure::new(RecipientExposure::Hashed, None, &[]).is_none());
    }

    #[test]
    fn each_mode_exports_the_one_before_it_and_more() {
        let d = dots();
        let at = |mode| Exposure::new(mode, Some(KEY), &d).expect("on");
        let a = "Jane@Example.com";
        assert_eq!(
            keys(&at(RecipientExposure::Domain), a),
            ["simmer.recipient.domain"]
        );
        assert_eq!(
            keys(&at(RecipientExposure::Hashed), a),
            ["simmer.recipient.domain", "simmer.recipient.hash"]
        );
        assert_eq!(
            keys(&at(RecipientExposure::Address), a),
            [
                "simmer.recipient.domain",
                "simmer.recipient.hash",
                "simmer.recipient.address"
            ]
        );
    }

    #[test]
    fn domain_mode_carries_no_local_part() {
        let e = Exposure::new(RecipientExposure::Domain, None, &[]).expect("on");
        let attrs = e.attributes("jane.doe@Example.COM");
        assert_eq!(attrs[0].value.as_str(), "example.com");
        assert!(!format!("{attrs:?}").contains("jane"));
    }

    #[test]
    fn a_domainless_recipient_has_no_domain_attribute() {
        let e = Exposure::new(RecipientExposure::Domain, None, &[]).expect("on");
        assert!(e.attributes("postmaster").is_empty());
    }

    #[test]
    fn the_hash_is_stable_keyed_and_normalised() {
        let d = dots();
        let h = |addr| hash(KEY, addr, &d);
        assert_eq!(h("bobsmith@gmail.com"), h("bobsmith@gmail.com"));
        assert_eq!(h("bobsmith@gmail.com").len(), 32);
        // §7.3's normalisation: case, the `+` tag, and dots on a dot-insensitive
        // domain.
        assert_eq!(h("Bob.Smith+news@Gmail.com"), h("bobsmith@gmail.com"));
        // Dots matter elsewhere.
        assert_ne!(h("bob.smith@example.com"), h("bobsmith@example.com"));
        assert_ne!(
            hash(KEY, "bobsmith@gmail.com", &d),
            hash(b"fedcba9876543210", "bobsmith@gmail.com", &d)
        );
        assert!(!h("bobsmith@gmail.com").contains("bobsmith"));
    }

    #[test]
    fn reply_text_is_masked_in_every_mode_but_address() {
        let text = "5.1.1 <jane@example.com> unknown\r\nuser";
        for mode in [RecipientExposure::Domain, RecipientExposure::Hashed] {
            let e = Exposure::new(mode, Some(KEY), &[]).expect("on");
            assert_eq!(e.reply_text(text), "5.1.1 <*@example.com> unknown user");
        }
        let e = Exposure::new(RecipientExposure::Address, None, &[]).expect("on");
        assert_eq!(e.reply_text(text), "5.1.1 <jane@example.com> unknown user");
    }
}
