//! D-083 — the one piece of HTTP policy `axum-reverse-proxy` does not supply:
//! making the upstream's own name in a response point back at the public one.
//!
//! Requests go out as `upstream` + prefix + the public path. Responses have to
//! undo exactly that, in the three places a response names a location:
//!
//! - a `Location` or `Content-Location` naming the upstream would send the
//!   browser straight to it, past this proxy;
//! - a `Set-Cookie` scoped `Domain=` the upstream would be refused by the
//!   browser, which is on the public host;
//! - with a path prefix, a `Location: /tracking/x` or a cookie `Path=/tracking`
//!   names the upstream's path, which the browser would send back here with
//!   the prefix doubled.
//!
//! Each rewrite applies only to an **exact** match on the upstream: host
//! compared case-insensitively, effective port, and the prefix at a path-segment
//! boundary. The redirect a tracking link exists to issue, to the click's real
//! destination, names some other host and must pass through byte for byte. So
//! these functions answer `None` for "leave the header exactly as it was", and
//! that is the common case.

use std::net::IpAddr;

use axum::http::HeaderMap;

/// The upstream as the rewrites need it: the name it answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// `http` or `https`, lower case.
    pub scheme: String,
    /// `host[:port]` as configured, for a URL that has to name the upstream.
    pub authority: String,
    /// Lower case, without brackets for an IPv6 literal.
    pub host: String,
    /// Explicit or the scheme's default.
    pub port: u16,
    /// The configured path with no trailing slash: `""` for none or `/`,
    /// otherwise `/tracking`. The crate joins it the same way.
    pub prefix: String,
}

impl Upstream {
    /// From a `scheme://host[:port][/prefix]` that `config::validate` has
    /// already accepted. `None` only if it has not.
    pub fn parse(upstream: &str) -> Option<Self> {
        let (scheme, rest) = upstream.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let default = default_port(&scheme)?;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        let (host, port) = split_authority(authority)?;
        let path = path.split(['?', '#']).next().unwrap_or("");
        Some(Self {
            host: host.to_ascii_lowercase(),
            port: port.unwrap_or(default),
            authority: authority.to_string(),
            prefix: path.trim_end_matches('/').to_string(),
            scheme,
        })
    }

    /// `tail` (a path, query and fragment, as it follows an authority) with the
    /// prefix removed: the public equivalent. `None` if it lies outside the
    /// prefix, where no public URL reaches it.
    fn strip_prefix(&self, tail: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return Some(tail.to_string());
        }
        let end = tail.find(['?', '#']).unwrap_or(tail.len());
        let (path, rest) = tail.split_at(end);
        let remainder = path.strip_prefix(&self.prefix)?;
        match remainder {
            "" => Some(format!("/{rest}")),
            r if r.starts_with('/') => Some(format!("{r}{rest}")),
            // `/trackingx` shares the characters, not the segment.
            _ => None,
        }
    }
}

/// Where the client thinks it is: what a rewritten URL must name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOrigin {
    pub scheme: String,
    /// `host[:port]` exactly as the client sent it.
    pub authority: String,
}

impl PublicOrigin {
    /// The host part of [`authority`](Self::authority), without the port and
    /// without brackets.
    pub fn host(&self) -> &str {
        split_authority(&self.authority).map_or(&self.authority, |(h, _)| h)
    }
}

/// The public origin of a request, read before it is forwarded.
///
/// The load balancer's `X-Forwarded-Host` and `X-Forwarded-Proto` win, because
/// they describe the name the browser used; this listener only ever sees the
/// load balancer. The first element of each is the client-facing hop. Without
/// them, the `Host` header and the configured `public_scheme`.
pub fn public_origin(headers: &HeaderMap, configured_scheme: &str) -> Option<PublicOrigin> {
    let first = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let authority = first("x-forwarded-host").or_else(|| first("host"))?;
    // A name that would not survive being put back into a URL is not one to put
    // into a redirect.
    split_authority(authority)?;
    let scheme = match first("x-forwarded-proto").map(str::to_ascii_lowercase) {
        Some(s) if s == "http" || s == "https" => s,
        _ => configured_scheme.to_string(),
    };
    Some(PublicOrigin {
        scheme,
        authority: authority.to_string(),
    })
}

/// A `Location` or `Content-Location` value, pointed at the public origin.
/// Query and fragment are kept byte for byte.
///
/// - An absolute or scheme-relative URL on the upstream, inside the prefix, is
///   given the public origin and loses the prefix. Outside the prefix no public
///   URL reaches it, so it is left naming the upstream.
/// - An absolute path (`/tracking/next`) resolves against the public origin in
///   the browser. With no prefix that is already right. With one, a path inside
///   it loses the prefix; a path outside it is made an absolute upstream URL,
///   the only way the browser can still reach what the upstream meant.
/// - Anything else — another host, another port, credentials, a
///   path-relative reference (`next`, `?q`) — is left alone. A path-relative
///   reference resolves against the public path, which maps onto the upstream
///   path segment for segment.
pub fn rewrite_location(value: &str, upstream: &Upstream, public: &PublicOrigin) -> Option<String> {
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    let (scheme, rest) =
        if lower.starts_with("https://") {
            (Some("https"), &value[8..])
        } else if lower.starts_with("http://") {
            (Some("http"), &value[7..])
        } else if let Some(rest) = value.strip_prefix("//") {
            // Scheme-relative: inherits the scheme of the page, which is the public one.
            (None, rest)
        } else if value.starts_with('/') {
            if upstream.prefix.is_empty() {
                return None;
            }
            return Some(upstream.strip_prefix(value).unwrap_or_else(|| {
                format!("{}://{}{value}", upstream.scheme, upstream.authority)
            }));
        } else {
            return None;
        };

    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return None;
    }
    let (host, port) = split_authority(authority)?;
    let effective_port = match (port, scheme) {
        (Some(p), _) => p,
        (None, Some(s)) => default_port(s)?,
        (None, None) => default_port(&upstream.scheme)?,
    };
    if !host.eq_ignore_ascii_case(&upstream.host) || effective_port != upstream.port {
        return None;
    }
    let tail = upstream.strip_prefix(tail)?;

    Some(match scheme {
        Some(_) => format!("{}://{}{tail}", public.scheme, public.authority),
        None => format!("//{}{tail}", public.authority),
    })
}

/// A `Set-Cookie` value rescoped from the upstream to the public name.
///
/// - `Domain=` the upstream host, or a parent domain of it, becomes the public
///   host. An IP-literal public host cannot carry `Domain` at all (RFC 6265
///   §5.1.3), so there it is dropped, leaving the cookie host-only on that
///   address — the nearest equivalent. A cookie with no `Domain` is host-only
///   and already lands on the public host.
/// - With a prefix, `Path=` inside it loses the prefix. A path outside it is
///   left alone: the browser will never send that cookie here, which is also
///   what it would do for the upstream's other paths.
///
/// Every other byte — name, value, `Expires`, `Secure`, `HttpOnly`, `SameSite`
/// and the spacing between them — is kept.
pub fn rewrite_set_cookie(
    value: &str,
    upstream: &Upstream,
    public: &PublicOrigin,
) -> Option<String> {
    let mut segments: Vec<String> = value.split(';').map(str::to_string).collect();
    let mut changed = false;
    let mut drop_index = None;

    // Segment 0 is `name=value`; a cookie *named* Domain or Path is not an
    // attribute.
    for (i, seg) in segments.iter_mut().enumerate().skip(1) {
        let Some((name, attr)) = seg.split_once('=') else {
            continue;
        };
        let attr = attr.trim();
        if name.trim().eq_ignore_ascii_case("domain") {
            let bare = attr.strip_prefix('.').unwrap_or(attr).to_ascii_lowercase();
            let covers_upstream = !bare.is_empty()
                && (upstream.host == bare || upstream.host.ends_with(&format!(".{bare}")));
            if !covers_upstream {
                continue;
            }
            let public_host = public.host();
            if public_host.parse::<IpAddr>().is_ok() {
                drop_index = Some(i);
            } else {
                *seg = format!("{name}={public_host}");
            }
            changed = true;
        } else if name.trim().eq_ignore_ascii_case("path") && !upstream.prefix.is_empty() {
            // A cookie path has no query, so the whole value is the path.
            if let Some(stripped) = upstream.strip_prefix(attr) {
                *seg = format!("{name}={stripped}");
                changed = true;
            }
        }
    }

    if let Some(i) = drop_index {
        segments.remove(i);
    }
    changed.then(|| segments.join(";"))
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

/// `host[:port]` → `(host, port)`, with IPv6 brackets removed. `None` for an
/// empty host or a port that is not a number.
fn split_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        match after {
            "" => (host, None),
            p => (host, Some(p.strip_prefix(':')?)),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(p) => Some(p.parse::<u16>().ok()?),
        None => None,
    };
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream() -> Upstream {
        Upstream::parse("https://Link.Domain2.com").unwrap()
    }

    fn prefixed() -> Upstream {
        Upstream::parse("https://link.domain2.com/tracking/").unwrap()
    }

    fn public() -> PublicOrigin {
        PublicOrigin {
            scheme: "https".to_string(),
            authority: "click.domain1.com".to_string(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn upstream_parses_with_default_and_explicit_ports() {
        assert_eq!(
            upstream(),
            Upstream {
                scheme: "https".into(),
                authority: "Link.Domain2.com".into(),
                host: "link.domain2.com".into(),
                port: 443,
                prefix: String::new(),
            }
        );
        let u = Upstream::parse("http://[::1]:8081").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8081));
        assert!(Upstream::parse("ftp://x").is_none());
    }

    #[test]
    fn a_prefix_is_kept_without_its_trailing_slash() {
        assert_eq!(prefixed().prefix, "/tracking");
        assert_eq!(Upstream::parse("https://h/a/b").unwrap().prefix, "/a/b");
        assert_eq!(Upstream::parse("https://h/").unwrap().prefix, "");
    }

    #[test]
    fn stripping_a_prefix_respects_the_segment_boundary() {
        let u = prefixed();
        assert_eq!(u.strip_prefix("/tracking/x?q#f").as_deref(), Some("/x?q#f"));
        assert_eq!(u.strip_prefix("/tracking").as_deref(), Some("/"));
        assert_eq!(u.strip_prefix("/tracking?q").as_deref(), Some("/?q"));
        assert_eq!(u.strip_prefix("/tracking/").as_deref(), Some("/"));
        assert_eq!(u.strip_prefix("/trackingx"), None);
        assert_eq!(u.strip_prefix("/other"), None);
        assert_eq!(u.strip_prefix(""), None);
    }

    // --- public_origin -----------------------------------------------------

    #[test]
    fn the_load_balancers_forwarded_headers_win() {
        let h = headers(&[
            ("host", "10.0.0.5"),
            ("x-forwarded-host", "click.domain1.com, inner.example"),
            ("x-forwarded-proto", "HTTPS"),
        ]);
        assert_eq!(public_origin(&h, "http"), Some(public()));
    }

    #[test]
    fn without_forwarded_headers_host_and_the_configured_scheme() {
        let h = headers(&[("host", "click.domain1.com:8081")]);
        let o = public_origin(&h, "https").unwrap();
        assert_eq!(o.authority, "click.domain1.com:8081");
        assert_eq!(o.scheme, "https");
        assert_eq!(o.host(), "click.domain1.com");
    }

    #[test]
    fn an_unusable_forwarded_proto_falls_back_to_the_configured_scheme() {
        let h = headers(&[("host", "a.example"), ("x-forwarded-proto", "wss")]);
        assert_eq!(public_origin(&h, "https").unwrap().scheme, "https");
    }

    #[test]
    fn no_host_at_all_means_no_origin() {
        assert_eq!(public_origin(&HeaderMap::new(), "https"), None);
        assert_eq!(public_origin(&headers(&[("host", ":80")]), "https"), None);
    }

    // --- Location ----------------------------------------------------------

    #[test]
    fn a_location_on_the_upstream_is_pointed_at_the_public_origin() {
        assert_eq!(
            rewrite_location(
                "https://link.domain2.com/u/abc?x=1#frag",
                &upstream(),
                &public()
            ),
            Some("https://click.domain1.com/u/abc?x=1#frag".to_string())
        );
    }

    #[test]
    fn the_upstream_host_matches_case_insensitively_and_with_its_default_port_spelled_out() {
        let expected = Some("https://click.domain1.com/x".to_string());
        assert_eq!(
            rewrite_location("HTTPS://LINK.domain2.com/x", &upstream(), &public()),
            expected
        );
        assert_eq!(
            rewrite_location("https://link.domain2.com:443/x", &upstream(), &public()),
            expected
        );
    }

    #[test]
    fn a_bare_upstream_origin_is_rewritten_with_nothing_appended() {
        assert_eq!(
            rewrite_location("https://link.domain2.com", &upstream(), &public()),
            Some("https://click.domain1.com".to_string())
        );
        assert_eq!(
            rewrite_location("https://link.domain2.com?q", &upstream(), &public()),
            Some("https://click.domain1.com?q".to_string())
        );
    }

    #[test]
    fn a_scheme_relative_location_stays_scheme_relative() {
        assert_eq!(
            rewrite_location("//link.domain2.com/x", &upstream(), &public()),
            Some("//click.domain1.com/x".to_string())
        );
    }

    #[test]
    fn the_redirect_to_the_real_destination_passes_through() {
        for v in [
            "https://shop.example/landing?utm=1",
            "https://domain2.com/x",           // the parent, not the upstream
            "https://evil-link.domain2.com/x", // a suffix, not the upstream
            "https://link.domain2.com.evil.example/x",
            "https://link.domain2.com:8443/x", // another port
            "http://link.domain2.com/x",       // another scheme, so port 80
            "https://user@link.domain2.com/x", // credentials
            "/relative/path",                  // no prefix: already public
            "relative",
            "mailto:someone@link.domain2.com",
        ] {
            assert_eq!(rewrite_location(v, &upstream(), &public()), None, "{v}");
        }
    }

    #[test]
    fn with_a_prefix_an_upstream_location_inside_it_loses_the_prefix() {
        let u = prefixed();
        assert_eq!(
            rewrite_location("https://link.domain2.com/tracking/u/abc?x=1", &u, &public()),
            Some("https://click.domain1.com/u/abc?x=1".to_string())
        );
        assert_eq!(
            rewrite_location("https://link.domain2.com/tracking", &u, &public()),
            Some("https://click.domain1.com/".to_string())
        );
    }

    #[test]
    fn with_a_prefix_an_upstream_location_outside_it_still_names_the_upstream() {
        let u = prefixed();
        for v in [
            "https://link.domain2.com/other",
            "https://link.domain2.com/trackingx/y",
            "https://link.domain2.com",
        ] {
            assert_eq!(rewrite_location(v, &u, &public()), None, "{v}");
        }
    }

    #[test]
    fn with_a_prefix_an_absolute_path_is_mapped_back_or_made_absolute() {
        let u = prefixed();
        assert_eq!(
            rewrite_location("/tracking/next?a#b", &u, &public()),
            Some("/next?a#b".to_string())
        );
        assert_eq!(
            rewrite_location("/login", &u, &public()),
            Some("https://link.domain2.com/login".to_string())
        );
        // Path-relative references already resolve inside the prefix.
        assert_eq!(rewrite_location("next", &u, &public()), None);
        assert_eq!(rewrite_location("?page=2", &u, &public()), None);
    }

    // --- Set-Cookie --------------------------------------------------------

    #[test]
    fn a_cookie_scoped_to_the_upstream_is_rescoped_and_nothing_else_moves() {
        assert_eq!(
            rewrite_set_cookie(
                "sid=a=b; Path=/; Domain=link.domain2.com; Secure; HttpOnly; SameSite=Lax",
                &upstream(),
                &public()
            ),
            Some(
                "sid=a=b; Path=/; Domain=click.domain1.com; Secure; HttpOnly; SameSite=Lax"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_parent_domain_a_leading_dot_and_attribute_case_all_match() {
        assert_eq!(
            rewrite_set_cookie("t=1;domain=.Domain2.com;path=/", &upstream(), &public()),
            Some("t=1;domain=click.domain1.com;path=/".to_string())
        );
    }

    #[test]
    fn the_public_port_is_not_part_of_a_cookie_domain() {
        let public = PublicOrigin {
            scheme: "http".into(),
            authority: "click.domain1.com:8081".into(),
        };
        assert_eq!(
            rewrite_set_cookie("t=1; Domain=link.domain2.com", &upstream(), &public),
            Some("t=1; Domain=click.domain1.com".to_string())
        );
    }

    #[test]
    fn an_ip_literal_public_host_makes_the_cookie_host_only() {
        let public = PublicOrigin {
            scheme: "http".into(),
            authority: "127.0.0.1:8081".into(),
        };
        assert_eq!(
            rewrite_set_cookie("t=1; Domain=link.domain2.com; Path=/", &upstream(), &public),
            Some("t=1; Path=/".to_string())
        );
    }

    #[test]
    fn other_cookies_pass_through() {
        for v in [
            "t=1; Path=/", // host-only
            "t=1; Domain=other.example",
            "t=1; Domain=ink.domain2.com", // not a parent: no dot boundary
            "t=1; Domain=sub.link.domain2.com", // a child, not a parent
            "Domain=link.domain2.com; Path=/", // a cookie *named* Domain
            "t=1; Domain=",
            "t=1; Path=/tracking", // no prefix configured
        ] {
            assert_eq!(rewrite_set_cookie(v, &upstream(), &public()), None, "{v}");
        }
    }

    #[test]
    fn with_a_prefix_a_cookie_path_inside_it_loses_the_prefix() {
        let u = prefixed();
        assert_eq!(
            rewrite_set_cookie(
                "t=1; Path=/tracking/u; Domain=link.domain2.com",
                &u,
                &public()
            ),
            Some("t=1; Path=/u; Domain=click.domain1.com".to_string())
        );
        assert_eq!(
            rewrite_set_cookie("t=1; path=/tracking", &u, &public()),
            Some("t=1; path=/".to_string())
        );
        assert_eq!(rewrite_set_cookie("t=1; Path=/other", &u, &public()), None);
        assert_eq!(
            rewrite_set_cookie("Path=/tracking; x=1", &u, &public()),
            None
        );
    }
}
