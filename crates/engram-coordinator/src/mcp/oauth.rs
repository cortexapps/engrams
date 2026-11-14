//! Durable encrypted PKCE and strict, exclusive refresh for registered MCP clients.
use super::*;
use chrono::{DateTime, Duration, Utc};
use engram_core::types::{
    mcp::{McpRefreshClaim, McpSealedState},
    oauth::*,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BegunAuthorization {
    pub flow_id: Uuid,
    pub context: AuthorityContext,
    pub authorization_url: String,
    pub expires_at: DateTime<Utc>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Callback {
    pub context: AuthorityContext,
    pub registration_id: String,
    pub flow_id: Uuid,
    pub state: String,
    pub redirect_uri: String,
    pub issuer: Option<String>,
    pub code: String,
    pub actor_id: String,
}
#[derive(Serialize, Deserialize)]
struct FlowState {
    require_issuer: bool,
    v: u32,
    id: Uuid,
    state: String,
    verifier: String,
    context: AuthorityContext,
    registration: ClientRegistration,
    policies: BTreeMap<String, (String, String)>,
    client_secret: Option<McpSecretReservation>,
    expected_version: Option<i64>,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Bundle {
    v: u32,
    pub context: AuthorityContext,
    registration: ClientRegistration,
    policies: BTreeMap<String, (String, String)>,
    client_secret: Option<McpSecretReservation>,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    provider_account: Option<String>,
    identity_source: String,
}
fn key(connection: &str, registration: &str) -> OAuthCredentialKey {
    OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Mcp,
        subject_id: connection.into(),
        provider: registration.into(),
    }
}
impl McpAuthority {
    fn policies(&self, r: &ClientRegistration) -> Result<BTreeMap<String, (String, String)>> {
        [
            &r.resource,
            &r.resource_metadata,
            &r.authorization_metadata,
            &r.authorization,
            &r.token,
        ]
        .into_iter()
        .map(|d| {
            let z = self.destination(d)?;
            Ok((
                d.zone_id.clone(),
                (z.policy_digest.clone(), z.policy_revision.clone()),
            ))
        })
        .collect()
    }
    fn registration(&self, id: &str) -> Result<&ClientRegistration> {
        self.registry.registrations.get(id).ok_or(McpError::Denied)
    }
    fn check_registration(
        &self,
        r: &ClientRegistration,
        policies: &BTreeMap<String, (String, String)>,
    ) -> Result<()> {
        if self.registration(&r.id)? != r || &self.policies(r)? != policies {
            return Err(McpError::Stale);
        }
        Ok(())
    }
    async fn seal_state<T: Serialize>(&self, value: &T) -> Result<McpSealedState> {
        let bytes = serde_json::to_vec(value).map_err(|_| McpError::Invalid)?;
        let s = CredCipher::new(self.manager.kek.as_ref())
            .seal(&bytes)
            .await
            .map_err(|_| McpError::Crypto)?;
        Ok(McpSealedState {
            wrapped_dek: s.wrapped_dek,
            nonce: s.nonce.to_vec(),
            ciphertext: s.ciphertext,
            key_id: s.key_id,
        })
    }
    async fn open_state<T: serde::de::DeserializeOwned>(&self, s: McpSealedState) -> Result<T> {
        let bytes = CredCipher::new(self.manager.kek.as_ref())
            .open(&SealedCred {
                wrapped_dek: s.wrapped_dek,
                nonce: s.nonce.try_into().map_err(|_| McpError::Crypto)?,
                ciphertext: s.ciphertext,
                key_id: s.key_id,
            })
            .await
            .map_err(|_| McpError::Crypto)?;
        serde_json::from_slice(&bytes).map_err(|_| McpError::Invalid)
    }
    pub async fn begin(
        &self,
        change: &ChangeContext,
        registration_id: &str,
        secret: Option<&CredentialReference>,
        expected_version: Option<i64>,
        actor: &str,
        sharing_consent: bool,
    ) -> Result<BegunAuthorization> {
        let context = self.context(change)?;
        if !sharing_consent {
            return Err(McpError::Denied);
        }
        let r = self.registration(registration_id)?.clone();
        if r.resource != context.resource {
            return Err(McpError::Denied);
        }
        let client_secret = match (r.confidential, secret) {
            (false, None) => None,
            (true, Some(CredentialReference::ClientSecret { reservation }))
                if reservation.connection_id == context.connection_id
                    && reservation.role == McpSecretRole::ClientSecret =>
            {
                let payload = self.open_secret(reservation).await?;
                if payload.revoked {
                    return Err(McpError::Denied);
                }
                Some(reservation.clone())
            }
            _ => return Err(McpError::Denied),
        };
        let current = self
            .manager
            .meta
            .get_oauth_credential(&key(&context.connection_id, registration_id))
            .await?;
        if current.as_ref().map(|r| r.version) != expected_version {
            return Err(McpError::Stale);
        }
        let resource = self
            .oauth_io(
                &context,
                &r.resource_metadata,
                actor,
                WorkerOperation::Metadata {
                    endpoint: r.resource_metadata.endpoint.clone(),
                },
                None,
                None,
            )
            .await?;
        if resource["resource"] != r.resource.endpoint
            || !resource["authorization_servers"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == &r.issuer))
        {
            return Err(McpError::Denied);
        }
        let metadata = self
            .oauth_io(
                &context,
                &r.authorization_metadata,
                actor,
                WorkerOperation::Metadata {
                    endpoint: r.authorization_metadata.endpoint.clone(),
                },
                None,
                None,
            )
            .await?;
        if metadata["issuer"] != r.issuer
            || metadata["authorization_endpoint"] != r.authorization.endpoint
            || metadata["token_endpoint"] != r.token.endpoint
            || !metadata["code_challenge_methods_supported"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "S256"))
        {
            return Err(McpError::Denied);
        }
        self.oauth_io(
            &context,
            &r.authorization,
            actor,
            WorkerOperation::ValidateDestination {
                endpoint: r.authorization.endpoint.clone(),
            },
            None,
            None,
        )
        .await?;
        let now = self.manager.clock.now_utc();
        let expires_at = now + Duration::minutes(10);
        let random = self.security.bytes()?;
        let id = Uuid::from_bytes(random[..16].try_into().map_err(|_| McpError::Crypto)?);
        let state = self.random()?;
        let verifier = self.random()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let flow = FlowState {
            require_issuer: r.issuer_parameter_required
                || metadata["authorization_response_iss_parameter_supported"] == true,
            v: 1,
            id,
            state: state.clone(),
            verifier,
            context: context.clone(),
            policies: self.policies(&r)?,
            registration: r.clone(),
            client_secret,
            expected_version,
        };
        self.manager
            .meta
            .create_mcp_oauth_flow(
                OAuthFlow {
                    id,
                    key: key(&context.connection_id, registration_id),
                    owner_replica: self.manager.replica.clone(),
                    lease_expires_at: expires_at,
                    expires_at,
                    status: OAuthFlowStatus::Pending,
                    error_code: None,
                    created_at: now,
                    updated_at: now,
                },
                self.seal_state(&flow).await?,
            )
            .await?;
        let mut url = url::Url::parse(&r.authorization.endpoint).map_err(|_| McpError::Invalid)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &r.client_id)
            .append_pair("redirect_uri", &r.redirect_uri)
            .append_pair("resource", &r.resource.endpoint)
            .append_pair(
                "scope",
                &r.scopes.iter().cloned().collect::<Vec<_>>().join(" "),
            )
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(BegunAuthorization {
            flow_id: id,
            context,
            authorization_url: url.into(),
            expires_at,
        })
    }
    pub async fn cancel_authorization(
        &self,
        flow_id: Uuid,
        connection_id: &str,
        registration_id: &str,
    ) -> Result<()> {
        self.enabled()?;
        self.manager
            .meta
            .cancel_mcp_oauth_flow(flow_id, &key(connection_id, registration_id))
            .await?;
        Ok(())
    }
    pub async fn callback(&self, callback: &Callback) -> Result<CredentialStatus> {
        self.enabled()?;
        // The winner removes ciphertext atomically BEFORE any token request.
        let flow: FlowState = self
            .open_state(
                self.manager
                    .meta
                    .consume_mcp_oauth_flow(
                        callback.flow_id,
                        &key(&callback.context.connection_id, &callback.registration_id),
                    )
                    .await?,
            )
            .await?;
        let result = async {
            let r = &flow.registration;
            if flow.v != 1
                || flow.id != callback.flow_id
                || flow.context != callback.context
                || flow.state != callback.state
                || r.id != callback.registration_id
                || r.redirect_uri != callback.redirect_uri
                || callback.issuer.as_ref().is_some_and(|i| i != &r.issuer)
                || (flow.require_issuer && callback.issuer.is_none())
                || callback.code.is_empty()
                || callback.code.len() > 8192
            {
                return Err(McpError::Denied);
            }
            self.check_registration(r, &flow.policies)?;
            let response = self
                .oauth_io(
                    &flow.context,
                    &r.token,
                    &callback.actor_id,
                    WorkerOperation::Token {
                        endpoint: r.token.endpoint.clone(),
                        grant: TokenGrant::AuthorizationCode {
                            code: callback.code.clone(),
                            redirect_uri: r.redirect_uri.clone(),
                            code_verifier: flow.verifier,
                            client_id: r.client_id.clone(),
                            resource: r.resource.endpoint.clone(),
                            client_secret: None,
                        },
                    },
                    flow.client_secret.as_ref(),
                    None,
                )
                .await?;
            let grant = validate_token(&response, r, None, self.manager.clock.now_utc())?;
            let bundle = Bundle {
                v: 1,
                context: flow.context,
                registration: flow.registration,
                policies: flow.policies,
                client_secret: flow.client_secret,
                access_token: grant.access_token,
                refresh_token: grant.refresh_token,
                expires_at: grant.expires_at,
                identity_source: if grant.account.is_some() {
                    "provider_response"
                } else {
                    "authorization_ceremony"
                }
                .into(),
                provider_account: grant.account,
            };
            let sealed = self.credential(&bundle).await?;
            let row = self
                .manager
                .meta
                .complete_mcp_oauth_flow(callback.flow_id, sealed, flow.expected_version)
                .await?;
            Ok(status(&bundle, &row))
        }
        .await;
        // Completion metadata cannot make a consumed flow replayable.
        let _ = self
            .manager
            .meta
            .finish_oauth_flow_unowned(
                callback.flow_id,
                if result.is_ok() {
                    OAuthFlowStatus::Succeeded
                } else {
                    OAuthFlowStatus::Failed
                },
                if result.is_err() {
                    Some("authorization_failed")
                } else {
                    None
                },
            )
            .await;
        result
    }
    async fn credential(&self, bundle: &Bundle) -> Result<NewSealedOAuthCredential> {
        let sealed = self.seal_state(bundle).await?;
        Ok(NewSealedOAuthCredential {
            key: key(&bundle.context.connection_id, &bundle.registration.id),
            wrapped_dek: sealed.wrapped_dek,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
            key_id: sealed.key_id,
            metadata: OAuthAccountMetadata {
                account_id: bundle.context.credential_identity.clone(),
                ..Default::default()
            },
            expires_at: bundle.expires_at,
        })
    }
    pub(crate) async fn decode_bundle(&self, row: &SealedOAuthCredential) -> Result<Bundle> {
        let bytes = self.manager.open(row).await.map_err(|_| McpError::Crypto)?;
        let bundle: Bundle = serde_json::from_slice(&bytes).map_err(|_| McpError::Invalid)?;
        if bundle.v != 1
            || row.key != key(&bundle.context.connection_id, &bundle.registration.id)
            || row.metadata.account_id != bundle.context.credential_identity
        {
            return Err(McpError::Denied);
        }
        Ok(bundle)
    }
    pub(crate) async fn open_bundle(&self, row: &SealedOAuthCredential) -> Result<Bundle> {
        let bundle = self.decode_bundle(row).await?;
        self.check_registration(&bundle.registration, &bundle.policies)?;
        if let Some(s) = &bundle.client_secret {
            if self
                .manager
                .meta
                .get_mcp_org_secret_reservation(&s.name)
                .await?
                .as_ref()
                != Some(s)
            {
                return Err(McpError::Stale);
            }
        }
        Ok(bundle)
    }
    pub(crate) async fn oauth_lease(
        &self,
        expected: &AuthorityContext,
        registration_id: &str,
    ) -> Result<ProviderCredential> {
        let key = key(&expected.connection_id, registration_id);
        let mut row = self
            .manager
            .meta
            .get_oauth_credential(&key)
            .await?
            .ok_or(McpError::Unavailable)?;
        // Bounded follower wait. Never reacquire or retry the rotating exchange.
        for _ in 0..20 {
            if row.broken_reason.as_deref() != Some("refresh_pending") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            row = self
                .manager
                .meta
                .get_oauth_credential(&key)
                .await?
                .ok_or(McpError::Unavailable)?;
        }
        if row.revoked_at.is_some() || row.broken_at.is_some() {
            return Err(McpError::Unavailable);
        }
        let mut bundle = self.open_bundle(&row).await?;
        if &bundle.context != expected {
            return Err(McpError::Stale);
        }
        if bundle
            .expires_at
            .is_some_and(|t| t <= self.manager.clock.now_utc())
        {
            let claim = McpRefreshClaim {
                id: self.manager.entropy.uuid(),
                key: key.clone(),
                version: row.version,
                expires_at: self.manager.clock.now_utc() + Duration::seconds(25),
            };
            let refresh_token = bundle.refresh_token.take().ok_or(McpError::Unavailable)?;
            let response = self
                .oauth_io(
                    &bundle.context,
                    &bundle.registration.token,
                    "credential-refresh",
                    WorkerOperation::Token {
                        endpoint: bundle.registration.token.endpoint.clone(),
                        grant: TokenGrant::RefreshToken {
                            refresh_token: refresh_token.clone(),
                            client_id: bundle.registration.client_id.clone(),
                            resource: bundle.registration.resource.endpoint.clone(),
                            client_secret: None,
                        },
                    },
                    bundle.client_secret.as_ref(),
                    Some(&claim),
                )
                .await?;
            let grant = validate_token(
                &response,
                &bundle.registration,
                bundle.provider_account.as_deref(),
                self.manager.clock.now_utc(),
            )?;
            bundle.access_token = grant.access_token;
            bundle.refresh_token = grant.refresh_token.or(Some(refresh_token));
            bundle.expires_at = grant.expires_at;
            if let Some(account) = grant.account {
                bundle.provider_account = Some(account);
                bundle.identity_source = "provider_response".into();
            }
            self.manager
                .meta
                .complete_mcp_refresh(self.credential(&bundle).await?, &claim)
                .await?;
        }
        if bundle
            .expires_at
            .is_some_and(|t| t <= self.manager.clock.now_utc())
        {
            return Err(McpError::Unavailable);
        }
        Ok(ProviderCredential {
            header: "authorization".into(),
            value: format!("Bearer {}", bundle.access_token),
        })
    }
}
fn status(bundle: &Bundle, row: &SealedOAuthCredential) -> CredentialStatus {
    CredentialStatus {
        context: bundle.context.clone(),
        reference: CredentialReference::OAuth {
            registration_id: bundle.registration.id.clone(),
        },
        version: row.version,
        status: if row.revoked_at.is_some() {
            "revoked"
        } else if row.broken_at.is_some() {
            "broken"
        } else {
            "connected"
        }
        .into(),
    }
}
struct ValidatedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    account: Option<String>,
}
fn token_text(v: &Value) -> Result<String> {
    let s = v.as_str().ok_or(McpError::Invalid)?;
    if s.is_empty() || s.len() > 8192 || s.contains(['\r', '\n']) {
        return Err(McpError::Invalid);
    }
    Ok(s.into())
}
fn validate_token(
    value: &Value,
    r: &ClientRegistration,
    account: Option<&str>,
    now: DateTime<Utc>,
) -> Result<ValidatedToken> {
    if !value.is_object()
        || value.get("error").is_some()
        || !value["token_type"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
    {
        return Err(McpError::Denied);
    }
    for (field, expected) in [
        ("iss", &r.issuer),
        ("issuer", &r.issuer),
        ("resource", &r.resource.endpoint),
        ("audience", &r.resource.endpoint),
    ] {
        if value.get(field).is_some_and(|v| v != expected) {
            return Err(McpError::Denied);
        }
    }
    if let Some(scope) = value.get("scope") {
        let s = scope.as_str().ok_or(McpError::Invalid)?;
        let scopes: std::collections::BTreeSet<_> =
            s.split_whitespace().map(str::to_owned).collect();
        if scopes != r.scopes {
            return Err(McpError::Denied);
        }
    }
    let provider_account = r
        .account_field
        .as_ref()
        .and_then(|f| value.get(f))
        .map(token_text)
        .transpose()?;
    if provider_account
        .as_ref()
        .is_some_and(|p| p.len() > 512 || account.is_some_and(|a| a != p))
    {
        return Err(McpError::Denied);
    }
    let expires_at = value
        .get("expires_in")
        .map(|v| {
            let n = v
                .as_i64()
                .filter(|n| *n > 0 && *n <= 31_536_000)
                .ok_or(McpError::Invalid)?;
            Ok::<_, McpError>(now + Duration::seconds(n))
        })
        .transpose()?;
    Ok(ValidatedToken {
        access_token: token_text(&value["access_token"])?,
        refresh_token: value.get("refresh_token").map(token_text).transpose()?,
        expires_at,
        account: provider_account,
    })
}
