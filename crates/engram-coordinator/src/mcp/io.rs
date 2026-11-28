//! Bounded worker I/O with durable deployment admission and refresh dispatch fencing.
use super::*;
use chrono::Duration;
use engram_core::types::mcp::{McpOAuthAdmission, McpRefreshClaim};
use serde_json::{json, Value};

impl McpAuthority {
    pub(super) async fn oauth_io(
        &self,
        context: &AuthorityContext,
        destination: &Destination,
        actor: &str,
        mut operation: WorkerOperation,
        secret: Option<&McpSecretReservation>,
        claim: Option<&McpRefreshClaim>,
    ) -> Result<Value> {
        self.enabled()?;
        if actor.is_empty() || actor.len() > 256 {
            return Err(McpError::Invalid);
        }
        let now = self.manager.clock.now_utc();
        let deadline = claim
            .map(|c| c.expires_at - Duration::seconds(5))
            .unwrap_or(now + Duration::seconds(10))
            .min(now + Duration::seconds(10));
        if deadline <= now {
            return Err(McpError::Stale);
        }
        let admission = McpOAuthAdmission {
            id: self.manager.entropy.uuid(),
            deployment_id: self.registry.deployment_id.clone(),
            connection_id: context.connection_id.clone(),
            expires_at: deadline + Duration::seconds(5),
        };
        if !self
            .manager
            .meta
            .acquire_mcp_oauth_admission(&admission)
            .await?
        {
            return Err(McpError::Unavailable);
        }
        let result = async {
            let identity = self.identity(destination, None).await?;
            if let Some(secret) = secret {
                let payload = self.open_secret(secret).await?;
                if payload.revoked
                    || secret.role != McpSecretRole::ClientSecret
                    || payload.context.connection_id != context.connection_id
                    || payload.context.config_revision != context.config_revision
                {
                    return Err(McpError::Stale);
                }
                match &mut operation {
                    WorkerOperation::Token { grant, .. } => match grant {
                        TokenGrant::AuthorizationCode { client_secret, .. }
                        | TokenGrant::RefreshToken { client_secret, .. } => {
                            *client_secret = Some(payload.value);
                        }
                    },
                    _ => return Err(McpError::Denied),
                }
            }
            let intent = WorkerIntent {
                id: self.manager.entropy.uuid().to_string(),
                boot_id: identity.boot_id.clone(),
                zone_id: identity.zone_id.clone(),
                policy_digest: identity.policy_digest.clone(),
                connection_id: context.connection_id.clone(),
                connection_revision: context.config_revision,
                credential_identity: context.credential_identity.clone(),
                scope: WorkerScope::Administration {
                    request_id: admission.id.to_string(),
                    actor_id: actor.into(),
                },
                operation: operation.kind().into(),
                destination: destination.endpoint.clone(),
                request_digest: digest(&json!({
                    "context": context,
                    "admission": admission.id.to_string(),
                    "operation": operation.kind(),
                    "destination": destination,
                }))?,
                operation_digest: digest(
                    &serde_json::to_value(&operation).map_err(|_| McpError::Invalid)?,
                )?,
                expires_at: deadline.timestamp_millis(),
                lease: WorkerLease {
                    id: admission.id.to_string(),
                    deployment_id: admission.deployment_id.clone(),
                    expires_at: admission.expires_at.timestamp_millis(),
                },
            };
            // Admission, worker identity, and client-secret checks precede the
            // irreversible rotating-token dispatch claim.
            if self.manager.clock.now_utc() >= deadline {
                return Err(McpError::Stale);
            }
            if let Some(claim) = claim {
                if !self.manager.meta.claim_mcp_refresh(claim).await? {
                    return Err(McpError::Unavailable);
                }
            }
            // A slow claim transaction must not extend the token RPC deadline.
            let remaining = deadline - self.manager.clock.now_utc();
            if remaining <= Duration::zero() {
                return Err(McpError::Stale);
            }
            tokio::time::timeout(
                remaining.to_std().map_err(|_| McpError::Stale)?,
                self.worker(destination)?
                    .rpc(&identity, &intent, &operation),
            )
            .await
            .map_err(|_| McpError::Transport)?
        }
        .await;
        // Release only after the bounded RPC has ended. Expiry covers a crashed caller.
        self.manager
            .meta
            .release_mcp_oauth_admission(&admission)
            .await?;
        result
    }
}
