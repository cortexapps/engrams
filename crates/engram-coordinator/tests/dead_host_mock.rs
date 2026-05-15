//! Trait-layer tests for `MetadataStore::mark_host_dead_and_reassign_sessions`.
//!
//! The actual detector loop (advisory locks, polling cadence, NOTIFY
//! emission) is Postgres-specific and tested separately against a
//! live database (`#[ignore]`'d). What we lock down here is the
//! semantic contract of the trait method itself: which sessions get
//! transitioned, which don't, and how the affected-list is reported.
//! Both Postgres and Mock impls must match these assertions.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SessionId};
use parking_lot::Mutex;

#[derive(Default)]
struct MiniMeta {
    sessions: Mutex<HashMap<SessionId, Session>>,
}

#[async_trait]
impl MetadataStore for MiniMeta {
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        let id = SessionId::new();
        self.sessions.lock().insert(
            id,
            Session {
                id,
                user_id: spec.user_id,
                status: SessionStatus::Pending,
                host_id: None,
                sandbox_id: None,
                created_at: Utc::now(),
                image: spec.image,
                harness: spec.harness,
                last_active_at: Utc::now(),
            },
        );
        Ok(id)
    }
    async fn create_session_active(
        &self,
        session_id: SessionId,
        spec: SessionSpec,
        host_id: engram_core::HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        self.sessions.lock().insert(
            session_id,
            Session {
                id: session_id,
                user_id: spec.user_id,
                status: SessionStatus::Active,
                host_id: Some(host_id),
                sandbox_id: Some(sandbox_id),
                created_at: Utc::now(),
                image: spec.image,
                harness: spec.harness,
                last_active_at: Utc::now(),
            },
        );
        Ok(())
    }
    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        self.sessions
            .lock()
            .get(&id)
            .cloned()
            .ok_or(MetaError::NotFound)
    }
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(self.sessions.lock().values().cloned().collect())
    }
    async fn set_session_status(
        &self,
        id: SessionId,
        status: SessionStatus,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.status = status;
        Ok(())
    }
    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.host_id = host_id;
        Ok(())
    }
    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<engram_core::SandboxId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.sandbox_id = sandbox_id;
        Ok(())
    }
    async fn upsert_host(&self, _h: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn set_host_status(&self, _id: HostId, _s: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }
    async fn touch_host_heartbeat(
        &self,
        _id: HostId,
        _s: HostStatus,
        _cap: engram_core::types::HostCapacity,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_stale_hosts(&self, _threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn mark_host_dead_and_reassign_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SessionId>, MetaError> {
        let mut g = self.sessions.lock();
        let mut affected = Vec::new();
        for s in g.values_mut() {
            if s.host_id == Some(host_id)
                && !matches!(s.status, SessionStatus::Completed | SessionStatus::Failed)
            {
                s.host_id = None;
                s.sandbox_id = None;
                s.status = SessionStatus::Dead;
                s.last_active_at = Utc::now();
                affected.push(s.id);
            }
        }
        Ok(affected)
    }
    async fn record_snapshot(&self, _s: SnapshotRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_snapshots_for_session(
        &self,
        _id: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn latest_snapshot_for_session(
        &self,
        _id: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(None)
    }
    async fn append_session_event(
        &self,
        _s: SessionId,
        _k: &str,
        _p: serde_json::Value,
    ) -> Result<i64, MetaError> {
        Ok(0)
    }
    async fn list_session_events_since(
        &self,
        _s: SessionId,
        _since: i64,
        _limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(Vec::new())
    }
    async fn upsert_registry_credential(
        &self,
        _: engram_core::types::RegistryCredential,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_registry_credentials(
        &self,
    ) -> Result<Vec<engram_core::types::RegistryCredential>, MetaError> {
        Ok(Vec::new())
    }
    async fn registry_credential_for_host(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
        Ok(None)
    }
    async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_harness_pack(
        &self,
        _: engram_core::types::HarnessPack,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_harness_packs(&self) -> Result<Vec<engram_core::types::HarnessPack>, MetaError> {
        Ok(Vec::new())
    }
    async fn get_harness_pack(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::HarnessPack>, MetaError> {
        Ok(None)
    }
    async fn delete_harness_pack(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_enabled_image(
        &self,
        _: engram_core::types::EnabledImage,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_enabled_images(
        &self,
    ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
        Ok(Vec::new())
    }
    async fn get_enabled_image(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(None)
    }
    async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_session_secrets(
        &self,
        _: engram_core::types::SessionSecrets,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_session_secrets(
        &self,
        _: SessionId,
    ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
        Ok(None)
    }
    async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
        Ok(())
    }
}

async fn seed_session(meta: &MiniMeta, host: HostId, status: SessionStatus) -> SessionId {
    use engram_core::types::session::HarnessSpec;
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm-test".into(),
            harness: HarnessSpec::None,
            user_id: None,
        })
        .await
        .unwrap();
    meta.assign_session_host(id, Some(host)).await.unwrap();
    meta.set_session_status(id, status).await.unwrap();
    id
}

#[tokio::test]
async fn evacuates_active_and_idle_sessions_clears_host_id() {
    let meta = MiniMeta::default();
    let host = HostId::new();
    let s_active = seed_session(&meta, host, SessionStatus::Active).await;
    let s_idle = seed_session(&meta, host, SessionStatus::Idle).await;

    let affected = meta
        .mark_host_dead_and_reassign_sessions(host)
        .await
        .unwrap();

    let mut affected_sorted = affected.clone();
    affected_sorted.sort();
    let mut expected = vec![s_active, s_idle];
    expected.sort();
    assert_eq!(
        affected_sorted, expected,
        "both Active and Idle sessions on the dead host must transition"
    );

    let s_active_row = meta.get_session(s_active).await.unwrap();
    assert_eq!(s_active_row.status, SessionStatus::Dead);
    assert_eq!(s_active_row.host_id, None);

    let s_idle_row = meta.get_session(s_idle).await.unwrap();
    assert_eq!(s_idle_row.status, SessionStatus::Dead);
    assert_eq!(s_idle_row.host_id, None);
}

#[tokio::test]
async fn skips_terminal_sessions_even_on_dead_host() {
    // Completed and Failed sessions are sticky — they don't get
    // re-routed even if their last host_id pointed at a dead host.
    // Otherwise the next access to a deleted session would attempt
    // to revive it via /resume.
    let meta = MiniMeta::default();
    let host = HostId::new();
    let s_done = seed_session(&meta, host, SessionStatus::Completed).await;
    let s_failed = seed_session(&meta, host, SessionStatus::Failed).await;
    let s_active = seed_session(&meta, host, SessionStatus::Active).await;

    let affected = meta
        .mark_host_dead_and_reassign_sessions(host)
        .await
        .unwrap();

    assert_eq!(
        affected,
        vec![s_active],
        "only the non-terminal session should be reassigned"
    );
    assert_eq!(
        meta.get_session(s_done).await.unwrap().status,
        SessionStatus::Completed,
        "completed must stay completed"
    );
    assert_eq!(
        meta.get_session(s_failed).await.unwrap().status,
        SessionStatus::Failed
    );
}

#[tokio::test]
async fn does_not_touch_sessions_on_other_hosts() {
    let meta = MiniMeta::default();
    let dead_host = HostId::new();
    let live_host = HostId::new();
    let s_dead = seed_session(&meta, dead_host, SessionStatus::Active).await;
    let s_live = seed_session(&meta, live_host, SessionStatus::Active).await;

    let affected = meta
        .mark_host_dead_and_reassign_sessions(dead_host)
        .await
        .unwrap();

    assert_eq!(affected, vec![s_dead]);

    let s_live_row = meta.get_session(s_live).await.unwrap();
    assert_eq!(
        s_live_row.host_id,
        Some(live_host),
        "sessions on a different host must keep their host_id"
    );
    assert_eq!(s_live_row.status, SessionStatus::Active);
}

#[tokio::test]
async fn idempotent_on_already_dead_host() {
    // A second call on the same dead host returns an empty vec and
    // doesn't error — important because the detector loop may race
    // with a `pg_notify` arrival, both trying to act on the same
    // host. The second caller just gets nothing to do.
    let meta = MiniMeta::default();
    let host = HostId::new();
    let _s = seed_session(&meta, host, SessionStatus::Active).await;

    let first = meta
        .mark_host_dead_and_reassign_sessions(host)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);

    let second = meta
        .mark_host_dead_and_reassign_sessions(host)
        .await
        .unwrap();
    assert!(
        second.is_empty(),
        "second call on an already-evacuated host must return empty"
    );
}

#[tokio::test]
async fn host_with_no_sessions_returns_empty() {
    let meta = MiniMeta::default();
    let lonely_host = HostId::new();
    let affected = meta
        .mark_host_dead_and_reassign_sessions(lonely_host)
        .await
        .unwrap();
    assert!(affected.is_empty());
}

#[tokio::test]
async fn arc_dyn_metadata_store_dispatches_correctly() {
    // The detector takes `Arc<dyn MetadataStore>`; verify dynamic
    // dispatch reaches the same impl. (Compile-time check + smoke
    // call.)
    let meta: Arc<dyn MetadataStore> = Arc::new(MiniMeta::default());
    let result = meta
        .mark_host_dead_and_reassign_sessions(HostId::new())
        .await;
    assert!(result.is_ok());
}
