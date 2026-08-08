//! `${ENV_VAR}` interpolation, per `SPEC.md` §4:
//!
//! > Secrets are supplied by `${ENV_VAR}` interpolation, resolved at startup;
//! > an unresolvable reference is a fatal startup error.
//!
//! Interpolation runs over the parsed YAML tree rather than over the raw text.
//! Doing it textually would let a secret containing `:` or a newline change the
//! document's structure — a password ending in `\n  routes:` would be a config
//! injection. Substituting into already-parsed scalars makes that impossible.
//!
//! §4.2 requires that *all* violations be reported, not just the first, so this
//! collects every unresolvable reference before returning.

use std::collections::BTreeSet;

use serde_yaml_ng::Value;

/// Where an unresolvable reference was found, and what it asked for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unresolved {
    /// Dotted path to the offending scalar, e.g. `routes[0].downstream.auth.password`.
    pub path: String,
    /// The environment variable name that could not be resolved.
    pub var: String,
}

/// Resolve every `${VAR}` in every string in the tree.
///
/// Returns the names and locations of all references that could not be resolved.
/// An empty return means the tree is fully interpolated.
pub fn interpolate(value: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<Unresolved> {
    let mut missing = BTreeSet::new();
    walk(value, &mut String::from("$"), lookup, &mut missing);
    missing.into_iter().collect()
}

/// Resolve against the process environment.
pub fn interpolate_from_env(value: &mut Value) -> Vec<Unresolved> {
    interpolate(value, &|name| std::env::var(name).ok())
}

fn walk(
    value: &mut Value,
    path: &mut String,
    lookup: &dyn Fn(&str) -> Option<String>,
    missing: &mut BTreeSet<Unresolved>,
) {
    match value {
        Value::String(s) => {
            if let Some(replaced) = substitute(s, path, lookup, missing) {
                *s = replaced;
            }
        }
        Value::Sequence(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let mark = path.len();
                path.push_str(&format!("[{i}]"));
                walk(item, path, lookup, missing);
                path.truncate(mark);
            }
        }
        Value::Mapping(map) => {
            // Keys are not interpolated. A `${VAR}` as a config *key* would let
            // the environment decide the shape of the document rather than its
            // values, which is not what §4 describes.
            for (k, v) in map.iter_mut() {
                let mark = path.len();
                match k.as_str() {
                    Some(name) => path.push_str(&format!(".{name}")),
                    None => path.push_str(".<non-string key>"),
                }
                walk(v, path, lookup, missing);
                path.truncate(mark);
            }
        }
        _ => {}
    }
}

/// Replace every `${NAME}` in `s`. Returns `None` when there is nothing to do,
/// so untouched scalars are not needlessly reallocated.
fn substitute(
    s: &str,
    path: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    missing: &mut BTreeSet<Unresolved>,
) -> Option<String> {
    if !s.contains("${") {
        return None;
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];

        let Some(end) = after.find('}') else {
            // An unterminated `${` is a literal, not a reference. Emitting it
            // verbatim keeps a password that genuinely contains "${" working.
            out.push_str(&rest[start..]);
            return Some(out);
        };

        let name = &after[..end];
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => {
                missing.insert(Unresolved {
                    path: path.to_string(),
                    var: name.to_string(),
                });
                // Leave the reference in place; the caller is going to abort
                // anyway, and keeping it makes the error message readable.
                out.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn yaml(s: &str) -> Value {
        serde_yaml_ng::from_str(s).expect("test yaml parses")
    }

    #[test]
    fn substitutes_a_whole_scalar() {
        let mut v = yaml("password: ${SECRET}");
        let missing = interpolate(&mut v, &env(&[("SECRET", "hunter2")]));
        assert!(missing.is_empty());
        assert_eq!(v["password"].as_str(), Some("hunter2"));
    }

    #[test]
    fn substitutes_within_a_larger_string() {
        // §4.1 writes e.g. "bounce+{{...}}@newbrand.com"; env refs can be partial too.
        let mut v = yaml("url: postgres://${USER}:${PASS}@db/simmer");
        let missing = interpolate(&mut v, &env(&[("USER", "simmer"), ("PASS", "p@ss")]));
        assert!(missing.is_empty());
        assert_eq!(v["url"].as_str(), Some("postgres://simmer:p@ss@db/simmer"));
    }

    #[test]
    fn recurses_through_sequences_and_mappings() {
        let mut v = yaml(
            r#"
routes:
  - downstream:
      auth:
        password: ${POSTAL_PASS}
  - downstream:
      auth:
        password: ${SENDGRID_KEY}
"#,
        );
        let missing = interpolate(
            &mut v,
            &env(&[("POSTAL_PASS", "aaa"), ("SENDGRID_KEY", "bbb")]),
        );
        assert!(missing.is_empty());
        assert_eq!(
            v["routes"][0]["downstream"]["auth"]["password"].as_str(),
            Some("aaa")
        );
        assert_eq!(
            v["routes"][1]["downstream"]["auth"]["password"].as_str(),
            Some("bbb")
        );
    }

    #[test]
    fn reports_every_unresolvable_reference_not_just_the_first() {
        // §4.2: "Report all violations, not just the first."
        let mut v = yaml(
            r#"
database:
  url: ${DATABASE_URL}
admin:
  auth_token: ${SIMMER_ADMIN_TOKEN}
routes:
  - downstream:
      auth:
        password: ${POSTAL_PASS}
"#,
        );
        let missing = interpolate(&mut v, &env(&[]));

        // Reported in document-path order rather than by variable name, so the
        // list reads top-to-bottom against the file the operator is looking at.
        let paths: Vec<&str> = missing.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "$.admin.auth_token",
                "$.database.url",
                "$.routes[0].downstream.auth.password",
            ]
        );

        let vars: Vec<&str> = missing.iter().map(|m| m.var.as_str()).collect();
        assert_eq!(vars, ["SIMMER_ADMIN_TOKEN", "DATABASE_URL", "POSTAL_PASS"]);
    }

    #[test]
    fn reports_a_usable_path_for_each_failure() {
        let mut v = yaml(
            r#"
routes:
  - downstream:
      auth:
        password: ${POSTAL_PASS}
"#,
        );
        let missing = interpolate(&mut v, &env(&[]));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].path, "$.routes[0].downstream.auth.password");
        assert_eq!(missing[0].var, "POSTAL_PASS");
    }

    #[test]
    fn resolves_what_it_can_even_when_others_fail() {
        let mut v = yaml("a: ${SET}\nb: ${UNSET}");
        let missing = interpolate(&mut v, &env(&[("SET", "yes")]));
        assert_eq!(missing.len(), 1);
        assert_eq!(v["a"].as_str(), Some("yes"));
        // Unresolved references are left verbatim so the error reads sensibly.
        assert_eq!(v["b"].as_str(), Some("${UNSET}"));
    }

    #[test]
    fn a_secret_containing_yaml_cannot_restructure_the_document() {
        // The reason interpolation runs over the parsed tree, not the raw text.
        let mut v = yaml("password: ${SECRET}\nstrict_senders: true");
        let missing = interpolate(
            &mut v,
            &env(&[("SECRET", "p\nstrict_senders: false\nevil: yes")]),
        );
        assert!(missing.is_empty());
        assert_eq!(
            v["password"].as_str(),
            Some("p\nstrict_senders: false\nevil: yes")
        );
        // The injected keys are inert: still one mapping with two entries.
        assert_eq!(v["strict_senders"].as_bool(), Some(true));
        assert!(v.get("evil").is_none());
        assert_eq!(v.as_mapping().unwrap().len(), 2);
    }

    #[test]
    fn leaves_non_references_alone() {
        let mut v = yaml(
            r#"
plain: "no refs here"
brace: "a { b } c"
dollar: "costs $5"
unterminated: "${OOPS"
"#,
        );
        let missing = interpolate(&mut v, &env(&[]));
        assert!(missing.is_empty(), "unterminated ${{ is a literal");
        assert_eq!(v["unterminated"].as_str(), Some("${OOPS"));
        assert_eq!(v["dollar"].as_str(), Some("costs $5"));
    }

    #[test]
    fn does_not_interpolate_mapping_keys() {
        let mut v = yaml("${KEY}: value");
        let missing = interpolate(&mut v, &env(&[("KEY", "substituted")]));
        assert!(missing.is_empty());
        assert!(v.get("${KEY}").is_some(), "key should be untouched");
        assert!(v.get("substituted").is_none());
    }

    #[test]
    fn substitution_result_is_not_rescanned() {
        // A secret whose value contains "${OTHER}" must not trigger a second
        // lookup — that would leak one variable's contents into another's slot.
        let mut v = yaml("a: ${OUTER}");
        let missing = interpolate(
            &mut v,
            &env(&[("OUTER", "${INNER}"), ("INNER", "should not appear")]),
        );
        assert!(missing.is_empty());
        assert_eq!(v["a"].as_str(), Some("${INNER}"));
    }

    #[test]
    fn duplicate_references_to_one_missing_var_report_once_per_site() {
        let mut v = yaml("a: ${X}\nb: ${X}");
        let missing = interpolate(&mut v, &env(&[]));
        assert_eq!(missing.len(), 2, "each site is separately actionable");
        assert!(missing.iter().all(|m| m.var == "X"));
    }
}
