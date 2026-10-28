//! The client-credentials grant family (RFC 6749 section 4.4) for
//! connector subjects: a provider whose API credentials are a client id and
//! secret with no user consent (Vanta). There is no browser round trip and
//! no refresh token. The grant runs once at connect time and again, from
//! the sealed refresh spec, whenever the refresh sweep or an on-demand
//! resolution finds the token inside its margin (`oauth_refresh`).
//!
//! The spec arrives over the wire from the orchestrator, which enforced
//! host containment at the connector parse boundary; this module re-checks
//! shape only and is the sole tier that resolves the client secrets.

use std::collections::BTreeMap;

use engram_core::types::connector_oauth::{
    ConnectorOAuthBundle, ConnectorOAuthKind, ConnectorOAuthRefreshSpec, TokenRequestBody,
    CONNECTOR_OAUTH_BUNDLE_VERSION,
};
use engram_core::types::oauth::{
    OAuthAccountMetadata, OAuthCredentialKey, OAuthSubjectKind, SealedOAuthCredential,
};

use crate::oauth::{validate_key, OAuthManager, OAuthServiceError, ValidatedOAuthBundle};
use crate::oauth_redirect::{
    extract_mapped, validate_https_url, RedirectMetadataSpec, MAX_SCOPES, META_ACCOUNT_ID,
    META_DISPLAY_NAME, META_WORKSPACE_ID, META_WORKSPACE_NAME,
};

/// Wire-supplied, orchestrator-validated client-credentials spec (the
/// connector's oauth facet in coordinator terms).
#[derive(Clone, Debug)]
pub struct ClientCredentialsSpec {
    pub token_url: String,
    pub scopes: Vec<String>,
    /// Empty means space, the RFC delimiter.
    pub scope_delimiter: String,
    pub client_id_ref: String,
    pub client_secret_ref: String,
    pub body: TokenRequestBody,
    pub metadata: RedirectMetadataSpec,
}

impl ClientCredentialsSpec {
    pub fn delimiter(&self) -> &str {
        if self.scope_delimiter.is_empty() {
            " "
        } else {
            &self.scope_delimiter
        }
    }

    /// The joined scope the grant asks for; `None` when the facet lists
    /// none (the provider then grants its default).
    fn joined_scope(&self) -> Option<String> {
        if self.scopes.is_empty() {
            None
        } else {
            Some(self.scopes.join(self.delimiter()))
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_https_url(&self.token_url, "token_url")?;
        if self.scopes.len() > MAX_SCOPES {
            return Err(format!("too many scopes (max {MAX_SCOPES})"));
        }
        if self
            .scopes
            .iter()
            .any(|s| s.is_empty() || s.chars().any(char::is_whitespace))
        {
            return Err("scopes must be non-empty and whitespace-free".into());
        }
        if !matches!(self.scope_delimiter.as_str(), "" | "," | " ") {
            return Err("scope_delimiter must be \",\" or \" \"".into());
        }
        if self.client_id_ref.trim().is_empty() || self.client_secret_ref.trim().is_empty() {
            return Err("client id/secret refs are required".into());
        }
        self.metadata.validate()
    }
}

impl OAuthManager {
    /// Run the grant and seal the obtained token for `key`. A later call
    /// replaces the credential (a reconnect with new secrets); the sealed
    /// refresh spec carries the scope and encoding so the sweep can run the
    /// same grant without the connector catalog.
    pub async fn grant_client_credentials(
        &self,
        key: &OAuthCredentialKey,
        spec: &ClientCredentialsSpec,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        validate_key(key)?;
        if key.subject_kind != OAuthSubjectKind::Connector {
            return Err(OAuthServiceError::BadRequest(
                "client-credentials grants are restricted to connector subjects".into(),
            ));
        }
        spec.validate().map_err(OAuthServiceError::BadRequest)?;
        let client_id = self.resolve_org_secret(&spec.client_id_ref).await?;
        let client_secret = self.resolve_org_secret(&spec.client_secret_ref).await?;
        let scope = spec.joined_scope();

        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "client_credentials".into()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ];
        if let Some(scope) = &scope {
            form.push(("scope", scope.clone()));
        }
        let grant = self
            .token_grant(&spec.token_url, &form, spec.delimiter(), "", spec.body)
            .await?;

        let mut fields = BTreeMap::new();
        extract_mapped(&spec.metadata.from_token_response, &grant.json, &mut fields);
        if let Some(probe) = &spec.metadata.probe {
            let json = self.run_probe(probe, &grant.access_token).await?;
            extract_mapped(&probe.map, &json, &mut fields);
        }
        let metadata = OAuthAccountMetadata {
            // A client-credentials app has no account of its own to switch
            // between; an unmapped id degrades to the subject, as for the
            // redirect family.
            account_id: fields
                .get(META_ACCOUNT_ID)
                .cloned()
                .unwrap_or_else(|| key.subject_id.clone()),
            display_name: fields.get(META_DISPLAY_NAME).cloned(),
            plan_type: None,
            workspace_id: fields.get(META_WORKSPACE_ID).cloned(),
            workspace_name: fields.get(META_WORKSPACE_NAME).cloned(),
        };

        let bundle = ConnectorOAuthBundle {
            v: CONNECTOR_OAUTH_BUNDLE_VERSION,
            kind: ConnectorOAuthKind::Oauth2ClientCredentials,
            access_token: grant.access_token,
            token_type: grant.token_type,
            refresh_token: None,
            scope: grant.scope.or(scope.clone()),
            obtained_at: self.clock.now_utc(),
            expires_at: grant.expires_at,
            refresh: Some(ConnectorOAuthRefreshSpec {
                token_url: spec.token_url.clone(),
                client_id_ref: spec.client_id_ref.clone(),
                client_secret_ref: spec.client_secret_ref.clone(),
                scope,
                body: spec.body,
            }),
        };
        let payload = bundle
            .to_json()
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        self.publish_bundle(
            key,
            ValidatedOAuthBundle {
                payload,
                metadata,
                expires_at: bundle.expires_at,
            },
        )
        .await?;
        self.meta
            .get_oauth_credential(key)
            .await?
            .ok_or(OAuthServiceError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ClientCredentialsSpec {
        ClientCredentialsSpec {
            token_url: "https://api.vanta.com/oauth/token".into(),
            scopes: vec!["vanta-api.all:read".into(), "vanta-api.all:write".into()],
            scope_delimiter: String::new(),
            client_id_ref: "vanta.client_id".into(),
            client_secret_ref: "vanta.client_secret".into(),
            body: TokenRequestBody::Json,
            metadata: RedirectMetadataSpec::default(),
        }
    }

    #[test]
    fn scope_joins_with_a_space_by_default() {
        assert_eq!(
            base().joined_scope().as_deref(),
            Some("vanta-api.all:read vanta-api.all:write")
        );
        let mut comma = base();
        comma.scope_delimiter = ",".into();
        assert_eq!(
            comma.joined_scope().as_deref(),
            Some("vanta-api.all:read,vanta-api.all:write")
        );
        let mut none = base();
        none.scopes.clear();
        assert_eq!(none.joined_scope(), None);
    }

    #[test]
    fn spec_validation_rejects_bad_shapes() {
        assert!(base().validate().is_ok());

        let mut http_url = base();
        http_url.token_url = "http://api.vanta.com/oauth/token".into();
        assert!(http_url.validate().is_err());

        let mut query_url = base();
        query_url.token_url = "https://api.vanta.com/oauth/token?x=1".into();
        assert!(query_url.validate().is_err());

        let mut spacey_scope = base();
        spacey_scope.scopes = vec!["read write".into()];
        assert!(spacey_scope.validate().is_err());

        let mut bad_delimiter = base();
        bad_delimiter.scope_delimiter = ";".into();
        assert!(bad_delimiter.validate().is_err());

        let mut no_refs = base();
        no_refs.client_secret_ref = " ".into();
        assert!(no_refs.validate().is_err());

        let mut bad_field = base();
        bad_field
            .metadata
            .from_token_response
            .insert("proto".into(), "team.id".into());
        assert!(bad_field.validate().is_err());
    }
}
