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

/// Hash-prefix shards the chunk space splits into — one per value of a
/// hash's first byte. Mirrors the coordinator's sweep sharding.
pub const SHARD_SPACE: usize = 256;

/// Every shard enabled: the unfiltered pin set.
pub const ALL_SHARDS: [bool; SHARD_SPACE] = [true; SHARD_SPACE];

/// Inclusive-exclusive `content_hash` bounds for one shard, for a PK
/// range scan on `chunk_gc_candidates`.
pub fn shard_hash_bounds(shard: u8) -> (Vec<u8>, Vec<u8>) {
    let mut lo = vec![0u8; 32];
    lo[0] = shard;
    let mut hi = vec![0u8; 32];
    if shard == u8::MAX {
        // Past the last shard: an all-0xFF..FF+1 bound is unrepresentable
        // in 32 bytes, so use a 33-byte value that sorts after every hash.
        hi = vec![0xFFu8; 33];
    } else {
        hi[0] = shard + 1;
    }
    (lo, hi)
}

/// Outcome of [`PinSet::collect_converging`].
#[derive(Debug)]
pub struct PinSetCollection {
    /// A superset of the live pin set as of the final ref read.
    pub pin_set: PinSet,
    /// Fetch rounds it took. `1` means nothing was published while the
    /// manifests were being read — the common case.
    pub rounds: usize,
    /// Whether the ref set went quiet before `max_rounds`. `false` still
    /// yields a usable set (see the method docs); it signals publish
    /// pressure, not an error.
    pub converged: bool,
}

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
    pub fn contains(&self, hash: &ChunkHash) -> bool {
        self.chunks.contains(hash)
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Union over the four pin-set sources. See module docs.
    pub async fn collect(
        meta: &dyn MetadataStore,
        chunk_store: &ChunkStore,
    ) -> Result<Self, GcError> {
        Self::collect_with_concurrency(meta, chunk_store, DEFAULT_COLLECT_CONCURRENCY).await
    }

    /// Collect a pin set that is provably complete as of a recent
    /// instant, by CONVERGING ON THE REF SET rather than on
    /// `chunk_generation`.
    ///
    /// `collect` is not atomic: it runs seven un-transacted list queries
    /// and then fans out manifest fetches over a wall-clock window, so a
    /// manifest published mid-collect can be missed. The previous guard
    /// bracketed the collect between two `chunk_generation` reads and
    /// retried when it moved.
    ///
    /// That used the wrong signal. `chunk_generation` ticks on EVERY
    /// flush / enable_image / record_snapshot, most of which do not
    /// change the ref set at all — so it fires constantly, and each false
    /// positive costs a full re-fetch of every manifest (~4 GB in prod).
    /// Measured 2026-09-01: 43% of sweeps exhausted the retry budget and
    /// skipped their drain entirely.
    ///
    /// A `ManifestRef` is `(manifest_id, version)`, so a new manifest
    /// VERSION is a new ref. Re-reading the ref set therefore detects
    /// exactly the publishes that can affect the pin set, and nothing
    /// else — and it costs seven cheap SQL queries instead of gigabytes
    /// of blob reads. Each round fetches only refs not already fetched,
    /// so convergence is cheap even when it takes several passes.
    ///
    /// The result is a SUPERSET of the true pin set as of the final ref
    /// read. A superset is the safe direction for both callers: the mark
    /// pass marks fewer chunks and the promote pass deletes fewer, so an
    /// extra entry costs a little efficiency and never a live chunk.
    ///
    /// Hitting `max_rounds` is NOT a failure and never justifies skipping
    /// a drain: every ref seen up to that point has been fetched, so the
    /// set is still valid as of the last read. It only means the fleet is
    /// publishing manifests faster than the walk converges, which is
    /// worth a metric.
    ///
    /// The residual race — a manifest published after the final ref read
    /// but before a delete — is inherent to any lock-free design and is
    /// what the promote pass's per-candidate re-check exists to narrow.
    pub async fn collect_converging(
        meta: &dyn MetadataStore,
        chunk_store: &ChunkStore,
        concurrency: usize,
        max_rounds: usize,
    ) -> Result<PinSetCollection, GcError> {
        Self::collect_converging_for_shards(meta, chunk_store, concurrency, max_rounds, &ALL_SHARDS)
            .await
    }

    /// As [`Self::collect_converging`], but keeping ONLY hashes whose
    /// first byte is an enabled shard.
    ///
    /// This is what stops pin-set memory scaling with the fleet. The set
    /// was a materialized `HashSet` of EVERY pinned chunk — ~14M hashes,
    /// 500 MB to 1 GB of coordinator heap — and it grows forever, so any
    /// memory limit only decides when the OOM lands. It landed on
    /// 2026-09-03: the sweep was OOMKilled repeatedly, the cursor froze,
    /// and reclamation stopped for four days after freeing 35 TB.
    ///
    /// The key space was already walked in hash-prefix shards; the pin
    /// set simply was not. A chunk `ab...` can only ever be pinned-or-not
    /// while walking shard `ab`, so every entry outside the shards this
    /// tick will touch is dead weight for the entire walk. Filtering
    /// during the fan-out costs NOTHING extra — same single pass over the
    /// manifests — and makes memory `pin_set * shards_this_tick / 256`,
    /// i.e. a tuning knob rather than a function of fleet size.
    pub async fn collect_converging_for_shards(
        meta: &dyn MetadataStore,
        chunk_store: &ChunkStore,
        concurrency: usize,
        max_rounds: usize,
        shards: &[bool; SHARD_SPACE],
    ) -> Result<PinSetCollection, GcError> {
        let concurrency = concurrency.max(1);
        let max_rounds = max_rounds.max(1);
        let mut fetched: HashSet<ManifestRef> = HashSet::new();
        let mut chunks: HashSet<ChunkHash> = HashSet::new();
        let mut rounds = 0usize;

        loop {
            let refs = collect_manifest_refs(meta).await?;
            let new: Vec<ManifestRef> = refs.difference(&fetched).copied().collect();
            if new.is_empty() {
                // The ref set went quiet: nothing was published while we
                // fetched, so the set is complete as of this read.
                return Ok(PinSetCollection {
                    pin_set: Self { chunks },
                    rounds,
                    converged: true,
                });
            }

            Self::fetch_into(chunk_store, &new, concurrency, shards, &mut chunks).await?;
            fetched.extend(new);
            rounds += 1;

            if rounds >= max_rounds {
                // Everything seen so far HAS been fetched, so the set is
                // still a valid superset as of the last read — just not
                // proven quiet. Callers must not treat this as a failure.
                return Ok(PinSetCollection {
                    pin_set: Self { chunks },
                    rounds,
                    converged: false,
                });
            }
        }
    }

    /// Fetch `refs` with a bounded fan-out, folding their chunk hashes
    /// into `chunks`.
    async fn fetch_into(
        chunk_store: &ChunkStore,
        refs: &[ManifestRef],
        concurrency: usize,
        shards: &[bool; SHARD_SPACE],
        chunks: &mut HashSet<ChunkHash>,
    ) -> Result<(), GcError> {
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut fetches = FuturesUnordered::new();
        for r in refs.iter().copied() {
            let permit_sem = semaphore.clone();
            let cs = chunk_store;
            fetches.push(async move {
                let _permit = permit_sem
                    .acquire_owned()
                    .await
                    .expect("PinSet semaphore must not be closed");
                cs.get_manifest(r).await
            });
        }
        while let Some(result) = fetches.next().await {
            for chunk_ref in result?.chunks {
                // The filter that bounds memory: a hash outside this
                // tick's shards can never be tested against.
                if shards[chunk_ref.hash.as_bytes()[0] as usize] {
                    chunks.insert(chunk_ref.hash);
                }
            }
        }
        Ok(())
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
        /// Refs revealed on LATER reads, modelling manifests published
        /// while a collect is in flight. Read `k` (0-indexed) returns
        /// `live` plus `staged[0..k]`, so read 0 sees none of them and
        /// each subsequent read reveals one more.
        staged: Vec<ManifestRef>,
        reads: std::sync::atomic::AtomicUsize,
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
            _: engram_core::types::BindingDisposition,
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
        async fn mark_host_dead_if_lease_expired(
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
        async fn list_session_events_window(
            &self,
            _session_id: SessionId,
            _cursor: engram_core::types::EventCursor,
            _limit: i64,
            _kinds: &[String],
            _tool_names: &[String],
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
            _file_name: Option<&str>,
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
            let k = self
                .reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut out = self.live.clone();
            out.extend(self.staged.iter().take(k).copied());
            Ok(out)
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

    /// A quiet ref set converges after one fetch round.
    #[tokio::test]
    async fn quiet_ref_set_converges_in_one_round() {
        let (store, _dir) = fresh_store();
        let a = seed_manifest(&store, &[b"a", b"b"], ManifestKind::Disk).await;
        let meta = PinSetMockMeta {
            live: vec![a],
            ..Default::default()
        };
        let got = PinSet::collect_converging(&meta, &store, 4, 3)
            .await
            .expect("collect");
        assert!(got.converged, "a quiet ref set must converge");
        assert_eq!(got.rounds, 1, "one fetch round, then a quiet re-read");
        assert_eq!(got.pin_set.len(), 2);
    }

    /// A manifest published BETWEEN the fetch and the re-read is picked
    /// up by the next round. This is the exact race the old
    /// `chunk_generation` bracket existed to catch — now caught directly,
    /// at ref granularity, instead of through a lossy proxy.
    #[tokio::test]
    async fn ref_published_mid_collect_is_not_missed() {
        let (store, _dir) = fresh_store();
        let a = seed_manifest(&store, &[b"a"], ManifestKind::Disk).await;
        let late = seed_manifest(&store, &[b"late"], ManifestKind::Disk).await;
        let meta = PinSetMockMeta {
            live: vec![a],
            staged: vec![late],
            ..Default::default()
        };
        let got = PinSet::collect_converging(&meta, &store, 4, 4)
            .await
            .expect("collect");
        assert!(got.converged);
        assert_eq!(got.rounds, 2, "a second round fetched the late ref");
        assert_eq!(
            got.pin_set.len(),
            2,
            "the mid-collect publish is IN the pin set — missing it is what deletes a live chunk"
        );
    }

    /// A ref set that never goes quiet still returns a USABLE superset:
    /// every ref seen was fetched before the cap. Refusing to drain here
    /// would forfeit reclamation for no safety gain — which is exactly
    /// what the old generation bracket did on 43% of prod sweeps.
    #[tokio::test]
    async fn unconverged_still_holds_every_ref_it_saw() {
        let (store, _dir) = fresh_store();
        let a = seed_manifest(&store, &[b"a"], ManifestKind::Disk).await;
        let mut staged = Vec::new();
        for i in 0..8u8 {
            staged.push(seed_manifest(&store, &[&[b'x', i]], ManifestKind::Disk).await);
        }
        let meta = PinSetMockMeta {
            live: vec![a],
            staged,
            ..Default::default()
        };
        let got = PinSet::collect_converging(&meta, &store, 4, 2)
            .await
            .expect("collect");
        assert!(!got.converged, "a never-quiet ref set hits the cap");
        assert_eq!(got.rounds, 2, "stopped at max_rounds");
        assert!(
            got.pin_set.len() >= 2,
            "every ref fetched before the cap is present — not an empty or partial set"
        );
    }
}
