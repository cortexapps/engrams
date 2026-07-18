//! ADR 0016 Phase C: chunk-GC pin-set collection.
//!
//! [`PinSet`] is the union of every chunk hash currently referenced
//! by a live row in the metadata store. Six pin-set sources:
//!
//! 1. `enabled_images.disk_manifest_*` — every chunked-disk enabled
//!    image pins its base manifest's chunks.
//! 2. `sessions.live_disk_manifest_*` — every Active session pins
//!    its current `FlushScheduler` manifest version.
//! 3. `snapshots.disk_manifest_*` WHERE `recoverable=true` — every
//!    recoverable snapshot pins its captured disk manifest.
//! 4. `snapshots.memory_manifest_*` WHERE `recoverable=true` —
//!    memory-side mirror.
//! 5. `enabled_images.base_snapshot_memory_manifest_*` (ADR 0022) —
//!    the per-template base memfile's backing chunks. Pinned for the
//!    template's enabled lifetime, independent of the base snapshot
//!    row's `recoverable` flag (the memfile is a shared inode every
//!    same-template `session.create` sibling `MAP_PRIVATE`s).
//! 6. `enabled_images.base_snapshot_disk_manifest_*` (ADR 0022) —
//!    the disk companion to #5: the rootfs a base `session.create`
//!    restores from, also recoverable-flag-independent.
//! 7. `cold_bases.{disk,memory}_manifest` (ADR 0084 §B6) — a cold
//!    base's chunks have NO other root: it never gets its own
//!    `snapshots` row (only the warm overlay it seeds does), so
//!    without this a live `cold_bases` row's chunks would be reaped
//!    right out from under it.
//!
//! Sources 5 & 6 frequently dedup against #3/#4 (the base snapshot is
//! usually still `recoverable`); their distinct value is keeping the
//! memfile + rootfs pinned even if that flag is ever cleared while an
//! enabled template still has live sharers.
//!
//! Each source returns a `Vec<ManifestRef>`; we dedup at the
//! `ManifestRef` level (a manifest referenced by both a session and
//! a snapshot is fetched once), fetch the manifest JSON from the
//! chunk store (bounded by a 16-permit semaphore), and fold
//! `manifest.chunks[].hash` into a single `HashSet<ChunkHash>`.
//!
//! Failure surface: a `MetaError` from any list query, or a
//! `ChunkStoreError` from any manifest fetch, fails the whole
//! collection. A partial pin set is dangerous — the sweep would
//! mark currently-live chunks as candidates. The caller (commit 4's
//! sweep orchestrator) retries on the next interval tick.

use std::collections::HashSet;
use std::sync::Arc;

use engram_core::error::MetaError;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;

use crate::error::ChunkStoreError;
use crate::manifest::ChunkHash;
use crate::store::ChunkStore;

/// Concurrency cap on parallel manifest fetches during pin-set
/// collection. Sized for fleet-scale (100s of manifests) without
/// saturating BlobStorage egress. Override via
/// [`PinSet::collect_with_concurrency`] in tests or future
/// operator tuning.
pub const DEFAULT_COLLECT_CONCURRENCY: usize = 16;

/// Errors from [`PinSet::collect`].
#[derive(Debug)]
pub enum GcError {
    /// One of the four PG list queries failed. Partial pin set is
    /// not safe to expose — caller retries.
    Meta(MetaError),
    /// A manifest fetch from the chunk store failed. Partial pin
    /// set is not safe — caller retries.
    ChunkStore(ChunkStoreError),
}

impl std::fmt::Display for GcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Meta(e) => write!(f, "metadata store: {e}"),
            Self::ChunkStore(e) => write!(f, "chunk store: {e}"),
        }
    }
}

impl std::error::Error for GcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Meta(e) => Some(e),
            Self::ChunkStore(e) => Some(e),
        }
    }
}

impl From<MetaError> for GcError {
    fn from(e: MetaError) -> Self {
        Self::Meta(e)
    }
}

impl From<ChunkStoreError> for GcError {
    fn from(e: ChunkStoreError) -> Self {
        Self::ChunkStore(e)
    }
}

/// The set of chunk hashes pinned by live PG rows. Phase C's sweep
/// collects this once per iteration and compares each blob in
/// `chunks/sha256/` against it; anything not in the set becomes a
/// GC candidate.
#[derive(Debug, Default, Clone)]
pub struct PinSet {
    chunks: HashSet<ChunkHash>,
}

impl PinSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, hash: &ChunkHash) -> bool {
        self.chunks.contains(hash)
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ChunkHash> {
        self.chunks.iter()
    }

    /// Direct insert — bypasses manifest fetch. Used by tests; the
    /// sweep orchestrator (commit 4) reaches for this when it
    /// wants to add a chunk to the pin set without re-fetching a
    /// manifest it already enumerated.
    pub fn insert(&mut self, hash: ChunkHash) -> bool {
        self.chunks.insert(hash)
    }

    /// Union over the four pin-set sources. See module docs.
    pub async fn collect(
        meta: &dyn MetadataStore,
        chunk_store: &ChunkStore,
    ) -> Result<Self, GcError> {
        Self::collect_with_concurrency(meta, chunk_store, DEFAULT_COLLECT_CONCURRENCY).await
    }

    /// Variant with an explicit concurrency cap for the manifest-
    /// fetch fan-out. Defaults to [`DEFAULT_COLLECT_CONCURRENCY`].
    pub async fn collect_with_concurrency(
        meta: &dyn MetadataStore,
        chunk_store: &ChunkStore,
        concurrency: usize,
    ) -> Result<Self, GcError> {
        let concurrency = concurrency.max(1);
        let refs = collect_manifest_refs(meta).await?;

        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut fetches = FuturesUnordered::new();
        for r in refs {
            let permit_sem = semaphore.clone();
            let cs = chunk_store;
            fetches.push(async move {
                // Hold the permit for the duration of the fetch;
                // dropped on task exit so the next manifest can
                // start.
                let _permit = permit_sem
                    .acquire_owned()
                    .await
                    .expect("PinSet semaphore must not be closed");
                cs.get_manifest(r).await
            });
        }

        let mut chunks: HashSet<ChunkHash> = HashSet::new();
        while let Some(result) = fetches.next().await {
            let manifest = result?;
            for chunk_ref in manifest.chunks {
                chunks.insert(chunk_ref.hash);
            }
        }
        Ok(Self { chunks })
    }
}

/// Run the four list queries and dedup their results at the
/// `ManifestRef` level. Returned set is the input to the manifest-
/// fetch fan-out.
async fn collect_manifest_refs(meta: &dyn MetadataStore) -> Result<HashSet<ManifestRef>, GcError> {
    let mut refs: HashSet<ManifestRef> = HashSet::new();
    for r in meta.list_enabled_image_disk_manifest_ids().await? {
        refs.insert(r);
    }
    for r in meta.list_live_session_disk_manifest_ids().await? {
        refs.insert(r);
    }
    for r in meta.list_recoverable_snapshot_disk_manifests().await? {
        refs.insert(r);
    }
    for r in meta.list_recoverable_snapshot_memory_manifests().await? {
        refs.insert(r);
    }
    // ADR 0022 Option A: sources #5 + #6 — the per-template base memfile
    // (memory) and its rootfs (disk) companion, pinned for the enabled
    // lifetime independent of the base snapshot row's `recoverable` flag.
    for r in meta
        .list_enabled_image_base_snapshot_memory_manifests()
        .await?
    {
        refs.insert(r);
    }
    for r in meta
        .list_enabled_image_base_snapshot_disk_manifests()
        .await?
    {
        refs.insert(r);
    }
    // ADR 0084 §B6: source #7 — cold bases have no root of their own
    // besides this (no `snapshots` row); never a second GC.
    for r in meta.cold_base_manifest_refs().await? {
        refs.insert(r);
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a
    // decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]

    use std::sync::Arc;

    use async_trait::async_trait;
    use engram_core::traits::{BlobStorage, MetadataStore};
    use engram_core::types::manifest::ManifestRef;
    use engram_core::types::session::{SessionSpec, SessionState};
    use engram_core::types::Session;
    use engram_core::{HostId, MetaError, SandboxId, SessionId};
    use engram_storage_local::LocalBlobStorage;
    use uuid::Uuid;

    use super::*;
    use crate::manifest::ManifestKind;

    /// Minimal MetadataStore mock that overrides only the four
    /// pin-set source methods. Other methods fall through to the
    /// trait defaults (Ok(...) / NotFound).
    #[derive(Default)]
    struct PinSetMockMeta {
        enabled: Vec<ManifestRef>,
        live: Vec<ManifestRef>,
        snap_disk: Vec<ManifestRef>,
        snap_mem: Vec<ManifestRef>,
        // ADR 0022 sources #5 + #6.
        enabled_base_mem: Vec<ManifestRef>,
        enabled_base_disk: Vec<ManifestRef>,
    }

    #[async_trait]
    impl MetadataStore for PinSetMockMeta {
        // Required trait methods we don't exercise — minimal stubs.
        async fn create_session(&self, _spec: SessionSpec) -> Result<SessionId, MetaError> {
            Err(MetaError::NotFound)
        }
        async fn transition_session_created(
            &self,
            _session_id: SessionId,
            _sandbox_id: SandboxId,
        ) -> Result<(), MetaError> {
            Err(MetaError::NotFound)
        }
        async fn reserve_and_persist_create(
            &self,
            _ws: engram_core::traits::SessionCreateWriteSet,
            _candidates: &[HostId],
            _affinity_len: usize,
        ) -> Result<engram_core::traits::CreateDisposition, MetaError> {
            Err(MetaError::NotFound)
        }
        async fn get_session(&self, _id: SessionId) -> Result<Session, MetaError> {
            Err(MetaError::NotFound)
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(Vec::new())
        }
        async fn transition_session(
            &self,
            _id: SessionId,
            _target: SessionState,
        ) -> Result<SessionState, MetaError> {
            Err(MetaError::NotFound)
        }
        async fn assign_session_host(
            &self,
            _id: SessionId,
            _host_id: Option<HostId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            _id: SessionId,
            _sandbox_id: Option<SandboxId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn upsert_host(
            &self,
            _host: engram_core::types::HostRecord,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_active_hosts(
            &self,
        ) -> Result<Vec<engram_core::types::HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn set_host_status(
            &self,
            _id: HostId,
            _status: engram_core::types::HostStatus,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::host::HostHeartbeat,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn set_host_cordoned(&self, _: HostId, _: bool) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_stale_hosts(
            &self,
            _threshold_secs: u64,
        ) -> Result<Vec<engram_core::types::HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _host_id: HostId,
        ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(
            &self,
            _snap: engram_core::types::SnapshotRecord,
        ) -> Result<bool, MetaError> {
            Ok(true)
        }
        async fn list_snapshots_for_session(
            &self,
            _sid: SessionId,
        ) -> Result<Vec<engram_core::types::SnapshotRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn latest_snapshot_for_session(
            &self,
            _sid: SessionId,
        ) -> Result<Option<engram_core::types::SnapshotRecord>, MetaError> {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _session_id: SessionId,
            _kind: &str,
            _payload: serde_json::Value,
        ) -> Result<i64, MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _session_id: SessionId,
            _since: i64,
            _limit: i64,
        ) -> Result<Vec<engram_core::types::PersistedEvent>, MetaError> {
            Ok(Vec::new())
        }
        async fn insert_artifact(
            &self,
            _id: uuid::Uuid,
            _session_id: SessionId,
            _blob_key: &str,
            _media_type: &str,
            _size_bytes: i64,
            _caption: Option<&str>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_artifact(
            &self,
            _session_id: SessionId,
            _id: uuid::Uuid,
        ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
            Ok(None)
        }
        async fn artifact_usage(&self, _session_id: SessionId) -> Result<(i64, i64), MetaError> {
            Ok((0, 0))
        }
        async fn upsert_registry_credential(
            &self,
            _cred: engram_core::types::RegistryCredential,
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
            _host: &str,
        ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
            Ok(None)
        }
        async fn delete_registry_credential(&self, _host: &str) -> Result<(), MetaError> {
            Ok(())
        }
        // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
        async fn upsert_enabled_image(
            &self,
            _image: engram_core::types::EnabledImage,
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
            _uri: &str,
        ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn get_enabled_image_any(
            &self,
            _uri: &str,
        ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn soft_delete_enabled_image(
            &self,
            _uri: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
            Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled)
        }
        async fn delete_enabled_image(&self, _uri: &str) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_session_secrets(
            &self,
            _session_id: SessionId,
        ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
            Ok(None)
        }
        async fn delete_session_secrets(&self, _session_id: SessionId) -> Result<(), MetaError> {
            Ok(())
        }

        // The four methods PinSet::collect actually drives.
        async fn list_enabled_image_disk_manifest_ids(
            &self,
        ) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.enabled.clone())
        }
        async fn list_live_session_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.live.clone())
        }
        async fn list_recoverable_snapshot_disk_manifests(
            &self,
        ) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.snap_disk.clone())
        }
        async fn list_recoverable_snapshot_memory_manifests(
            &self,
        ) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.snap_mem.clone())
        }
        async fn list_enabled_image_base_snapshot_memory_manifests(
            &self,
        ) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.enabled_base_mem.clone())
        }
        async fn list_enabled_image_base_snapshot_disk_manifests(
            &self,
        ) -> Result<Vec<ManifestRef>, MetaError> {
            Ok(self.enabled_base_disk.clone())
        }
    }

    /// Write a manifest containing `chunks_bytes` (each entry =
    /// one chunk's contents) into a fresh ChunkStore and return
    /// the resulting ManifestRef. Each entry's bytes are content-
    /// addressed by sha256, so identical content across calls yields
    /// the same `ChunkHash` (used to exercise pin-set dedup).
    async fn seed_manifest(
        store: &ChunkStore,
        chunks_bytes: &[&[u8]],
        kind: ManifestKind,
    ) -> ManifestRef {
        let chunk_size_bytes = kind.default_chunk_size();
        // total_bytes must be > the largest offset in the manifest,
        // per `Manifest::validate`. Offsets are i * chunk_size for
        // i in 0..N, so total_bytes = N * chunk_size is the minimum.
        let total_bytes = (chunks_bytes.len() as u64) * chunk_size_bytes;
        let mut manifest = crate::manifest::Manifest::empty(kind, total_bytes);
        for (i, bytes) in chunks_bytes.iter().enumerate() {
            let hash = store.put_chunk(bytes).await.expect("put chunk");
            manifest.chunks.push(crate::manifest::ChunkRef {
                offset: (i as u64) * chunk_size_bytes,
                hash,
            });
        }
        let r = ManifestRef::new();
        store
            .put_manifest(r, &manifest)
            .await
            .expect("put manifest");
        r
    }

    fn fresh_store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        (store, dir)
    }

    #[tokio::test]
    async fn empty_meta_yields_empty_pin_set() {
        let (store, _dir) = fresh_store();
        let meta = PinSetMockMeta::default();
        let pin_set = PinSet::collect(&meta, &store).await.expect("collect");
        assert!(pin_set.is_empty(), "empty meta must produce empty pin set");
    }

    #[tokio::test]
    async fn four_sources_union_with_dedup() {
        let (store, _dir) = fresh_store();

        // Three manifests, three distinct chunk sets.
        let mref_a = seed_manifest(&store, &[b"aaa", b"bbb"], ManifestKind::Disk).await;
        let mref_b = seed_manifest(&store, &[b"ccc", b"ddd"], ManifestKind::Disk).await;
        let mref_c = seed_manifest(&store, &[b"eee", b"fff"], ManifestKind::Memory).await;

        // Reference mref_a from two sources to prove dedup at the
        // manifest level (single fetch, not double).
        let meta = PinSetMockMeta {
            enabled: vec![mref_a],
            live: vec![mref_a, mref_b],
            snap_disk: vec![mref_b],
            snap_mem: vec![mref_c],
            ..Default::default()
        };

        let pin_set = PinSet::collect(&meta, &store).await.expect("collect");
        // 3 manifests × 2 chunks each, all distinct = 6 chunks.
        assert_eq!(pin_set.len(), 6, "union must be 6 distinct chunks");

        // Spot-check membership.
        let hash_aaa = crate::manifest::ChunkHash::of(b"aaa");
        let hash_fff = crate::manifest::ChunkHash::of(b"fff");
        assert!(pin_set.contains(&hash_aaa));
        assert!(pin_set.contains(&hash_fff));

        let hash_unknown = crate::manifest::ChunkHash::of(b"never-pinned");
        assert!(!pin_set.contains(&hash_unknown));
    }

    #[tokio::test]
    async fn base_snapshot_memfile_pinned_via_enabled_image_only() {
        // ADR 0022 sources #5/#6: a base snapshot's memfile (memory) +
        // rootfs (disk) manifests must stay pinned via the enabled_images
        // row even when NO recoverable snapshot references them (i.e. the
        // base snapshot row's `recoverable` flag was cleared while the
        // template is still enabled and has live sharers). Here snap_disk
        // / snap_mem are empty on purpose — the only path to these
        // manifests is the new enabled-image base-snapshot sources.
        let (store, _dir) = fresh_store();
        let base_mem = seed_manifest(&store, &[b"mem-1", b"mem-2"], ManifestKind::Memory).await;
        let base_disk = seed_manifest(&store, &[b"disk-1", b"disk-2"], ManifestKind::Disk).await;

        let meta = PinSetMockMeta {
            enabled_base_mem: vec![base_mem],
            enabled_base_disk: vec![base_disk],
            ..Default::default()
        };

        let pin_set = PinSet::collect(&meta, &store).await.expect("collect");
        assert_eq!(
            pin_set.len(),
            4,
            "both base manifests' chunks must be pinned"
        );
        for c in [b"mem-1".as_slice(), b"mem-2", b"disk-1", b"disk-2"] {
            assert!(
                pin_set.contains(&crate::manifest::ChunkHash::of(c)),
                "base-snapshot chunk must be pinned via enabled_images source",
            );
        }
    }

    #[tokio::test]
    async fn shared_chunks_across_manifests_dedup_in_set() {
        let (store, _dir) = fresh_store();

        // Two manifests share one chunk hash by content
        // (content-addressed → same hash).
        let mref_x = seed_manifest(&store, &[b"shared", b"unique-x"], ManifestKind::Disk).await;
        let mref_y = seed_manifest(&store, &[b"shared", b"unique-y"], ManifestKind::Memory).await;

        let meta = PinSetMockMeta {
            snap_disk: vec![mref_x],
            snap_mem: vec![mref_y],
            ..Default::default()
        };

        let pin_set = PinSet::collect(&meta, &store).await.expect("collect");
        // 2 manifests × 2 chunks = 4 slots, but `shared` collides
        // to 1 hash → 3 distinct chunks in the pin set.
        assert_eq!(pin_set.len(), 3, "shared chunk must dedup to 1 entry");
    }

    #[tokio::test]
    async fn manifest_fetch_failure_aborts_collection() {
        let (store, _dir) = fresh_store();
        // Reference a manifest_id that was never committed → fetch
        // returns BlobError::NotFound → GcError::ChunkStore.
        let bogus = ManifestRef {
            manifest_id: Uuid::new_v4(),
            version: 1,
        };
        let meta = PinSetMockMeta {
            enabled: vec![bogus],
            ..Default::default()
        };
        let result = PinSet::collect(&meta, &store).await;
        match result {
            Err(GcError::ChunkStore(_)) => {}
            other => panic!("expected ChunkStore error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn concurrency_one_still_collects_correctly() {
        let (store, _dir) = fresh_store();
        let mref_a = seed_manifest(&store, &[b"alpha"], ManifestKind::Disk).await;
        let mref_b = seed_manifest(&store, &[b"beta"], ManifestKind::Disk).await;
        let meta = PinSetMockMeta {
            enabled: vec![mref_a, mref_b],
            ..Default::default()
        };
        // Sequential fan-out — proves the bounded path matches the
        // default-concurrency path.
        let pin_set = PinSet::collect_with_concurrency(&meta, &store, 1)
            .await
            .expect("collect with concurrency=1");
        assert_eq!(pin_set.len(), 2);
    }
}
