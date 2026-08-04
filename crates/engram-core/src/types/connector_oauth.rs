//! ADR 0106 addendum: the sealed bundle shape for connector OAuth
//! credentials (`OAuthSubjectKind::Connector`).
//!
//! The store treats every bundle as opaque ciphertext; only the redirect
//! driver and the refresh machinery parse this shape. The refresh spec is
//! sealed INSIDE the bundle so the refresh scanner needs no access to the
//! connector catalog (the orchestrator's trust boundary): the spec holds
//! org-secret REFS, not secrets, and it versions atomically with the token
//! rotation it belongs to. A stale spec (admin edited the facet without
//! reconnecting) self-heals on the next authorization flow.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current bundle schema version. Parsing rejects bundles from the future;
/// unknown fields inside a known version are tolerated.
pub const CONNECTOR_OAUTH_BUNDLE_VERSION: u32 = 1;

/// How to run `grant_type=refresh_token` against the provider. Absent for
/// providers whose access tokens do not expire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOAuthRefreshSpec {
    pub token_url: String,
    /// Org-secret refs for the BYO app's client credentials.
    pub client_id_ref: String,
    pub client_secret_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOAuthBundle {
    pub v: u32,
    pub kind: ConnectorOAuthKind,
    pub access_token: String,
    /// Normalized lowercase; providers report `Bearer`/`bearer`.
    pub token_type: String,
    pub refresh_token: Option<String>,
    /// Canonical joined form; providers may respond with a string or an
    /// array (Linear pre-2026 apps) — [`normalize_scope`] folds both.
    pub scope: Option<String>,
    pub obtained_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub refresh: Option<ConnectorOAuthRefreshSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorOAuthKind {
    Oauth2AuthorizationCode,
}

impl ConnectorOAuthBundle {
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|e| e.to_string())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct VersionProbe {
            v: u32,
        }
        let probe: VersionProbe =
            serde_json::from_slice(bytes).map_err(|e| format!("bundle version probe: {e}"))?;
        if probe.v > CONNECTOR_OAUTH_BUNDLE_VERSION {
            return Err(format!(
                "connector OAuth bundle version {} is newer than supported {}",
                probe.v, CONNECTOR_OAUTH_BUNDLE_VERSION
            ));
        }
        serde_json::from_slice(bytes).map_err(|e| format!("connector OAuth bundle: {e}"))
    }

    /// A bundle refreshes iff the provider issued a refresh token AND the
    /// exchange recorded how to use it.
    pub fn refreshable(&self) -> bool {
        self.refresh_token.is_some() && self.refresh.is_some()
    }
}

/// Fold a token response's `scope` — string or array — into the canonical
/// joined form. Returns `None` for an absent or empty scope.
pub fn normalize_scope(value: Option<&serde_json::Value>, delimiter: &str) -> Option<String> {
    let joined = match value? {
        serde_json::Value::String(s) => s.trim().to_owned(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(delimiter),
        _ => return None,
    };
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> ConnectorOAuthBundle {
        ConnectorOAuthBundle {
            v: CONNECTOR_OAUTH_BUNDLE_VERSION,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: "at".into(),
            token_type: "bearer".into(),
            refresh_token: Some("rt".into()),
            scope: Some("read,write".into()),
            obtained_at: DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            expires_at: Some(
                DateTime::parse_from_rfc3339("2026-08-04T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            refresh: Some(ConnectorOAuthRefreshSpec {
                token_url: "https://api.linear.app/oauth/token".into(),
                client_id_ref: "linear.client_id".into(),
                client_secret_ref: "linear.client_secret".into(),
            }),
        }
    }

    #[test]
    fn round_trips() {
        let b = bundle();
        let parsed = ConnectorOAuthBundle::from_json(&b.to_json().unwrap()).unwrap();
        assert_eq!(parsed, b);
        assert!(parsed.refreshable());
    }

    #[test]
    fn tolerates_unknown_fields_within_version() {
        let mut value = serde_json::to_value(bundle()).unwrap();
        value["future_field"] = serde_json::json!("ignored");
        let parsed = ConnectorOAuthBundle::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed, bundle());
    }

    #[test]
    fn rejects_future_version() {
        let mut value = serde_json::to_value(bundle()).unwrap();
        value["v"] = serde_json::json!(CONNECTOR_OAUTH_BUNDLE_VERSION + 1);
        let err =
            ConnectorOAuthBundle::from_json(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(err.contains("newer than supported"), "{err}");
    }

    #[test]
    fn non_refreshing_bundle() {
        let b = ConnectorOAuthBundle {
            refresh_token: None,
            refresh: None,
            expires_at: None,
            ..bundle()
        };
        assert!(!b.refreshable());
        let parsed = ConnectorOAuthBundle::from_json(&b.to_json().unwrap()).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn scope_normalizes_string_and_array() {
        let s = serde_json::json!("read write");
        assert_eq!(
            normalize_scope(Some(&s), ","),
            Some("read write".to_owned())
        );
        let arr = serde_json::json!(["read", " write ", ""]);
        assert_eq!(
            normalize_scope(Some(&arr), ","),
            Some("read,write".to_owned())
        );
        assert_eq!(normalize_scope(Some(&serde_json::json!("")), ","), None);
        assert_eq!(normalize_scope(Some(&serde_json::json!(42)), ","), None);
        assert_eq!(normalize_scope(None, ","), None);
    }
}
