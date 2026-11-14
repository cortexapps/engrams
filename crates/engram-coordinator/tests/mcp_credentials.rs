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

mod publication_race {
    use super::*;
    use engram_core::traits::{
        CreateDisposition, DisableEnabledImageOutcome, SessionCreateWriteSet,
    };
    use engram_core::types::event::{ArtifactRow, EventCursor, PersistedEvent};
    use engram_core::types::host::{HostHeartbeat, HostRecord, HostStatus};
    use engram_core::types::registry::{EnabledImage, RegistryCredential, SessionSecrets};
    use engram_core::types::session::{BindingDisposition, Session, SessionSpec, SessionState};
    use engram_core::types::snapshot::SnapshotRecord;
    use engram_core::{HostId, MetaError, SandboxId, SessionId};
    use tokio::sync::Notify;

    // Forward to the real simulator. Only the payload-read boundary is paused;
    // reservation, ciphertext publication, and all purpose reads keep real behavior.
    struct PausedMeta {
        inner: Arc<SimMetadataStore>,
        read_started: Notify,
        resume_read: Notify,
    }

    macro_rules! delegate_required {
        ($( $method:ident ( $( $arg:ident: $ty:ty ),* $(,)? ) -> $ret:ty; )*) => {
            #[async_trait::async_trait]
            impl MetadataStore for PausedMeta {
                async fn get_mcp_org_secret_reservation(&self, name: &str) -> Result<Option<engram_core::types::org_secret::McpSecretReservation>, MetaError> {
                    self.inner.get_mcp_org_secret_reservation(name).await
                }
                async fn get_org_secret_sealed(&self, name: &str) -> Result<Option<engram_core::types::org_secret::SealedOrgSecret>, MetaError> {
                    self.read_started.notify_one();
                    self.resume_read.notified().await;
                    self.inner.get_org_secret_sealed(name).await
                }
                $(async fn $method(&self, $( $arg: $ty ),*) -> $ret {
                    self.inner.$method($( $arg ),*).await
                })*
            }
        }
    }

    delegate_required! {
        create_session(spec: SessionSpec) -> Result<SessionId, MetaError>;
        transition_session_created(session_id: SessionId, sandbox_id: SandboxId,) -> Result<u64, MetaError>;
        get_session(id: SessionId) -> Result<Session, MetaError>;
        list_active_sessions() -> Result<Vec<Session>, MetaError>;
        reserve_and_persist_create(ws: SessionCreateWriteSet, candidates: &[HostId], affinity_len: usize,) -> Result<CreateDisposition, MetaError>;
        transition_session(id: SessionId, target: SessionState, disposition: BindingDisposition,) -> Result<SessionState, MetaError>;
        assign_session_host(id: SessionId, host_id: Option<HostId>,) -> Result<(), MetaError>;
        upsert_host(host: HostRecord) -> Result<(), MetaError>;
        list_active_hosts() -> Result<Vec<HostRecord>, MetaError>;
        set_host_status(id: HostId, status: HostStatus) -> Result<(), MetaError>;
        touch_host_heartbeat(id: HostId, hb: HostHeartbeat,) -> Result<engram_core::types::host::HeartbeatAck, MetaError>;
        set_host_cordon(id: HostId, owner: Option<engram_core::types::host::CordonOwner>, reason: Option<&str>,) -> Result<(), MetaError>;
        mark_host_dead_if_lease_expired(host_id: HostId,) -> Result<Vec<(SessionId, SessionState)>, MetaError>;
        record_snapshot(snap: SnapshotRecord) -> Result<bool, MetaError>;
        list_snapshots_for_session(sid: SessionId,) -> Result<Vec<SnapshotRecord>, MetaError>;
        latest_snapshot_for_session(sid: SessionId,) -> Result<Option<SnapshotRecord>, MetaError>;
        append_session_event(session_id: SessionId, kind: &str, payload: serde_json::Value,) -> Result<i64, MetaError>;
        list_session_events_window(session_id: SessionId, cursor: EventCursor, limit: i64, kinds: &[String], tool_names: &[String],) -> Result<Vec<PersistedEvent>, MetaError>;
        insert_artifact(id: uuid::Uuid, session_id: SessionId, blob_key: &str, media_type: &str, size_bytes: i64, caption: Option<&str>, file_name: Option<&str>,) -> Result<(), MetaError>;
        get_artifact(session_id: SessionId, id: uuid::Uuid,) -> Result<Option<ArtifactRow>, MetaError>;
        artifact_usage(session_id: SessionId) -> Result<(i64, i64), MetaError>;
        upsert_registry_credential(cred: RegistryCredential) -> Result<(), MetaError>;
        list_registry_credentials() -> Result<Vec<RegistryCredential>, MetaError>;
        registry_credential_for_host(registry_host: &str,) -> Result<Option<RegistryCredential>, MetaError>;
        delete_registry_credential(registry_host: &str) -> Result<(), MetaError>;
        upsert_enabled_image(image: EnabledImage) -> Result<(), MetaError>;
        list_enabled_images() -> Result<Vec<EnabledImage>, MetaError>;
        get_enabled_image(image_uri: &str) -> Result<Option<EnabledImage>, MetaError>;
        get_enabled_image_any(image_uri: &str,) -> Result<Option<EnabledImage>, MetaError>;
        soft_delete_enabled_image(image_uri: &str,) -> Result<DisableEnabledImageOutcome, MetaError>;
        delete_enabled_image(image_uri: &str) -> Result<(), MetaError>;
        get_session_secrets(session_id: SessionId,) -> Result<Option<SessionSecrets>, MetaError>;
        delete_session_secrets(session_id: SessionId) -> Result<(), MetaError>;
    }

    #[tokio::test]
    async fn reservation_published_after_initial_check_blocks_payload_and_fallback() {
        for publish_value in [true, false] {
            let meta = SimMetadataStore::new(ManualClock::new(), Arc::new(SimEntropy::seeded(4)));
            let paused = Arc::new(PausedMeta {
                inner: meta.clone(),
                read_started: Notify::new(),
                resume_read: Notify::new(),
            });
            let kek = Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test"));
            let store = LayeredSecretStore::new(vec![
                Arc::new(OrgSecretBackend::new(paused.clone(), kek.clone())),
                Arc::new(StaticSecretStore::new(HashMap::from([(
                    "fresh".into(),
                    "fallback-value".into(),
                )]))),
            ]);
            let reader = tokio::spawn(async move {
                store
                    .get(
                        &SecretContext {
                            repo: "",
                            image_tag: "",
                        },
                        "fresh",
                        &SecretSchema::default(),
                    )
                    .await
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                paused.read_started.notified(),
            )
            .await
            .unwrap();
            // The initial generic checks have returned no reservation. The read
            // is now suspended before it can observe the newly published value.
            let reserved = meta
                .reserve_mcp_org_secret("fresh", "connection-a", McpSecretRole::ApiKey)
                .await
                .unwrap();
            if publish_value {
                meta.put_mcp_org_secret(
                    seal_org_secret(kek.as_ref(), "fresh", b"private-value")
                        .await
                        .unwrap(),
                    &reserved,
                )
                .await
                .unwrap();
            }
            paused.resume_read.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), reader)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(result, Err(SecretError::Unauthorized(_))),
                "a reservation published during a generic read must block plaintext and fallback"
            );
        }
    }
}

#[tokio::test]
async fn generic_connector_resolver_rejects_mcp_before_loading_credentials() {
    use engram_coordinator::oauth::{OAuthManager, OAuthServiceError};
    use engram_core::types::oauth::{OAuthCredentialKey, OAuthSubjectKind};
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(4));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let manager = OAuthManager::new(
        meta,
        Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test")),
        clock,
        entropy,
        Arc::new(StaticSecretStore::new(HashMap::new())),
    );
    assert!(matches!(
        manager
            .resolve_connector_token(&OAuthCredentialKey {
                subject_kind: OAuthSubjectKind::Mcp,
                subject_id: "connection-a".into(),
                provider: "registered-a".into()
            })
            .await,
        Err(OAuthServiceError::BadRequest(_))
    ));
}
