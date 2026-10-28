//! Service-account JWT-bearer grants (RFC 7523) for connector subjects.

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

/// Service-account grant settings supplied by the connector.
#[derive(Clone, Debug)]
pub struct ServiceAccountSpec {
    pub token_url: String,
    pub scopes: Vec<String>,
    /// Empty means space, the RFC delimiter.
    pub scope_delimiter: String,
    pub key_ref: String,
    pub subject: Option<String>,
    pub metadata: RedirectMetadataSpec,
}

impl ServiceAccountSpec {
    pub fn delimiter(&self) -> &str {
        if self.scope_delimiter.is_empty() {
            " "
        } else {
            &self.scope_delimiter
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_https_url(&self.token_url, "token_url")?;
        if self.scopes.is_empty() {
            return Err("at least one scope is required".into());
        }
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
        if self.key_ref.trim().is_empty() {
            return Err("a key ref is required".into());
        }
        self.metadata.validate()
    }
}

impl OAuthManager {
    /// Run the grant and seal the obtained token for `key`. A later call
    /// replaces the credential (a reconnect with new secrets); the sealed
    /// refresh spec carries the scope and encoding so the sweep can run the
    /// same grant without the connector catalog.
    pub async fn grant_service_account(
        &self,
        key: &OAuthCredentialKey,
        spec: &ServiceAccountSpec,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        validate_key(key)?;
        if key.subject_kind != OAuthSubjectKind::Connector {
            return Err(OAuthServiceError::BadRequest(
                "service-account grants are restricted to connector subjects".into(),
            ));
        }
        spec.validate().map_err(OAuthServiceError::BadRequest)?;
        let principal = match spec.subject.as_ref().filter(|s| !s.trim().is_empty()) {
            Some(principal) => Some(principal.clone()),
            None => match self.meta.get_oauth_credential(key).await? {
                Some(row) => {
                    let payload = self.open(&row).await?;
                    let bundle = ConnectorOAuthBundle::from_json(&payload)
                        .map_err(|_| OAuthServiceError::InvalidBundle)?;
                    bundle.refresh.and_then(|refresh| refresh.subject)
                }
                None => None,
            },
        }
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| OAuthServiceError::BadRequest("a principal is required".into()))?;
        let scope = spec.scopes.join(spec.delimiter());
        let assertion = self
            .service_account_assertion(&spec.token_url, &spec.key_ref, &principal, &scope)
            .await?;
        let form = [
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            ),
            ("assertion", assertion),
        ];
        let grant = self
            .token_grant(
                &spec.token_url,
                &form,
                spec.delimiter(),
                "",
                TokenRequestBody::Form,
            )
            .await?;

        let mut fields = BTreeMap::new();
        extract_mapped(&spec.metadata.from_token_response, &grant.json, &mut fields);
        if let Some(probe) = &spec.metadata.probe {
            let json = self.run_probe(probe, &grant.access_token).await?;
            extract_mapped(&probe.map, &json, &mut fields);
        }
        let metadata = OAuthAccountMetadata {
            // Use the connector id when the provider maps no account id.
            account_id: fields
                .get(META_ACCOUNT_ID)
                .cloned()
                .unwrap_or_else(|| key.subject_id.clone()),
            display_name: Some(
                fields
                    .get(META_DISPLAY_NAME)
                    .cloned()
                    .unwrap_or_else(|| principal.clone()),
            ),
            plan_type: None,
            workspace_id: fields.get(META_WORKSPACE_ID).cloned(),
            workspace_name: fields.get(META_WORKSPACE_NAME).cloned(),
        };

        let bundle = ConnectorOAuthBundle {
            v: CONNECTOR_OAUTH_BUNDLE_VERSION,
            kind: ConnectorOAuthKind::Oauth2JwtBearer,
            access_token: grant.access_token,
            token_type: grant.token_type,
            refresh_token: None,
            scope: grant.scope.or_else(|| Some(scope.clone())),
            obtained_at: self.clock.now_utc(),
            expires_at: grant.expires_at,
            refresh: Some(ConnectorOAuthRefreshSpec {
                token_url: spec.token_url.clone(),
                client_id_ref: String::new(),
                client_secret_ref: String::new(),
                assertion_key_ref: Some(spec.key_ref.clone()),
                subject: Some(principal),
                scope: Some(scope),
                body: TokenRequestBody::Form,
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

#[derive(serde::Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(serde::Serialize)]
struct AssertionClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    sub: &'a str,
    iat: i64,
    exp: i64,
}

impl OAuthManager {
    /// Sign an assertion from the current org secret. Both grant paths check
    /// the key's token URI before sending the assertion to the provider.
    pub(crate) async fn service_account_assertion(
        &self,
        token_url: &str,
        key_ref: &str,
        principal: &str,
        scope: &str,
    ) -> Result<String, OAuthServiceError> {
        use jsonwebtoken::{Algorithm, EncodingKey, Header};

        let secret = self.resolve_org_secret(key_ref).await?;
        let key: ServiceAccountKey = serde_json::from_str(&secret).map_err(|_| {
            OAuthServiceError::BadRequest("invalid service-account JSON key".into())
        })?;
        if key.token_uri != token_url {
            return Err(OAuthServiceError::BadRequest(
                "service-account token_uri must match token_url".into(),
            ));
        }
        if key.client_email.trim().is_empty() || principal.trim().is_empty() || scope.is_empty() {
            return Err(OAuthServiceError::BadRequest(
                "service-account email, principal, and scope are required".into(),
            ));
        }
        let encoding_key = EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .map_err(|_| OAuthServiceError::BadRequest("invalid service-account RSA key".into()))?;
        let iat = self.clock.now_utc().timestamp();
        jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &AssertionClaims {
                iss: &key.client_email,
                scope,
                aud: token_url,
                sub: principal,
                iat,
                exp: iat + 3600,
            },
            &encoding_key,
        )
        .map_err(|_| {
            OAuthServiceError::BadRequest("could not sign service-account assertion".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_specs_are_rejected() {
        let base = ServiceAccountSpec {
            token_url: "https://example.com/token".into(),
            scopes: vec!["directory.read".into()],
            scope_delimiter: String::new(),
            key_ref: "directory.key".into(),
            subject: None,
            metadata: RedirectMetadataSpec::default(),
        };
        assert!(base.validate().is_ok());
        for scopes in [
            vec![],
            vec![String::new()],
            vec!["read write".into()],
            vec!["read".into(); MAX_SCOPES + 1],
        ] {
            assert!(ServiceAccountSpec {
                scopes,
                ..base.clone()
            }
            .validate()
            .is_err());
        }
        for token_url in [
            "http://example.com/token",
            "https://example.com/token?query=1",
            "https://example.com/token#fragment",
        ] {
            assert!(ServiceAccountSpec {
                token_url: token_url.into(),
                ..base.clone()
            }
            .validate()
            .is_err());
        }
        assert!(ServiceAccountSpec {
            key_ref: " ".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ServiceAccountSpec {
            scope_delimiter: ";".into(),
            ..base
        }
        .validate()
        .is_err());
    }
}
