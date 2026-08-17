//! The one place a base URL and a request path become a URL.
//!
//! Every request this CLI sends carries a bearer token, so the only question
//! that matters here is *where it is going*. String concatenation cannot answer
//! that: `base + path` happily produces `https://api.ilert.com//evil.example/x`,
//! which is a URL pointing at `evil.example`, and the token goes with it.
//!
//! Request paths come from three untrusted-ish places — the `paths` keys of a
//! spec fetched over the network, the `target` argument of `ilert api`, and
//! path parameters substituted into a template — so the check belongs at the
//! point they all pass through rather than at each of them. [`Endpoint::resolve`]
//! is that point, and [`crate::http::HttpClient`] is the only thing that calls
//! it: no code path can reach the wire without going through here.
//!
//! Earlier checks (`commands::api::resolve_target`, [`validate_spec_path`]) are
//! kept because they produce better messages at the place the caller can act on
//! them. They are not the enforcement.

use anyhow::Result;
use url::{Origin, Url};

use crate::errors::CliError;

/// A validated base URL, parsed once.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// `scheme://host[:port]`, exactly as [`Origin::ascii_serialization`]
    /// writes it. Never carries a path, query, fragment or userinfo.
    authority: String,
    /// The base URL's path prefix without its trailing slash, or `""`. An ilert
    /// instance can live under one (`https://gateway.example.com/ilert`), and a
    /// request must not be able to climb out of it.
    prefix: String,
    origin: Origin,
}

impl Endpoint {
    /// Parse and validate a configured base URL.
    ///
    /// Rejects the three shapes that would make the rest of this module lie:
    ///
    /// - **Credentials** (`https://user:pass@host`) — reqwest would turn them
    ///   into a second `Authorization` header, so the profile would be sending
    ///   an authentication we never chose and never redact.
    /// - **A query string** — it would silently apply to every request, and
    ///   concatenating a path onto it produces a path inside the query rather
    ///   than a path at all.
    /// - **A fragment** — never sent to a server, so it can only mislead.
    pub fn parse(base_url: &str) -> Result<Self> {
        let refuse = |reason: &str| -> anyhow::Error {
            CliError::user(format!("Invalid base URL '{base_url}': {reason}.")).into()
        };

        let url = Url::parse(base_url).map_err(|e| refuse(&e.to_string()))?;

        if !matches!(url.scheme(), "http" | "https") {
            return Err(refuse(&format!(
                "unsupported scheme '{}' (expected http or https)",
                url.scheme()
            )));
        }
        if url.host().is_none() {
            return Err(refuse("it has no host"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refuse(
                "it embeds credentials, which would be sent as a second \
                 authentication on every request. Use --api-key or a profile",
            ));
        }
        if url.query().is_some() {
            return Err(refuse("it has a query string"));
        }
        if url.fragment().is_some() {
            return Err(refuse("it has a fragment"));
        }

        let origin = url.origin();
        Ok(Self {
            authority: origin.ascii_serialization(),
            prefix: url.path().trim_end_matches('/').to_string(),
            origin,
        })
    }

    /// The canonical base URL: origin plus path prefix, no trailing slash.
    pub fn base(&self) -> String {
        format!("{}{}", self.authority, self.prefix)
    }

    /// Resolve a request path against this endpoint.
    ///
    /// The returned URL is guaranteed to share this endpoint's origin and, if
    /// the base carries a path prefix, to sit inside it.
    pub fn resolve(&self, path: &str) -> Result<Url> {
        validate_request_path(path)?;

        let candidate = format!("{}{}{}", self.authority, self.prefix, path);
        let full = Url::parse(&candidate).map_err(|_| {
            CliError::user(format!(
                "Could not resolve path '{path}' against {}.",
                self.base()
            ))
        })?;

        // Belt and braces. The origin cannot change given the checks above, but
        // this is the property everything else in this module exists to
        // protect, so it is asserted against the parsed result rather than
        // inferred from the input.
        if full.origin() != self.origin {
            return Err(CliError::user(format!(
                "Refusing to send a request to '{}', which is not {}.",
                full.origin().ascii_serialization(),
                self.authority
            ))
            .into());
        }
        if !full.username().is_empty() || full.password().is_some() {
            return Err(CliError::user(format!(
                "Refusing path '{path}': it introduces credentials into the URL."
            ))
            .into());
        }

        // `Url` normalises `..` while parsing, so `full.path()` is what the
        // server would actually see — which is where a traversal out of the
        // prefix becomes visible, and the only place it can be checked.
        if !self.prefix.is_empty() {
            let resolved = full.path();
            let inside =
                resolved == self.prefix || resolved.starts_with(&format!("{}/", self.prefix));
            if !inside {
                return Err(CliError::user(format!(
                    "Refusing path '{path}': it resolves to '{resolved}', outside the \
                     base URL's '{}' prefix.",
                    self.prefix
                ))
                .into());
            }
        }

        Ok(full)
    }
}

/// Whether a request path is one we are willing to append to a base URL.
///
/// The rules are deliberately narrow — anything rejected here has a legitimate
/// spelling that is accepted.
pub fn validate_request_path(path: &str) -> Result<()> {
    let refuse = |reason: &str| -> anyhow::Error {
        CliError::user(format!("Refusing request path '{path}': {reason}.")).into()
    };

    if path.is_empty() {
        return Err(refuse("it is empty"));
    }
    if !path.starts_with('/') {
        // An absolute URL is the interesting case, so name it: the caller who
        // typed one is trying to reach another host, not mistyping a path.
        if Url::parse(path).is_ok() {
            return Err(refuse(
                "it is an absolute URL. Pass a path and let the base URL decide the host",
            ));
        }
        return Err(refuse("it must start with '/'"));
    }
    if path.starts_with("//") {
        // `//host/x` is a scheme-relative URL: appended to a base it re-targets
        // the request at `host` while still looking like a path.
        return Err(refuse(
            "it is scheme-relative. Paths must start with exactly one '/'",
        ));
    }
    if path.contains('\\') {
        // WHATWG URL parsing treats a backslash as a separator for http(s), so
        // `/\evil.example` is `//evil.example` wearing a disguise.
        return Err(refuse("it contains a backslash"));
    }
    if path.contains('#') {
        // Everything after a fragment is never sent, so a path carrying one
        // silently addresses something other than what it reads as.
        return Err(refuse("it contains a fragment marker"));
    }
    if let Some(c) = path.chars().find(|c| c.is_control()) {
        // URL parsers strip tab, CR and LF rather than rejecting them, so a
        // path can be split into two different-looking things.
        return Err(refuse(&format!(
            "it contains the control character U+{:04X}",
            c as u32
        )));
    }

    Ok(())
}

/// Validate a `paths` key from an OpenAPI document.
///
/// The spec is fetched over the network and its `paths` keys become request
/// paths verbatim, so a document that declares `"//evil.example/steal"` would
/// otherwise mint a command that sends the caller's token off-origin. Checked
/// where the spec is parsed so the operation never exists, not merely where it
/// would have been sent.
pub fn validate_spec_path(path: &str) -> Result<()> {
    validate_request_path(path)?;
    // Template placeholders are filled by `runner::path_segment`, which
    // percent-encodes; an unbalanced brace means the placeholder would survive
    // into the URL instead.
    if path.matches('{').count() != path.matches('}').count() {
        return Err(CliError::user(format!(
            "Refusing spec path '{path}': it has unbalanced '{{}}' placeholders."
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://api.ilert.com";

    fn endpoint(base: &str) -> Endpoint {
        Endpoint::parse(base).expect("valid base")
    }

    #[test]
    fn ordinary_paths_resolve_onto_the_base() {
        let e = endpoint(BASE);
        assert_eq!(
            e.resolve("/api/alerts").unwrap().as_str(),
            "https://api.ilert.com/api/alerts"
        );
        // A query string in the path stays a query string.
        assert_eq!(
            e.resolve("/api/alerts?states=PENDING").unwrap().as_str(),
            "https://api.ilert.com/api/alerts?states=PENDING"
        );
        // Percent-encoded segments survive untouched.
        assert_eq!(
            e.resolve("/api/users/a%40b.com").unwrap().as_str(),
            "https://api.ilert.com/api/users/a%40b.com"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_base_is_not_doubled() {
        assert_eq!(
            endpoint("https://api.ilert.com/")
                .resolve("/api/alerts")
                .unwrap()
                .as_str(),
            "https://api.ilert.com/api/alerts"
        );
    }

    #[test]
    fn a_scheme_relative_path_cannot_re_target_the_request() {
        let err = endpoint(BASE).resolve("//evil.example/steal").unwrap_err();
        assert!(err.to_string().contains("scheme-relative"), "{err}");
    }

    #[test]
    fn a_backslash_cannot_stand_in_for_a_separator() {
        for path in ["/\\evil.example/x", "/api\\..\\..", "\\\\evil.example"] {
            let err = endpoint(BASE).resolve(path).unwrap_err();
            assert!(
                err.to_string().contains("backslash") || err.to_string().contains("start with '/'"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn control_characters_are_refused_rather_than_stripped() {
        // Url::parse silently removes these, so a path can be made to read as
        // one thing and resolve as another.
        for path in [
            "/api/al\nerts",
            "/api/al\rerts",
            "/api/al\terts",
            "/api/\0x",
        ] {
            let err = endpoint(BASE).resolve(path).unwrap_err();
            assert!(
                err.to_string().contains("control character"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn an_absolute_url_is_refused_with_a_pointed_message() {
        let err = endpoint(BASE)
            .resolve("https://evil.example/x")
            .unwrap_err();
        assert!(err.to_string().contains("absolute URL"), "{err}");
    }

    #[test]
    fn a_fragment_cannot_hide_the_real_path() {
        let err = endpoint(BASE).resolve("/api/alerts#/../../x").unwrap_err();
        assert!(err.to_string().contains("fragment"), "{err}");
    }

    #[test]
    fn traversal_cannot_escape_a_base_path_prefix() {
        let e = endpoint("https://gateway.example.com/ilert");
        assert_eq!(
            e.resolve("/api/alerts").unwrap().as_str(),
            "https://gateway.example.com/ilert/api/alerts"
        );

        for escape in [
            "/../secret",
            "/api/../../secret",
            "/api/%2e%2e/%2e%2e/secret",
            "/..%2f..%2fsecret",
        ] {
            match e.resolve(escape) {
                Ok(url) => assert!(
                    url.path().starts_with("/ilert/"),
                    "{escape} escaped the prefix: {url}"
                ),
                Err(err) => assert!(err.to_string().contains("outside the base URL"), "{err}"),
            }
        }
    }

    #[test]
    fn a_prefix_is_not_matched_by_a_lookalike_sibling() {
        // `/ilert` must not admit `/ilertx`, which is a different application.
        let e = endpoint("https://gateway.example.com/ilert");
        let err = e.resolve("/../ilertx/api").unwrap_err();
        assert!(err.to_string().contains("outside the base URL"), "{err}");
    }

    #[test]
    fn a_base_url_with_credentials_is_refused() {
        let err = Endpoint::parse("https://user:pass@api.ilert.com").unwrap_err();
        assert!(err.to_string().contains("credentials"), "{err}");
        assert!(Endpoint::parse("https://user@api.ilert.com").is_err());
    }

    #[test]
    fn a_base_url_with_a_query_or_fragment_is_refused() {
        assert!(Endpoint::parse("https://api.ilert.com?tenant=1").is_err());
        assert!(Endpoint::parse("https://api.ilert.com#x").is_err());
    }

    #[test]
    fn a_base_url_must_be_http_with_a_host() {
        assert!(Endpoint::parse("ftp://api.ilert.com").is_err());
        assert!(Endpoint::parse("file:///etc/passwd").is_err());
        assert!(Endpoint::parse("not a url").is_err());
        // The scheme check is about resolvability, not about TLS —
        // `config::ensure_secure_base_url` is what refuses cleartext.
        assert!(Endpoint::parse("http://localhost:8080").is_ok());
    }

    #[test]
    fn the_canonical_base_drops_a_trailing_slash_and_keeps_a_prefix() {
        assert_eq!(
            endpoint("https://api.ilert.com/").base(),
            "https://api.ilert.com"
        );
        assert_eq!(
            endpoint("https://gateway.example.com/ilert/").base(),
            "https://gateway.example.com/ilert"
        );
        // A default port is not part of an endpoint's identity.
        assert_eq!(
            endpoint("https://api.ilert.com:443").base(),
            "https://api.ilert.com"
        );
        assert_eq!(
            endpoint("https://api.ilert.com:8443").base(),
            "https://api.ilert.com:8443"
        );
    }

    #[test]
    fn spec_paths_are_held_to_the_same_rule() {
        assert!(validate_spec_path("/alerts/{id}").is_ok());
        assert!(validate_spec_path("//evil.example/steal").is_err());
        assert!(validate_spec_path("alerts").is_err());
        assert!(validate_spec_path("https://evil.example/x").is_err());
        assert!(validate_spec_path("/alerts/{id").is_err());
    }
}
