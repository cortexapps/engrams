//! Host allow-list matching.
//!
//! Two forms come from the manifest:
//!
//! - **Exact**: `allow_hosts = ["api.github.com"]`. Case-insensitive
//!   exact match against the SNI / Host header.
//! - **Pattern**: `allow_host_patterns = ["*.githubusercontent.com"]`.
//!   Single leading wildcard label; matches anything `<x>.foo.com`
//!   where `<x>` has no embedded dot. We deliberately don't support
//!   arbitrary glob syntax — `*` mid-string or multi-label wildcards
//!   are an attack-surface footgun (e.g. `*.com` would allow every
//!   `.com` host) and microsandbox doesn't either.
//!
//! A `HostList` is the union of both forms; it derives a single
//! `matches(hostname)` so callers don't have to fan out.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HostSpec {
    /// Exact case-insensitive match.
    Exact(String),
    /// Leading-wildcard pattern: `*.example.com`. The `*` matches
    /// exactly one label (no embedded dot).
    Wildcard {
        /// `.example.com` (with the leading dot, no leading `*`).
        suffix: String,
    },
}

impl HostSpec {
    /// Build from a manifest string, classifying into Exact or
    /// Wildcard. Returns `Err` for unsupported shapes (`*foo`,
    /// `foo.*`, `*`, empty string) so a stale manifest fails fast at
    /// session-create rather than silently allowing too much.
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        if let Some(rest) = s.strip_prefix("*.") {
            if rest.is_empty() || rest.contains('*') {
                return Err(ParseError::UnsupportedWildcard(raw.into()));
            }
            return Ok(Self::Wildcard {
                suffix: format!(".{}", rest.to_ascii_lowercase()),
            });
        }
        if s.contains('*') {
            return Err(ParseError::UnsupportedWildcard(raw.into()));
        }
        Ok(Self::Exact(s.to_ascii_lowercase()))
    }

    pub fn matches(&self, hostname: &str) -> bool {
        let h = hostname.to_ascii_lowercase();
        match self {
            Self::Exact(want) => want == &h,
            Self::Wildcard { suffix } => {
                // h must end in `.example.com` AND have exactly one
                // label before — no embedded dot.
                let Some(label) = h.strip_suffix(suffix) else {
                    return false;
                };
                !label.is_empty() && !label.contains('.')
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ParseError {
    Empty,
    UnsupportedWildcard(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty allow_hosts entry"),
            Self::UnsupportedWildcard(s) => write!(
                f,
                "unsupported allow_hosts wildcard `{s}` — only `*.foo.com` is supported"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Bundle of `allow_hosts` + `allow_host_patterns`. `matches` returns
/// true if any element matches.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostList {
    pub specs: Vec<HostSpec>,
}

impl HostList {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from the manifest's two parallel fields. Every entry is
    /// validated; the first `ParseError` short-circuits.
    pub fn from_manifest(
        allow_hosts: &[String],
        allow_host_patterns: &[String],
    ) -> Result<Self, ParseError> {
        let mut specs = Vec::with_capacity(allow_hosts.len() + allow_host_patterns.len());
        for h in allow_hosts {
            specs.push(HostSpec::parse(h)?);
        }
        for p in allow_host_patterns {
            // Patterns must be wildcards. An exact-form value here is
            // a config error in the manifest — surface it.
            let spec = HostSpec::parse(p)?;
            if matches!(spec, HostSpec::Exact(_)) {
                return Err(ParseError::UnsupportedWildcard(format!(
                    "{p} (in allow_host_patterns; use allow_hosts for exact matches)"
                )));
            }
            specs.push(spec);
        }
        Ok(Self { specs })
    }

    pub fn matches(&self, hostname: &str) -> bool {
        self.specs.iter().any(|s| s.matches(hostname))
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_case_insensitive() {
        let s = HostSpec::parse("Api.GitHub.com").unwrap();
        assert!(s.matches("api.github.com"));
        assert!(s.matches("API.GITHUB.COM"));
        assert!(!s.matches("evil.api.github.com"));
        assert!(!s.matches("github.com"));
    }

    #[test]
    fn wildcard_matches_one_label() {
        let s = HostSpec::parse("*.example.com").unwrap();
        assert!(s.matches("foo.example.com"));
        assert!(s.matches("bar.example.com"));
        // Multi-label must not match.
        assert!(!s.matches("a.b.example.com"));
        // The bare suffix must not match (no preceding label).
        assert!(!s.matches("example.com"));
    }

    #[test]
    fn rejects_unsupported_wildcards() {
        assert!(HostSpec::parse("*.com").is_ok()); // syntactically valid but very loose; allowed.
        assert!(HostSpec::parse("*").is_err());
        assert!(HostSpec::parse("foo.*").is_err());
        assert!(HostSpec::parse("*.foo.*").is_err());
        assert!(HostSpec::parse("a*b.com").is_err());
        assert!(HostSpec::parse("").is_err());
        assert!(HostSpec::parse("   ").is_err());
    }

    #[test]
    fn host_list_unions_exact_and_wildcard() {
        let l = HostList::from_manifest(
            &["api.github.com".into()],
            &["*.githubusercontent.com".into()],
        )
        .unwrap();
        assert!(l.matches("api.github.com"));
        assert!(l.matches("raw.githubusercontent.com"));
        assert!(!l.matches("api.evil.com"));
        assert!(!l.matches("a.b.githubusercontent.com"));
    }

    #[test]
    fn allow_host_patterns_must_be_wildcards() {
        // Exact entries belong in allow_hosts; rejecting them in
        // patterns is a config-correctness check.
        let err = HostList::from_manifest(&[], &["api.github.com".into()]).unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedWildcard(_)));
    }

    #[test]
    fn empty_list_matches_nothing() {
        let l = HostList::empty();
        assert!(!l.matches("api.github.com"));
        assert!(l.is_empty());
    }
}
