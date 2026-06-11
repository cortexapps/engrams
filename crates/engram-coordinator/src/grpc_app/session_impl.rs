//! Non-RPC async helpers for SessionService. Lives in a separate file so the
//! convention test's SOURCES scan (which counts `async fn` against
//! `self.auth.check(&req)?;`) does not count these internal helpers.

/// Unseal the orchestrator secret at `secret_id` and, when the image's
/// manifest declares a builtin `claude` harness and the session runs in
/// agent mode, inject the plaintext as `CLAUDE_CODE_OAUTH_TOKEN`. Returns
/// `NotFound` when no secret row exists — the orchestrator must `PutSecret`
/// before `CreateSession(harness_secret_id=...)`.
///
/// NEVER log the return value — keep the handler's redaction discipline.
pub(super) async fn build_harness_secret_env(
    state: &crate::state::SharedState,
    secret_id: &str,
    req: &crate::api::sessions::CreateSessionRequest,
) -> Result<std::collections::HashMap<String, String>, tonic::Status> {
    let row = state
        .services
        .meta
        .get_sealed_secret(secret_id)
        .await
        .map_err(|e| super::into_status(crate::error::ApiError::from(e)))?
        .ok_or_else(|| {
            tonic::Status::not_found(format!(
                "secret {secret_id:?} not found — call PutSecret before CreateSession"
            ))
        })?;
    let nonce: [u8; 12] = row
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| tonic::Status::internal("sealed_secret.nonce wrong length"))?;
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: row.wrapped_dek,
        nonce,
        ciphertext: row.ciphertext,
        key_id: row.key_id,
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plaintext_bytes = cipher
        .open(&sealed)
        .await
        .map_err(|e| tonic::Status::internal(format!("unseal secret: {e}")))?;
    let plaintext = String::from_utf8(plaintext_bytes)
        .map_err(|_| tonic::Status::internal("secret value is not valid UTF-8"))?;

    // Only inject when the image carries a builtin claude harness and the
    // session runs in agent mode — mirrors inject_user_claude_token.
    let image_uri: &str = &req.image;
    let is_builtin_claude =
        if let Ok(Some(enabled)) = state.services.meta.get_enabled_image(image_uri).await {
            if let Ok(manifest) =
                toml::from_str::<engram_core::types::ImageManifest>(&enabled.manifest_toml)
            {
                manifest.harness.as_ref().and_then(|h| h.name.as_deref()) == Some("claude")
                    && !req.mode.is_dev_vm()
            } else {
                false
            }
        } else {
            false
        };

    let mut env = std::collections::HashMap::new();
    if is_builtin_claude {
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), plaintext);
    }
    // Drop plaintext — it's either in env or discarded.
    Ok(env)
}
