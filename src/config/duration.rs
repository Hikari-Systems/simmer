//! Duration strings for the `SPEC.md` §4.1 config (`30s`, `300s`, `5m`).
//!
//! Hand-rolled rather than pulling `humantime`, because the spec only ever uses
//! a bare integer with a single unit suffix and the error messages want to name
//! the offending config key, which a third-party parser cannot do for us.

use std::time::Duration;

use serde::{de, Deserialize, Deserializer};

/// Parse the `<integer><unit>` forms the spec uses. Units: `ms`, `s`, `m`, `h`, `d`.
///
/// A bare integer is rejected rather than assumed to be seconds — an unsuffixed
/// `300` in a config that elsewhere writes `300s` is far more likely to be a
/// mistake than an intent, and a silently-wrong timeout is expensive to find.
pub fn parse(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }

    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration '{s}' has no unit suffix (expected e.g. '30s')"))?;

    if split == 0 {
        return Err(format!("duration '{s}' does not start with a number"));
    }

    let (value, unit) = s.split_at(split);
    let value: u64 = value
        .parse()
        .map_err(|_| format!("duration '{s}' has an unparseable number"))?;

    let millis = match unit {
        "ms" => Some(value),
        "s" => value.checked_mul(1_000),
        "m" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        "d" => value.checked_mul(86_400_000),
        other => {
            return Err(format!(
                "duration '{s}' has unknown unit '{other}' (expected ms, s, m, h or d)"
            ))
        }
    }
    .ok_or_else(|| format!("duration '{s}' overflows"))?;

    Ok(Duration::from_millis(millis))
}

/// serde adaptor so config fields can be written `command: 30s`.
///
/// Accepts a YAML string. A bare YAML integer is rejected by [`parse`], which is
/// the behaviour we want — see the note there.
pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(d)?;
    parse(&raw).map_err(de::Error::custom)
}

/// As [`deserialize`], for `Option<Duration>` fields.
pub fn deserialize_opt<'de, D>(d: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    match raw {
        None => Ok(None),
        Some(s) => parse(&s).map(Some).map_err(de::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_units_the_spec_uses() {
        // Every duration literal that appears in SPEC.md §4.1.
        assert_eq!(parse("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse("300s").unwrap(), Duration::from_secs(300));
        assert_eq!(parse("600s").unwrap(), Duration::from_secs(600));
        assert_eq!(parse("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse("120s").unwrap(), Duration::from_secs(120));
        assert_eq!(parse("5s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn parses_the_other_units() {
        assert_eq!(parse("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse("2h").unwrap(), Duration::from_secs(7_200));
        assert_eq!(parse("1d").unwrap(), Duration::from_secs(86_400));
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(parse("  30s  ").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn zero_is_allowed() {
        // A zero timeout is a legitimate (if unwise) setting; validation, not
        // parsing, is the place to object to it.
        assert_eq!(parse("0s").unwrap(), Duration::ZERO);
    }

    #[test]
    fn rejects_a_bare_integer() {
        // The important one: `command: 300` must not silently become 300s.
        let err = parse("300").unwrap_err();
        assert!(err.contains("no unit suffix"), "unhelpful message: {err}");
    }

    #[test]
    fn rejects_unknown_units() {
        let err = parse("30sec").unwrap_err();
        assert!(
            err.contains("unknown unit 'sec'"),
            "unhelpful message: {err}"
        );
    }

    #[test]
    fn rejects_empty_and_unit_only() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
        let err = parse("s").unwrap_err();
        assert!(
            err.contains("does not start with a number"),
            "unhelpful message: {err}"
        );
    }

    #[test]
    fn rejects_overflow_rather_than_wrapping() {
        assert!(parse("99999999999999999999d").is_err());
        assert!(parse(&format!("{}d", u64::MAX)).is_err());
    }
}
