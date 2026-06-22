//! Profile-granted integration capabilities (ADR 0056).
//!
//! A [`Capability`] is the unit a profile grants and the broker clamps to:
//! a `(provider, action, resource)` triple, written
//! `provider:action[@resource]` on the wire —
//!
//! ```text
//! github:contents:write@cortexapps/engrams
//! github:issues:write
//! datadog:logs:read
//! s3:read@my-bucket
//! ```
//!
//! Capabilities bind server-side to a session in Postgres at create
//! (`session_capabilities`); the broker reads the bound set and **clamps**
//! every guest request to it (the "server decides the scope" invariant).
//! [`Capability::clamp`] and [`Capability::covers`] are pure and unit-tested
//! here — they are the load-bearing enforcement primitive, exercised at the
//! seam in a later phase (this phase only binds; nothing enforces yet).

use serde::{Deserialize, Serialize};

/// A `(provider, action, resource)` grant. `action` may itself contain
/// colons (`contents:write`); `resource` is an optional scope refinement
/// (`None` = the provider's default scope; `Some("org/repo")` / `"org/*"`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Capability {
    pub provider: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// A capability string that doesn't parse as `provider:action[@resource]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityError(pub String);

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CapabilityError {}

impl Capability {
    /// Parse `provider:action[@resource]`. The resource is split off at the
    /// first `@` (so `action` may contain `:` but not `@`); `provider` is the
    /// segment before the first `:` and `action` is the (possibly
    /// colon-bearing) remainder. All three non-resource parts must be
    /// non-empty.
    pub fn parse(s: &str) -> Result<Self, CapabilityError> {
        let (head, resource) = match s.split_once('@') {
            Some((h, r)) if !r.is_empty() => (h, Some(r.to_string())),
            Some(_) => {
                return Err(CapabilityError(format!(
                    "capability `{s}` has an empty resource after `@`"
                )))
            }
            None => (s, None),
        };
        let (provider, action) = head.split_once(':').ok_or_else(|| {
            CapabilityError(format!(
                "capability `{s}` must be `provider:action[@resource]`"
            ))
        })?;
        if provider.is_empty() || action.is_empty() {
            return Err(CapabilityError(format!(
                "capability `{s}` has an empty provider or action"
            )));
        }
        Ok(Self {
            provider: provider.to_string(),
            action: action.to_string(),
            resource,
        })
    }

    /// The canonical `provider:action[@resource]` string.
    pub fn to_wire(&self) -> String {
        match &self.resource {
            Some(r) => format!("{}:{}@{}", self.provider, self.action, r),
            None => format!("{}:{}", self.provider, self.action),
        }
    }

    /// Does `self` (a *bound* capability) cover `req` (a *requested* one)?
    /// Same provider + action, and the bound resource encompasses the
    /// requested resource:
    ///
    /// - bound `resource == None` → no resource restriction → covers any
    ///   request (with or without a resource).
    /// - bound `Some(pattern)` → the request must name a resource the
    ///   pattern matches ([`resource_matches`]); a request with no resource
    ///   (the broader ask) is **not** covered by a resource-restricted grant.
    pub fn covers(&self, req: &Capability) -> bool {
        if self.provider != req.provider || self.action != req.action {
            return false;
        }
        match (&self.resource, &req.resource) {
            (None, _) => true,
            (Some(pattern), Some(value)) => resource_matches(pattern, value),
            (Some(_), None) => false,
        }
    }

    /// Clamp `requested` to `bound`: keep only the requested capabilities
    /// some bound capability covers. Set-intersection at the
    /// `(provider, action, resource)` grain — the guest can never end up
    /// with more than its profile granted. (Resource *narrowing* beyond
    /// the requested value is a later-phase refinement; this keeps the
    /// covered request verbatim.)
    pub fn clamp(requested: &[Capability], bound: &[Capability]) -> Vec<Capability> {
        requested
            .iter()
            .filter(|r| bound.iter().any(|b| b.covers(r)))
            .cloned()
            .collect()
    }
}

/// Match a resource `pattern` from a bound capability against a request's
/// `value`. Supports exact match, `*` (any), and a trailing `/*` prefix glob
/// (`cortexapps/*` covers `cortexapps` and `cortexapps/engrams`). Deliberately
/// small — richer globbing can grow with real providers.
fn resource_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return value == prefix
            || value
                .strip_prefix(prefix)
                .is_some_and(|r| r.starts_with('/'));
    }
    pattern == value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_action_resource() {
        let c = Capability::parse("github:contents:write@cortexapps/engrams").unwrap();
        assert_eq!(c.provider, "github");
        assert_eq!(c.action, "contents:write"); // action keeps its colon
        assert_eq!(c.resource.as_deref(), Some("cortexapps/engrams"));
        assert_eq!(c.to_wire(), "github:contents:write@cortexapps/engrams");
    }

    #[test]
    fn parses_without_resource() {
        let c = Capability::parse("datadog:logs:read").unwrap();
        assert_eq!(c.provider, "datadog");
        assert_eq!(c.action, "logs:read");
        assert_eq!(c.resource, None);
        assert_eq!(c.to_wire(), "datadog:logs:read");
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "github", "github:", ":write", "github:write@", "@x"] {
            assert!(
                Capability::parse(bad).is_err(),
                "expected `{bad}` to reject"
            );
        }
    }

    #[test]
    fn covers_requires_same_provider_and_action() {
        let bound = Capability::parse("github:issues:write").unwrap();
        assert!(bound.covers(&Capability::parse("github:issues:write").unwrap()));
        assert!(!bound.covers(&Capability::parse("github:contents:write").unwrap()));
        assert!(!bound.covers(&Capability::parse("datadog:issues:write").unwrap()));
    }

    #[test]
    fn unrestricted_resource_covers_anything() {
        let bound = Capability::parse("github:contents:write").unwrap();
        assert!(bound.covers(&Capability::parse("github:contents:write").unwrap()));
        assert!(bound.covers(&Capability::parse("github:contents:write@a/b").unwrap()));
    }

    #[test]
    fn resource_glob_and_exact() {
        let org = Capability::parse("github:contents:write@cortexapps/*").unwrap();
        assert!(org.covers(&Capability::parse("github:contents:write@cortexapps/engrams").unwrap()));
        assert!(org.covers(&Capability::parse("github:contents:write@cortexapps").unwrap()));
        assert!(!org.covers(&Capability::parse("github:contents:write@other/repo").unwrap()));

        let exact = Capability::parse("github:contents:write@cortexapps/engrams").unwrap();
        assert!(
            exact.covers(&Capability::parse("github:contents:write@cortexapps/engrams").unwrap())
        );
        assert!(
            !exact.covers(&Capability::parse("github:contents:write@cortexapps/other").unwrap())
        );
        // A resource-restricted grant does NOT cover the broader (no-resource) ask.
        assert!(!exact.covers(&Capability::parse("github:contents:write").unwrap()));
    }

    #[test]
    fn clamp_keeps_only_covered_requests() {
        let bound = vec![
            Capability::parse("github:contents:read").unwrap(),
            Capability::parse("datadog:logs:read@*").unwrap(),
        ];
        let requested = vec![
            Capability::parse("github:contents:read").unwrap(), // covered
            Capability::parse("github:contents:write").unwrap(), // NOT granted → dropped
            Capability::parse("datadog:logs:read@idx-1").unwrap(), // covered by @*
        ];
        let clamped = Capability::clamp(&requested, &bound);
        assert_eq!(clamped.len(), 2);
        assert!(clamped.iter().any(|c| c.action == "contents:read"));
        assert!(clamped.iter().any(|c| c.provider == "datadog"));
        assert!(!clamped.iter().any(|c| c.action == "contents:write"));
    }
}
