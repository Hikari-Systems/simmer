//! Just enough XML for a listing page: the text of named elements, in order,
//! and the five predefined entities. The documents are the providers' own, the
//! values are object keys and timestamps, and none of them nests a tag inside
//! another of the same name.

/// Every `<tag>…</tag>` inside `scope`, in order, with entities decoded.
pub fn texts(doc: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(decode(&after[..end]));
        rest = &after[end + close.len()..];
    }
    out
}

/// Every `<tag>…</tag>` block's inner text, undecoded, for nested lookups.
pub fn blocks<'a>(doc: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

pub fn first(doc: &str, tag: &str) -> Option<String> {
    texts(doc, tag).into_iter().next()
}

fn decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_and_entities() {
        let doc = "<a><Key>x&amp;y</Key><Key>z</Key></a><Next>t</Next>";
        assert_eq!(texts(doc, "Key"), vec!["x&y", "z"]);
        assert_eq!(first(doc, "Next").as_deref(), Some("t"));
        assert_eq!(first(doc, "Missing"), None);
        assert_eq!(blocks(doc, "a"), vec!["<Key>x&amp;y</Key><Key>z</Key>"]);
    }
}
