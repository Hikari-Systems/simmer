//! Parsing a message as a downstream stored it.

use std::collections::BTreeMap;

/// The unfolded value of the first occurrence of a header, case-insensitively.
pub fn header(raw: &str, name: &str) -> Option<String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<(String, String)> = None;

    for line in raw.replace("\r\n", "\n").lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = current.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = current.take() {
            headers.entry(k).or_insert(v);
        }
        if let Some((k, v)) = line.split_once(':') {
            current = Some((k.to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    if let Some((k, v)) = current.take() {
        headers.entry(k).or_insert(v);
    }

    headers.get(&name.to_ascii_lowercase()).cloned()
}

/// Every `Received:` header, unfolded, in the order they appear (newest first).
/// Unfolding joins a continuation with one space, which is exactly what a fold
/// at a space took out.
pub fn received(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines
        .into_iter()
        .filter(|l| l.to_ascii_lowercase().starts_with("received:"))
        .collect()
}

/// The message's lines with the named headers — and their continuation lines —
/// removed, for a byte comparison that ignores what is allowed to differ.
pub fn without_headers(raw: &str, excluded: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skipping = false;
    let mut in_headers = true;
    for line in raw.replace("\r\n", "\n").lines() {
        if in_headers && line.is_empty() {
            in_headers = false;
            skipping = false;
        }
        if !in_headers {
            out.push(line.to_string());
            continue;
        }
        let continuation = line.starts_with(' ') || line.starts_with('\t');
        if continuation {
            if !skipping {
                out.push(line.to_string());
            }
            continue;
        }
        skipping = line
            .split_once(':')
            .map(|(name, _)| excluded.iter().any(|e| e.eq_ignore_ascii_case(name)))
            .unwrap_or(false);
        if !skipping {
            out.push(line.to_string());
        }
    }
    out
}
