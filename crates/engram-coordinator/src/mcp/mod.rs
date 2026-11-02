//! Dedicated coordinator credential authority. Never registered on app gRPC.
pub mod worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpError {
    Invalid,
    Denied,
    Unavailable,
    NotFound,
    Stale,
    Crypto,
    Transport,
}
pub type Result<T> = std::result::Result<T, McpError>;
impl From<engram_core::MetaError> for McpError {
    fn from(_: engram_core::MetaError) -> Self {
        Self::Stale
    }
}

pub mod control;
mod io;
mod lifecycle;
pub mod model;
pub mod oauth;
mod policy;
pub mod startup;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use engram_core::types::org_secret::{McpSecretReservation, McpSecretRole};
use engram_crypto::{CredCipher, SealedCred};
use model::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use worker::*;

pub trait SecurityEntropy: Send + Sync {
    fn bytes(&self) -> Result<[u8; 32]>;
}
pub struct SystemSecurityEntropy;
impl SecurityEntropy for SystemSecurityEntropy {
    fn bytes(&self) -> Result<[u8; 32]> {
        engram_crypto::security_random_bytes().map_err(|_| McpError::Crypto)
    }
}
pub struct McpAuthority {
    pub(crate) manager: Arc<crate::oauth::OAuthManager>,
    pub(crate) security: Arc<dyn SecurityEntropy>,
    pub(crate) registry: Registry,
    workers: BTreeMap<String, Arc<dyn WorkerTransport>>,
}
#[derive(Serialize, Deserialize)]
struct SecretPayload {
    v: u32,
    context: AuthorityContext,
    reservation: McpSecretReservation,
    header: String,
    value: String,
    revoked: bool,
}
impl McpAuthority {
    pub fn new(
        manager: Arc<crate::oauth::OAuthManager>,
        security: Arc<dyn SecurityEntropy>,
        registry: Registry,
        workers: BTreeMap<String, Arc<dyn WorkerTransport>>,
    ) -> Result<Self> {
        if registry.deployment_id.is_empty() || registry.deployment_id.len() > 256 {
            return Err(McpError::Invalid);
        }
        let this = Self {
            manager,
            security,
            registry,
            workers,
        };
        for (id, zone) in &this.registry.zones {
            if !this.workers.contains_key(id)
                || zone.policy_digest.len() != 64
                || zone.policy_revision.is_empty()
            {
                return Err(McpError::Invalid);
            }
            for endpoint in &zone.endpoints {
                Self::canonical_endpoint(endpoint)?;
            }
        }
        for (id, r) in &this.registry.registrations {
            if id != &r.id
                || r.client_id.is_empty()
                || r.client_id.len() > 8192
                || r.scopes
                    .iter()
                    .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_whitespace))
                || r.scopes.len() > 128
            {
                return Err(McpError::Invalid);
            }
            Self::canonical_endpoint(&r.issuer)?;
            Self::canonical_endpoint(&r.redirect_uri)?;
            for d in [
                &r.resource,
                &r.resource_metadata,
                &r.authorization_metadata,
                &r.authorization,
                &r.token,
            ] {
                this.destination(d)?;
            }
        }
        Ok(this)
    }
    pub(crate) fn enabled(&self) -> Result<()> {
        if !self.registry.enabled {
            Err(McpError::Unavailable)
        } else {
            Ok(())
        }
    }
    pub(crate) fn random(&self) -> Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(self.security.bytes()?))
    }
    pub(crate) fn canonical_endpoint(endpoint: &str) -> Result<()> {
        let url = url::Url::parse(endpoint).map_err(|_| McpError::Invalid)?;
        if url.scheme() != "https"
            || !matches!(url.host(), Some(url::Host::Domain(_)))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.as_str() != endpoint
        {
            return Err(McpError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn destination(&self, d: &Destination) -> Result<&Zone> {
        let z = self
            .registry
            .zones
            .get(&d.zone_id)
            .ok_or(McpError::Denied)?;
        if !z.endpoints.contains(&d.endpoint) {
            return Err(McpError::Denied);
        }
        Ok(z)
    }
    pub(crate) fn worker(&self, d: &Destination) -> Result<&Arc<dyn WorkerTransport>> {
        self.destination(d)?;
        self.workers.get(&d.zone_id).ok_or(McpError::Denied)
    }
    pub(crate) async fn identity(
        &self,
        d: &Destination,
        expected: Option<&WorkerIdentity>,
    ) -> Result<WorkerIdentity> {
        let z = self.destination(d)?;
        let actual = self.worker(d)?.identity().await?;
        if actual.protocol != 1
            || actual.zone_id != d.zone_id
            || actual.policy_digest != z.policy_digest
            || actual.policy_revision != z.policy_revision
            || expected.is_some_and(|v| v != &actual)
        {
            return Err(McpError::Stale);
        }
        Ok(actual)
    }
    pub(crate) fn context(&self, change: &ChangeContext) -> Result<AuthorityContext> {
        self.enabled()?;
        self.destination(&change.resource)?;
        if change.connection_id.is_empty()
            || change.connection_id.len() > 256
            || change.config_revision < 1
            || change.config_revision > 9_007_199_254_740_991
            || change.previous_authorization_generation < 0
        {
            return Err(McpError::Invalid);
        }
        let generation = change
            .previous_authorization_generation
            .checked_add(1)
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or(McpError::Invalid)?;
        let zone = self.destination(&change.resource)?;
        Ok(AuthorityContext {
            policy_digest: zone.policy_digest.clone(),
            policy_revision: zone.policy_revision.clone(),
            connection_id: change.connection_id.clone(),
            config_revision: change.config_revision,
            authorization_generation: generation,
            credential_identity: self.random()?,
            resource: change.resource.clone(),
        })
    }
    pub async fn put_secret(
        &self,
        change: &ChangeContext,
        role: McpSecretRole,
        expected: Option<&CredentialReference>,
        header: &str,
        value: &str,
    ) -> Result<CredentialStatus> {
        let context = self.context(change)?;
        if value.is_empty()
            || value.len() > 8192
            || value.contains(['\r', '\n'])
            || !["authorization", "x-api-key", "api-key"].contains(&header)
        {
            return Err(McpError::Invalid);
        }
        let reservation = match expected {
            None => {
                self.manager
                    .meta
                    .reserve_mcp_org_secret(
                        &format!("mcp-{}", self.random()?),
                        &context.connection_id,
                        role,
                    )
                    .await?
            }
            Some(
                CredentialReference::ApiKey { reservation }
                | CredentialReference::ClientSecret { reservation },
            ) => {
                if reservation.connection_id != context.connection_id || reservation.role != role {
                    return Err(McpError::Denied);
                }
                let previous = self.open_secret(reservation).await?;
                if previous.context.authorization_generation
                    != change.previous_authorization_generation
                {
                    return Err(McpError::Stale);
                }
                reservation.clone()
            }
            _ => return Err(McpError::Denied),
        };
        let mut next = reservation.clone();
        next.generation = next.generation.checked_add(1).ok_or(McpError::Invalid)?;
        let payload = SecretPayload {
            v: 1,
            context: context.clone(),
            reservation: next.clone(),
            header: header.into(),
            value: value.into(),
            revoked: false,
        };
        let bytes = serde_json::to_vec(&payload).map_err(|_| McpError::Invalid)?;
        let sealed = crate::org_secrets::seal_org_secret(
            self.manager.kek.as_ref(),
            &reservation.name,
            &bytes,
        )
        .await
        .map_err(|_| McpError::Crypto)?;
        self.manager
            .meta
            .put_mcp_org_secret(sealed, &reservation)
            .await?;
        let reference = match role {
            McpSecretRole::ApiKey => CredentialReference::ApiKey {
                reservation: next.clone(),
            },
            McpSecretRole::ClientSecret => CredentialReference::ClientSecret {
                reservation: next.clone(),
            },
        };
        Ok(CredentialStatus {
            context,
            reference,
            version: next.generation,
            status: "connected".into(),
        })
    }
    async fn open_secret(&self, expected: &McpSecretReservation) -> Result<SecretPayload> {
        let row = self
            .manager
            .meta
            .get_org_secret_sealed(&expected.name)
            .await?
            .ok_or(McpError::Unavailable)?;
        // Read the current permanent reservation after the captured ciphertext.
        if self
            .manager
            .meta
            .get_mcp_org_secret_reservation(&expected.name)
            .await?
            .as_ref()
            != Some(expected)
        {
            return Err(McpError::Stale);
        }
        let envelope = SealedCred {
            wrapped_dek: row.wrapped_dek,
            nonce: row.nonce.try_into().map_err(|_| McpError::Crypto)?,
            ciphertext: row.ciphertext,
            key_id: row.key_id,
        };
        let bytes = CredCipher::new(self.manager.kek.as_ref())
            .open(&envelope)
            .await
            .map_err(|_| McpError::Crypto)?;
        let payload: SecretPayload =
            serde_json::from_slice(&bytes).map_err(|_| McpError::Invalid)?;
        if payload.v != 1
            || &payload.reservation != expected
            || payload.context.connection_id != expected.connection_id
        {
            return Err(McpError::Denied);
        }
        Ok(payload)
    }
    pub async fn lease(&self, lease: &ResourceLease) -> Result<ProviderCredential> {
        self.enabled()?;
        let now = self.manager.clock.now_utc().timestamp_millis();
        if lease.expires_at <= now
            || lease.expires_at > now + 10000
            || lease.operation_expires_at < lease.expires_at
            || lease.operation_expires_at > now + 300000
            || lease.admission.expires_at < lease.operation_expires_at
            || lease.admission.expires_at > now + 300000
            || lease.admission.deployment_id != self.registry.deployment_id
            || lease.admission.id.is_empty()
            || lease.intent_id.is_empty()
            || lease.request_digest.len() != 64
            || !lease.request_digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(McpError::Denied);
        }
        if !matches!(
            (&*lease.operation, &lease.scope),
            ("discover", WorkerScope::Administration { .. })
                | ("execute", WorkerScope::Execution { .. })
        ) {
            return Err(McpError::Denied);
        }
        if lease.context.policy_digest != lease.worker.policy_digest
            || lease.context.policy_revision != lease.worker.policy_revision
        {
            return Err(McpError::Stale);
        }
        self.identity(&lease.context.resource, Some(&lease.worker))
            .await?;
        let credential = match &lease.reference {
            CredentialReference::ApiKey { reservation } => {
                if reservation.role != McpSecretRole::ApiKey {
                    return Err(McpError::Denied);
                }
                let p = self.open_secret(reservation).await?;
                if p.context != lease.context || p.revoked {
                    return Err(McpError::Stale);
                }
                ProviderCredential {
                    header: p.header,
                    value: p.value,
                }
            }
            CredentialReference::OAuth { registration_id } => {
                self.oauth_lease(&lease.context, registration_id).await?
            }
            _ => return Err(McpError::Denied),
        };
        if self.manager.clock.now_utc().timestamp_millis() >= lease.expires_at {
            return Err(McpError::Stale);
        }
        Ok(credential)
    }
}
