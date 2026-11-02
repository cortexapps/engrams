//! Reference-only status and expected-version revocation.
use super::*;
use engram_core::types::oauth::{OAuthCredentialKey, OAuthSubjectKind};
impl McpAuthority {
    pub async fn status(
        &self,
        connection: &str,
        reference: &CredentialReference,
    ) -> Result<CredentialStatus> {
        self.enabled()?;
        match reference {
            CredentialReference::ApiKey { reservation }
            | CredentialReference::ClientSecret { reservation } => {
                let current = self
                    .manager
                    .meta
                    .get_mcp_org_secret_reservation(&reservation.name)
                    .await?
                    .ok_or(McpError::Unavailable)?;
                if current.connection_id != connection || current.role != reservation.role {
                    return Err(McpError::Denied);
                }
                let p = self.open_secret(&current).await?;
                let reference = if current.role == McpSecretRole::ApiKey {
                    CredentialReference::ApiKey {
                        reservation: current.clone(),
                    }
                } else {
                    CredentialReference::ClientSecret {
                        reservation: current.clone(),
                    }
                };
                Ok(CredentialStatus {
                    context: p.context,
                    reference,
                    version: current.generation,
                    status: if p.revoked { "revoked" } else { "connected" }.into(),
                })
            }
            CredentialReference::OAuth { registration_id } => {
                let row = self
                    .manager
                    .meta
                    .get_oauth_credential(&OAuthCredentialKey {
                        subject_kind: OAuthSubjectKind::Mcp,
                        subject_id: connection.into(),
                        provider: registration_id.clone(),
                    })
                    .await?
                    .ok_or(McpError::Unavailable)?;
                let bundle = self.decode_bundle(&row).await?;
                Ok(CredentialStatus {
                    context: bundle.context,
                    reference: reference.clone(),
                    version: row.version,
                    status: row.status(self.manager.clock.now_utc()).as_str().into(),
                })
            }
        }
    }
    pub async fn revoke(&self, expected: &CredentialStatus) -> Result<CredentialStatus> {
        self.enabled()?;
        if &self
            .status(&expected.context.connection_id, &expected.reference)
            .await?
            != expected
        {
            return Err(McpError::Stale);
        }
        match &expected.reference {
            CredentialReference::ApiKey { reservation }
            | CredentialReference::ClientSecret { reservation } => {
                let mut p = self.open_secret(reservation).await?;
                p.revoked = true;
                p.value.clear();
                p.reservation.generation = p
                    .reservation
                    .generation
                    .checked_add(1)
                    .ok_or(McpError::Invalid)?;
                p.context.authorization_generation = p
                    .context
                    .authorization_generation
                    .checked_add(1)
                    .ok_or(McpError::Invalid)?;
                p.context.credential_identity = self.random()?;
                let sealed = crate::org_secrets::seal_org_secret(
                    self.manager.kek.as_ref(),
                    &reservation.name,
                    &serde_json::to_vec(&p).map_err(|_| McpError::Invalid)?,
                )
                .await
                .map_err(|_| McpError::Crypto)?;
                self.manager
                    .meta
                    .put_mcp_org_secret(sealed, reservation)
                    .await?;
            }
            CredentialReference::OAuth { registration_id } => {
                self.manager
                    .meta
                    .revoke_oauth_credential(
                        &OAuthCredentialKey {
                            subject_kind: OAuthSubjectKind::Mcp,
                            subject_id: expected.context.connection_id.clone(),
                            provider: registration_id.clone(),
                        },
                        expected.version,
                    )
                    .await?;
            }
        }
        self.status(&expected.context.connection_id, &expected.reference)
            .await
    }
}
