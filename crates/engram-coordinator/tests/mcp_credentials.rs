use engram_coordinator::org_secrets::{seal_org_secret, OrgSecretBackend};
use engram_core::traits::{
    LayeredSecretStore, MetadataStore, SecretContext, SecretStore, StaticSecretStore,
};
use engram_core::types::org_secret::McpSecretRole;
use engram_core::types::SecretSchema;
use engram_core::SecretError;
use engram_crypto::EnvVarKeyProvider;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn reserved_refs_deny_aliases_fallbacks_and_overrides_before_value_lookup() {
    let clock = ManualClock::new();
    let meta = SimMetadataStore::new(clock, Arc::new(SimEntropy::seeded(4)));
    let kek = Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test"));
    let r = meta
        .reserve_mcp_org_secret("reserved", "connection-a", McpSecretRole::ApiKey)
        .await
        .unwrap();
    let store = LayeredSecretStore::new(vec![
        Arc::new(OrgSecretBackend::new(meta.clone(), kek.clone())),
        Arc::new(StaticSecretStore::new(HashMap::from([(
            "reserved".into(),
            "fallback-value".into(),
        )]))),
    ]);
    let ctx = SecretContext {
        repo: "",
        image_tag: "",
    };
    for present in [false, true] {
        if present {
            meta.put_mcp_org_secret(
                seal_org_secret(kek.as_ref(), "reserved", b"private")
                    .await
                    .unwrap(),
                &r,
            )
            .await
            .unwrap();
        }
        for (name, reference) in [("reserved", None), ("alias", Some("reserved"))] {
            let schema = SecretSchema {
                r#ref: reference.map(String::from),
                ..Default::default()
            };
            assert!(matches!(
                store.get(&ctx, name, &schema).await,
                Err(SecretError::Unauthorized(_))
            ));
            assert!(matches!(
                store
                    .resolve(
                        &ctx,
                        &HashMap::from([(name.into(), schema)]),
                        Some(&HashMap::from([(name.into(), "override".into())]))
                    )
                    .await,
                Err(SecretError::Unauthorized(_))
            ));
        }
    }
}

#[tokio::test]
async fn legacy_mcp_bindings_cannot_fetch_or_update_guest_credentials() {
    use engram_coordinator::oauth::{OAuthManager, OAuthServiceError};
    use engram_core::types::oauth::{
        NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthSubjectKind,
        SessionOAuthBinding,
    };
    use engram_crypto::CredCipher;
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(4));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let kek = Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test"));
    let key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Mcp,
        subject_id: "connection-a".into(),
        provider: "openai-codex".into(),
    };
    let sealed = CredCipher::new(kek.as_ref())
        .seal(b"private-bundle")
        .await
        .unwrap();
    meta.put_oauth_credential(
        NewSealedOAuthCredential {
            key: key.clone(),
            wrapped_dek: sealed.wrapped_dek,
            nonce: sealed.nonce.to_vec(),
            ciphertext: sealed.ciphertext,
            key_id: sealed.key_id,
            metadata: OAuthAccountMetadata::default(),
            expires_at: None,
        },
        None,
    )
    .await
    .unwrap();
    let sid = engram_core::SessionId::from(uuid::Uuid::from_u128(4));
    meta.with_db_mut(|db| {
        db.session_oauth_bindings.insert(
            sid,
            SessionOAuthBinding {
                session_id: sid,
                key,
            },
        );
    });
    let manager = OAuthManager::new(
        meta,
        kek,
        clock,
        entropy,
        Arc::new(StaticSecretStore::new(HashMap::new())),
    );
    assert!(matches!(
        manager.fetch_session(sid).await,
        Err(OAuthServiceError::BadRequest(_))
    ));
    assert!(matches!(
        manager.update_session(sid, 1, b"replacement").await,
        Err(OAuthServiceError::BadRequest(_))
    ));
    assert!(matches!(
        manager.report_session_broken(sid, 1, "revoked").await,
        Err(OAuthServiceError::BadRequest(_))
    ));
}
