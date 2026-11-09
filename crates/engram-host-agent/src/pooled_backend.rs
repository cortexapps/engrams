//! `SandboxBackend` wrapper that owns the host-side resources of a
//! sandbox: its session disks, the image cache, the chunk store, the
//! checkpoint chain, and the optional egress proxy. The inner backend
//! (VZ or FC) only attaches the devices this layer provisions.
use engram_core::types::sandbox::SessionDisks;
use engram_core::types::snapshot::{DiskRole, MIGRATION_DISK_BLOCK_BYTES};
use engram_host_core::session_volumes::{PendingCheckpoint, Source};
use engram_host_core::{SessionVolumes, Volume};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{
    AgentSpec, AuxBundleRef, ExecRequest, ExecStream, SandboxSpec, SessionFileMetadata,
    SessionFileSpec, SessionFileStream,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use engram_fill::{Class, FillEngine};
use tokio::fs;
use tracing::Instrument as _;

use crate::capture_lock::{CaptureGate, CaptureUser};
use crate::egress::HostEgress;
use crate::image_cache::{CachedImage, ImageCache};

/// Rebuilds `migration_fetch` items from their frames. Frames of one item
/// arrive in order and back to back; the last carries the item's hash.
/// Each requested item arrives once.
struct ItemAssembler {
    item: Option<u32>,
    bytes: Vec<u8>,
    /// Indexed by item: the item arrived whole.
    arrived: Vec<bool>,
}

impl ItemAssembler {
    fn new(requested: usize) -> Self {
        Self {
            item: None,
            bytes: Vec::new(),
            arrived: vec![false; requested],
        }
    }

    /// Add `frame`; on an item's last frame, return its index and bytes.
    fn push(
        &mut self,
        frame: engram_core::types::snapshot::MigrationFrame,
    ) -> Result<Option<(u32, Vec<u8>)>, SandboxError> {
        match self.arrived.get(frame.item_idx as usize) {
            Some(false) => {}
            Some(true) => {
                return Err(SandboxError::Snapshot(format!(
                    "migration fetch: item {} arrived twice",
                    frame.item_idx
                )))
            }
            None => {
                return Err(SandboxError::Snapshot(format!(
                    "migration fetch: item {} was not requested",
                    frame.item_idx
                )))
            }
        }
        if self.item.is_some_and(|i| i != frame.item_idx) || frame.offset != self.bytes.len() as u64
        {
            return Err(SandboxError::Snapshot(format!(
                "migration fetch: frame of item {} at offset {} out of order",
                frame.item_idx, frame.offset
            )));
        }
        self.item = Some(frame.item_idx);
        self.bytes.extend_from_slice(&frame.data);
        if !frame.last {
            return Ok(None);
        }
        self.item = None;
        let bytes = std::mem::take(&mut self.bytes);
        // A source of the previous release sends no hash; #1772.
        if let Some(want) = frame.item_hash {
            if engram_migrate_proto::wire_hash(&bytes) != want {
                return Err(SandboxError::Snapshot(format!(
                    "migration fetch: item {} does not match its hash",
                    frame.item_idx
                )));
            }
        }
        self.arrived[frame.item_idx as usize] = true;
        Ok(Some((frame.item_idx, bytes)))
    }

    /// At the end of the stream: every requested item arrived whole. A
    /// stream that ends early must not read as a complete fetch.
    fn finish(&self) -> Result<(), SandboxError> {
        if let Some(item) = self.item {
            return Err(SandboxError::Snapshot(format!(
                "migration fetch: the stream ended inside item {item}"
            )));
        }
        let missing = self.arrived.iter().filter(|arrived| !**arrived).count();
        if missing > 0 {
            return Err(SandboxError::Snapshot(format!(
                "migration fetch: the stream ended with {missing} of {} items missing",
                self.arrived.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod item_assembler_tests {
    use engram_core::types::snapshot::MigrationFrame;

    use super::ItemAssembler;

    fn frame(item: u32, offset: u64, data: &'static [u8], last: Option<&[u8]>) -> MigrationFrame {
        MigrationFrame {
            item_idx: item,
            offset,
            data: bytes::Bytes::from_static(data),
            last: last.is_some(),
            item_hash: last.map(engram_migrate_proto::wire_hash),
        }
    }

    #[test]
    fn an_item_is_returned_whole_on_its_last_frame() {
        let mut a = ItemAssembler::new(2);
        assert!(a.push(frame(0, 0, b"ab", None)).unwrap().is_none());
        let (idx, bytes) = a.push(frame(0, 2, b"cd", Some(b"abcd"))).unwrap().unwrap();
        assert_eq!((idx, bytes.as_slice()), (0, &b"abcd"[..]));
        let (idx, _) = a.push(frame(1, 0, b"x", Some(b"x"))).unwrap().unwrap();
        assert_eq!(idx, 1);
        a.finish().unwrap();
    }

    #[test]
    fn a_gap_a_mix_of_items_or_a_bad_hash_is_refused() {
        let mut gap = ItemAssembler::new(1);
        gap.push(frame(0, 0, b"ab", None)).unwrap();
        assert!(gap.push(frame(0, 3, b"d", Some(b"abd"))).is_err());

        let mut mixed = ItemAssembler::new(2);
        mixed.push(frame(0, 0, b"ab", None)).unwrap();
        assert!(mixed.push(frame(1, 2, b"cd", Some(b"abcd"))).is_err());

        let mut bad = ItemAssembler::new(1);
        assert!(bad.push(frame(0, 0, b"ab", Some(b"other"))).is_err());
    }

    #[test]
    fn an_unrequested_or_repeated_item_is_refused() {
        let mut a = ItemAssembler::new(1);
        assert!(a.push(frame(1, 0, b"x", Some(b"x"))).is_err());
        a.push(frame(0, 0, b"x", Some(b"x"))).unwrap();
        assert!(a.push(frame(0, 0, b"x", Some(b"x"))).is_err());
    }

    #[test]
    fn a_stream_that_ends_early_is_refused() {
        let mut half = ItemAssembler::new(1);
        half.push(frame(0, 0, b"ab", None)).unwrap();
        assert!(half.finish().is_err());

        let mut short = ItemAssembler::new(2);
        short.push(frame(0, 0, b"x", Some(b"x"))).unwrap();
        assert!(short.finish().is_err());
    }

    #[test]
    fn an_item_from_a_source_without_hashes_is_accepted() {
        let mut a = ItemAssembler::new(1);
        let mut f = frame(0, 0, b"ab", Some(b"ab"));
        f.item_hash = None;
        assert!(a.push(f).unwrap().is_some());
    }
}

#[cfg(test)]
mod first_touch_tests {
    use super::*;

    /// Records the chunk GETs in order.
    struct RecordingBlob {
        inner: engram_storage_local::LocalBlobStorage,
        gets: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl engram_core::traits::BlobStorage for RecordingBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::storage::ByteStream,
        ) -> Result<u64, engram_core::BlobError> {
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::storage::ByteStream, engram_core::BlobError> {
            if key.starts_with("chunks/") {
                self.gets.lock().push(key.to_string());
            }
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::storage::BlobObjectMeta, engram_core::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, engram_core::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    #[tokio::test]
    async fn a_lazy_restore_fetches_the_prefix_then_the_rest_in_touch_order() {
        const CHUNK: u64 = 4096;
        let tmp = tempfile::tempdir().unwrap();
        let blob = Arc::new(RecordingBlob {
            inner: engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
            gets: parking_lot::Mutex::new(Vec::new()),
        });
        let plain = ChunkStore::new(blob.clone());
        let mut chunks = Vec::new();
        for i in 0..4u8 {
            let hash = plain.put_chunk(&vec![i + 1; CHUNK as usize]).await.unwrap();
            chunks.push(engram_chunk_store::ChunkRef {
                offset: u64::from(i) * CHUNK,
                hash,
            });
        }
        let hashes: Vec<_> = chunks.iter().map(|c| c.hash).collect();
        let manifest = engram_chunk_store::Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: engram_chunk_store::ManifestKind::Memory,
            chunk_size: engram_chunk_store::ChunkSize::bytes(CHUNK),
            total_bytes: 4 * CHUNK,
            chunks,
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mref = engram_core::types::manifest::ManifestRef::new();
        plain.put_manifest(mref, &manifest).await.unwrap();
        // The last run touched chunk 2, then (after its first completed
        // run) chunks 3 and 1.
        let session = SessionId::new();
        let file = tmp.path().join(engram_chunk_store::FirstTouch::FILE);
        let record = engram_chunk_store::FirstTouch {
            entries: vec![(2, 10), (3, 20), (1, 30)],
        };
        std::fs::write(&file, record.encode()).unwrap();
        publish_first_touch(&plain, session, &file, Some(10)).await;

        let cache = engram_chunk_store::cache::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig {
                eviction_enabled: false,
                ..engram_chunk_store::cache::ChunkCacheConfig::new(tmp.path().join("cache"))
            },
        );
        let fill = FillEngine::new(
            ChunkStore::new(blob.clone()).with_chunk_cache(cache),
            engram_fill::Config {
                in_flight: 1,
                max_in_flight: 1,
                ..engram_fill::Config::default()
            },
        );
        blob.gets.lock().clear();
        PooledBackend::prefetch_memory_chunks(&fill, Some(mref), Class::Hydrate, Some(session))
            .await
            .unwrap();
        let order: Vec<String> = [2, 3, 1, 0]
            .iter()
            .map(|&i| hashes[i].storage_key())
            .collect();
        assert_eq!(*blob.gets.lock(), order);
    }

    #[test]
    fn only_the_first_run_started_here_ends_the_prefix() {
        use engram_harness_proto::HarnessEvent;
        let started = |id: &str| HarnessEvent::RunStarted {
            run_id: id.into(),
            prompt_summary: None,
            prompt_id: None,
        };
        let completed = |id: &str| HarnessEvent::RunCompleted {
            run_id: id.into(),
            ok: true,
        };
        // A replayed completion of a run from before the restore.
        assert_eq!(first_run_after(None, &completed("old"), || 1), None);
        let first = first_run_after(None, &started("r1"), || 1).unwrap();
        assert_eq!(first_run_after(Some(&first), &started("r2"), || 2), None);
        assert_eq!(first_run_after(Some(&first), &completed("old"), || 3), None);
        let done = first_run_after(Some(&first), &completed("r1"), || 4).unwrap();
        assert_eq!(done, FirstRun::Completed { at_ms: 4 });
        assert_eq!(first_run_after(Some(&done), &completed("r1"), || 5), None);
    }

    #[test]
    fn the_prefix_ends_at_the_first_completed_run_and_at_the_cap() {
        let record = engram_chunk_store::FirstTouch {
            entries: vec![(4, 10), (1, 20), (7, 30)],
        };
        assert_eq!(split_first_touch(&record, 20), (vec![4, 1], vec![7]));
        assert_eq!(
            split_first_touch(&record, i64::MAX),
            (vec![4, 1, 7], vec![])
        );
        let long = engram_chunk_store::FirstTouch {
            entries: (0..FIRST_TOUCH_PREFIX_CAP as u32 + 2)
                .map(|i| (i, 0))
                .collect(),
        };
        let (prefix, rest) = split_first_touch(&long, 0);
        assert_eq!(prefix.len(), FIRST_TOUCH_PREFIX_CAP);
        assert_eq!(
            rest,
            vec![
                FIRST_TOUCH_PREFIX_CAP as u32,
                FIRST_TOUCH_PREFIX_CAP as u32 + 1
            ]
        );
    }
}

/// The sidecar's `trace_lineage_id`: the session whose traces this
/// snapshot's restores read.
async fn read_trace_lineage(src: &Path) -> Option<SessionId> {
    let bytes = fs::read(src.join("manifest.json")).await.ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    serde_json::from_value(value.get("trace_lineage_id")?.clone()).ok()
}

/// The chunk indices under `key` as this manifest's chunk hashes, in
/// order. Positions the manifest leaves out are zero pages and need no
/// fetch.
async fn first_touch_hashes(
    store: &ChunkStore,
    session: SessionId,
    key: &str,
    manifest: &engram_chunk_store::Manifest,
) -> Vec<engram_chunk_store::ChunkHash> {
    let indices = match store.blob_storage().get(key).await {
        Ok(bytes) => engram_chunk_store::FirstTouch::decode_indices(&bytes).unwrap_or_default(),
        Err(engram_core::BlobError::NotFound) => return Vec::new(),
        Err(e) => {
            tracing::warn!(%session, error = %e, "first-touch prefix unreadable");
            return Vec::new();
        }
    };
    let chunk_size = manifest.chunk_size.as_u64();
    let by_offset: std::collections::HashMap<u64, engram_chunk_store::ChunkHash> =
        manifest.chunks.iter().map(|c| (c.offset, c.hash)).collect();
    indices
        .into_iter()
        .filter_map(|i| by_offset.get(&(u64::from(i) * chunk_size)).copied())
        .collect()
}

/// The most chunks a published first-touch prefix holds: 4 GiB of 512 KiB
/// chunks. A cold resume's first agent turn touched about 3,500 chunks, so
/// the cap keeps more than twice that, and bounds what a restore fetches
/// at `Prefix` ahead of the rest of guest memory.
const FIRST_TOUCH_PREFIX_CAP: usize = 8192;

/// The first agent run that started in a sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FirstRun {
    Started { run_id: String },
    Completed { at_ms: i64 },
}

/// The first run after `event`, or `None` when `event` changes nothing:
/// the first `RunStarted` names the run, and only its `RunCompleted`
/// completes it.
fn first_run_after(
    current: Option<&FirstRun>,
    event: &engram_harness_proto::HarnessEvent,
    now_ms: impl FnOnce() -> i64,
) -> Option<FirstRun> {
    use engram_harness_proto::HarnessEvent;
    match (current, event) {
        (None, HarnessEvent::RunStarted { run_id, .. }) => Some(FirstRun::Started {
            run_id: run_id.clone(),
        }),
        (Some(FirstRun::Started { run_id: first }), HarnessEvent::RunCompleted { run_id, .. })
            if first == run_id =>
        {
            Some(FirstRun::Completed { at_ms: now_ms() })
        }
        _ => None,
    }
}

/// The record's chunk indices in touch order, split into the prefix (the
/// chunks touched up to and including `cut_ms`, at most
/// [`FIRST_TOUCH_PREFIX_CAP`]) and the rest.
fn split_first_touch(record: &engram_chunk_store::FirstTouch, cut_ms: i64) -> (Vec<u32>, Vec<u32>) {
    let mut prefix: Vec<u32> = record.entries.iter().map(|(idx, _)| *idx).collect();
    let cut = record
        .entries
        .iter()
        .take_while(|(_, at)| *at <= cut_ms)
        .count()
        .min(FIRST_TOUCH_PREFIX_CAP);
    let rest = prefix.split_off(cut);
    (prefix, rest)
}

/// Publish the chunks the sandbox touched up to its first completed run
/// (all of them before one completes), for the session's next restore to
/// fetch first. Best-effort.
async fn publish_first_touch(
    store: &ChunkStore,
    session: SessionId,
    file: &Path,
    first_run_completed: Option<i64>,
) {
    let Ok(bytes) = tokio::fs::read(file).await else {
        return;
    };
    let Some(record) = engram_chunk_store::FirstTouch::decode(&bytes) else {
        tracing::debug!(file = %file.display(), "first-touch record torn; not published");
        return;
    };
    if record.entries.is_empty() {
        return;
    }
    let (prefix, rest) = split_first_touch(&record, first_run_completed.unwrap_or(i64::MAX));
    let encode = engram_chunk_store::FirstTouch::encode_indices;
    let blob = store.blob_storage();
    let key = engram_chunk_store::FirstTouch::key(session.as_uuid());
    let rest_key = engram_chunk_store::FirstTouch::rest_key(session.as_uuid());
    match tokio::try_join!(
        blob.put(&key, encode(&prefix).into()),
        blob.put(&rest_key, encode(&rest).into()),
    ) {
        Ok(_) => tracing::info!(
            %session,
            chunks = prefix.len(),
            rest = rest.len(),
            "published first-touch prefix"
        ),
        Err(e) => tracing::warn!(%session, error = %e, "publish first-touch prefix failed"),
    }
}

/// Return the cached image's legacy `rootfs.ext4` path or a
/// typed error when neither it nor a bundle is present.
///
/// ADR 0007 Phase 6: chunked-storage OCI pushes ship only
/// `bundle.json`; the disk is resolved through the chunk store.
/// Callers hit this helper on the fallback paths (no chunk_store
/// wired, or no bundle on the image) — at that point we *need* a
/// concrete file, and the absence of both rootfs.ext4 AND bundle
/// is a config error worth surfacing loudly rather than papering
/// over.
fn legacy_rootfs_path(cached: &CachedImage) -> Result<PathBuf, SandboxError> {
    cached.rootfs_path.clone().ok_or_else(|| {
        SandboxError::InvalidSpec(format!(
            "image {} has neither rootfs.ext4 nor a chunk-store path resolvable here \
             (host missing chunk_store/materialize_dir wiring?)",
            cached.digest,
        ))
    })
}

fn disk_err(e: std::io::Error) -> SandboxError {
    SandboxError::Vm(format!("session disk: {e}").into())
}

/// What the snapshot's sidecar says about swap.
struct SidecarSwap {
    swap_mib: u64,
    /// The captured guest was a base capture: its swap was never a
    /// session's state, so a fresh device is the right one.
    base_capture: bool,
}

async fn read_sidecar_swap(src: &Path) -> Result<SidecarSwap, SandboxError> {
    let bytes = fs::read(src.join("manifest.json"))
        .await
        .map_err(|e| SandboxError::Snapshot(format!("read snapshot sidecar: {e}")))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| SandboxError::Snapshot(format!("parse snapshot sidecar: {e}")))?;
    let spec = value
        .get("spec")
        .ok_or_else(|| SandboxError::Snapshot("sidecar spec is absent".into()))?;
    Ok(SidecarSwap {
        swap_mib: spec.get("swap_mib").and_then(|v| v.as_u64()).unwrap_or(0),
        base_capture: spec
            .get("env")
            .and_then(|v| v.get("ENGRAM_BASE_CAPTURE"))
            .and_then(|v| v.as_str())
            == Some("1"),
    })
}

/// ADR 0045 C2 (destination): a staged post-copy restore awaiting its
/// fetch poller spawn (keyed by the restore metadata's snapshot id;
/// consumed inside `restore_with` once the session volumes exist). The
/// fields drive the Linux-only poller; non-Linux builds never
/// construct one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone)]
struct PostCopyDestPending {
    hot_trace_file: String,
    source_addr: String,
    export_id: String,
}

/// A minted-but-not-yet-captured post-copy export. The capture consumes it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone)]
struct PendingPresetup {
    export_id: String,
    peer_token: String,
    /// The chain ref the presetup advertised as the session manifest
    /// (the capture validates the chain has not moved underneath).
    chain_ref: engram_core::types::manifest::ManifestRef,
    /// The disk checkpoints the presetup advertised; the capture's
    /// pending checkpoints must be changes on exactly these.
    disk_manifest_ref: Option<engram_core::types::manifest::ManifestRef>,
    swap_manifest_ref: Option<engram_core::types::manifest::ManifestRef>,
    /// A snapshot of each disk, taken while the guest runs, and the
    /// blocks it changed since the advertised checkpoint. The dest pulls
    /// them before the pause; the capture seals only what changed since.
    precopy: std::collections::BTreeMap<DiskRole, Arc<DiskSeal>>,
    sidecar_json: Vec<u8>,
    idle_marked: bool,
    hot_chunks: Vec<[u8; 32]>,
    /// What the destination's fetch poller has asked for. It asks for
    /// the precopy once its disks are provisioned, and for `state.bin`
    /// once the precopy has landed, so the capture pauses the guest
    /// only then.
    dest: Arc<DestProgress>,
}

/// Two one-way latches the capture waits on.
#[derive(Default)]
struct DestProgress {
    contacted: Latch,
    ready: Latch,
}

#[derive(Default)]
struct Latch {
    set: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl Latch {
    fn set(&self) {
        if !self.set.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    /// `true` when the latch is set within `bound`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn wait(&self, bound: std::time::Duration) -> bool {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.set.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        tokio::time::timeout(bound, notified).await.is_ok()
    }
}

/// How long a live capture waits for a destination to ask for anything,
/// and then for it to ask for `state.bin`, before it pauses the guest
/// anyway. A destination that never gets ready fails its restore; the
/// coordinator then aborts to the source.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DEST_CONTACT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DEST_READY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The unwind guard for a capture's window between the guest `pause`
/// and the moment a finisher, a finalize record, or an export owns the
/// captured state. A `?` return or a dropped handler future inside that
/// window runs no error arm, so `Drop` does the recovery: discard the
/// pending disk checkpoints, re-insert a consumed presetup so a
/// coordinator retry finds it, and resume the guest when it was paused.
struct CaptureUnwind {
    inner: Arc<dyn SandboxBackend>,
    volumes: Option<Arc<SessionVolumes>>,
    id: SandboxId,
    /// Disk snapshots taken under the pause that nothing owns yet.
    pending: Vec<PendingCheckpoint>,
    /// Post-copy seals taken under the pause that no export owns yet.
    disk_seal: Vec<Arc<DiskSeal>>,
    #[allow(clippy::type_complexity)]
    presetup_restore: Option<(
        SandboxId,
        PendingPresetup,
        Arc<DashMap<SandboxId, PendingPresetup>>,
    )>,
    defused: bool,
    resume_on_drop: bool,
}

impl CaptureUnwind {
    fn new(backend: &PooledBackend, id: SandboxId) -> Self {
        Self {
            inner: backend.inner.clone(),
            volumes: backend.volumes.clone(),
            id,
            pending: Vec::new(),
            disk_seal: Vec::new(),
            presetup_restore: None,
            defused: true,
            resume_on_drop: true,
        }
    }

    /// The pause has happened; from here `Drop` recovers unless
    /// `defuse` is called.
    fn arm(&mut self) {
        self.defused = false;
    }

    /// Something durable owns the captured state; `Drop` is a no-op.
    fn defuse(&mut self) {
        self.defused = true;
    }
}

impl Drop for CaptureUnwind {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        let inner = self.inner.clone();
        let volumes = self.volumes.clone();
        let resume_on_drop = self.resume_on_drop;
        let id = self.id;
        let pending = std::mem::take(&mut self.pending);
        let mut disk_seal = std::mem::take(&mut self.disk_seal);
        if let Some((pid, presetup, map)) = self.presetup_restore.take() {
            // A newer presetup wins (the same policy as `migration_presetup`);
            // the superseded one's precopy goes with it.
            match map.entry(pid) {
                dashmap::mapref::entry::Entry::Vacant(slot) => {
                    slot.insert(presetup);
                }
                dashmap::mapref::entry::Entry::Occupied(_) => {
                    disk_seal.extend(presetup.precopy.into_values());
                }
            }
        }
        tracing::warn!(
            sandbox_id = %id,
            resume = resume_on_drop,
            "capture unwind: capture failed or was cancelled; \
             discarding the pending disk checkpoints and restoring the presetup",
        );
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                sandbox_id = %id,
                "capture unwind: no tokio runtime in Drop; the next startup reconcile \
                 removes the pending checkpoints",
            );
            return;
        };
        handle.spawn(async move {
            if let Some(volumes) = volumes.as_ref() {
                for pending in pending {
                    discard_pending(volumes, pending).await;
                }
                for seal in disk_seal {
                    discard_seal(volumes, seal).await;
                }
            }
            if !resume_on_drop {
                return;
            }
            match inner.resume(id).await {
                Ok(()) => tracing::info!(sandbox_id = %id, "capture unwind: guest resumed in place"),
                Err(e) => tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "capture unwind: best-effort resume failed (guest may stay paused until destroy)",
                ),
            }
        });
    }
}

/// A snapshot of one disk a live move serves blocks from: the
/// presetup's, taken while the guest runs, or the capture's, taken
/// under the pause. `sealed` lists the blocks of
/// [`MIGRATION_DISK_BLOCK_BYTES`] that changed since the snapshot
/// before it: the advertised checkpoint for a precopy, the precopy for
/// a seal.
pub struct DiskSeal {
    pub(crate) pending: PendingCheckpoint,
    pub(crate) sealed: std::collections::BTreeSet<u64>,
}

impl DiskSeal {
    fn info(&self) -> engram_core::types::snapshot::DiskSealInfo {
        engram_core::types::snapshot::DiskSealInfo {
            block_indices: self.sealed.iter().copied().collect(),
        }
    }

    /// Block `index` of the snapshot; short when it is the last one.
    async fn read_block(&self, index: u64) -> std::io::Result<Vec<u8>> {
        let volume = self.pending.snapshot();
        let offset = index * MIGRATION_DISK_BLOCK_BYTES;
        let len = MIGRATION_DISK_BLOCK_BYTES.min(volume.size_bytes.saturating_sub(offset));
        volume.read_range(offset, len).await
    }
}

pub(crate) async fn discard_pending(volumes: &SessionVolumes, pending: PendingCheckpoint) {
    let (sandbox, role) = (pending.sandbox(), pending.role());
    if let Err(error) = volumes.discard(pending).await {
        tracing::warn!(%sandbox, %role, %error, "discard of a pending disk checkpoint failed");
    }
}

/// The seal's snapshot is deleted when this holder is the last one; an
/// export still serving from it keeps it until commit or abort.
async fn discard_seal(volumes: &SessionVolumes, seal: Arc<DiskSeal>) {
    match Arc::try_unwrap(seal) {
        Ok(seal) => discard_pending(volumes, seal.pending).await,
        // A fetch stream still reads the snapshot; the release of the
        // sandbox's volumes removes it.
        Err(seal) => tracing::debug!(
            sandbox_id = %seal.pending.sandbox(),
            role = %seal.pending.role(),
            "disk seal still in use at discard; left for the sandbox's release",
        ),
    }
}

/// Wraps an inner [`SandboxBackend`] (FC or VZ) with the host-side
/// resources of a sandbox. `create` provisions the session disks and
/// resolves the image; captures checkpoint the disks and chunk the
/// memory; `destroy` releases everything.
pub struct PooledBackend {
    teardowns: engram_core::teardown::TeardownRegistry<SandboxId>,
    inner: Arc<dyn SandboxBackend>,
    /// The session disks of every sandbox on this host. `None` on a
    /// host without a session disk: the inner backend opens the image's
    /// flat rootfs file and nothing checkpoints the disk.
    volumes: Option<Arc<SessionVolumes>>,
    /// Resolves `spec.image_uri` before `create` delegates to `inner`.
    image_cache: Option<ImageCache>,
    /// ADR 0006: the per-host egress proxy. `None` is the no-filtering
    /// path (dev or operator opt-out).
    egress: Option<Arc<HostEgress>>,
    /// `sandbox_id → session_id`, the host-agent's canonical binding map.
    session_bindings: Arc<DashMap<SandboxId, SessionId>>,
    /// Guests whose control plane stopped answering (ADR 0091).
    /// Advertised in every heartbeat until a successful capture or
    /// destroy clears them.
    unreachable_guests: Arc<DashMap<SandboxId, SessionId>>,
    /// Sandboxes with a dead-guest confirmation probe in flight: at
    /// most one probe per sandbox regardless of the driver's tick rate.
    dead_probe_inflight: Arc<DashMap<SandboxId, ()>>,
    /// The chunk store that holds disk and memory manifests. `None`
    /// disables chunked snapshots. Always `fill.store()` when set.
    chunk_store: Option<ChunkStore>,
    /// The host's chunk fill engine, over `chunk_store`.
    fill: Option<FillEngine>,
    /// Peer health for the resume peer-fill arm (ADR 0095): a lost peer
    /// is skipped across every resume on this host for the lost window.
    peer_health: std::sync::Arc<crate::peer_fill::PeerHealth>,
    /// The staged-bundle dir, copied from `inner.bundle_dir()` so this
    /// layer materializes generations into the dir the backend reads.
    bundle_dir: PathBuf,
    /// NVMe-backed LRU cache fronting the chunk store for memory chunks.
    chunk_cache: Option<ChunkCache>,
    /// Scratch root for the `MaterializeImage` RPC (ADR 0080 §C).
    /// `None` = this host cannot materialize images.
    materialize_scratch: Option<PathBuf>,
    /// The one-materialize-per-host gate; a `try_lock` miss is the
    /// retryable `Busy` failure.
    materialize_gate: Arc<tokio::sync::Mutex<()>>,
    /// The OCI client `materialize_image` pulls with.
    oci_client: Option<engram_oci::OciClient>,
    /// The snapshot each sandbox produced but has not committed or
    /// aborted, so a retry overwrites in place instead of leaking a
    /// snapshot dir per attempt.
    inflight_snapshots: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    /// The metadata of the snapshot a held source produced, for the
    /// coordinator's durable finalize after the hold.
    held_snapshots: DashMap<SandboxId, SnapshotMetadata>,
    /// Unix ms at which each sandbox's memory last matched a durable
    /// manifest: its last successful capture, or the chain seed of its
    /// restore. A sandbox without a stamp is due for a checkpoint.
    last_snapshot_unix_ms: Arc<DashMap<SandboxId, i64>>,
    /// Each sandbox's first agent run. The first-touch prefix a capture
    /// publishes ends when it completes.
    first_runs: Arc<DashMap<SandboxId, FirstRun>>,
    /// A restored sandbox's hinted peer, registered with the fill engine
    /// for its memory manifest until the sandbox goes.
    peer_registrations: Arc<DashMap<SandboxId, engram_fill::PeerRegistration>>,
    /// A lazily restored sandbox's registration with the fill engine for
    /// its memory manifest, until the sandbox goes: its faults and its
    /// disk reads share one `Demand` cap, also after the memory prefetch
    /// ends.
    sandbox_registrations: Arc<DashMap<SandboxId, engram_fill::SandboxRegistration>>,
    /// A lazy restore's background memory prefetch; dropping the entry
    /// cancels it.
    memory_prefetches: DashMap<SandboxId, AbortTaskOnDrop>,
    /// The adaptive checkpoint controller's input: what the last
    /// capture observed about the memory dirty rate (ADR 0101 B).
    checkpoint_pacing: Arc<DashMap<SandboxId, EpochPacingSample>>,
    /// Root for checkpoint state: `records/` (durable per-checkpoint
    /// records awaiting coordinator ack) and `finalize/`. `None`
    /// disables checkpoint chains: `snapshot()` stays pure-Full.
    checkpoint_dir: Option<PathBuf>,
    /// Signalled after each checkpoint record lands in `records/`, so the
    /// heartbeat advertises it at once instead of on the next period.
    checkpoint_recorded: Arc<tokio::sync::Notify>,
    /// Durable mirror of the chain heads for survivor rehydrate after a
    /// pod roll. `Some` iff `checkpoint_dir` is `Some`.
    chain_heads: Option<Arc<crate::checkpoint::ChainHeadStore>>,
    /// Per-sandbox rolling chain state. Present once the first (Full)
    /// checkpoint completed; every later capture is a diff against it.
    checkpoint_chains: Arc<DashMap<SandboxId, crate::checkpoint::CheckpointChain>>,
    capture_gates: Arc<DashMap<SandboxId, Arc<CaptureGate>>>,
    /// `sandbox_id → snapshot_id` of an in-flight, durably persisted
    /// eviction finalize job (issue #529). Makes `snapshot_begin`
    /// idempotent under an eviction-scanner retry storm, and keeps
    /// `destroy` from releasing the disks the job still publishes.
    pending_finalizes: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    /// A weak self-reference, set once via `set_self_ref`, so a
    /// detached finalize job reaches the full `destroy`.
    self_ref: Arc<std::sync::OnceLock<std::sync::Weak<PooledBackend>>>,
    /// Open live-migration exports (frozen sandboxes serving a move).
    migrations: Arc<crate::migration::MigrationRegistry>,
    /// The post-copy page server, when the host runs one.
    migrate_peer: Arc<std::sync::OnceLock<Arc<crate::migrate_peer::PeerServer>>>,
    /// Presetups awaiting their capture.
    pending_presetups: Arc<DashMap<SandboxId, PendingPresetup>>,
    /// Staged post-copy restores awaiting their fetch poller.
    postcopy_dests: Arc<DashMap<engram_core::types::SnapshotId, PostCopyDestPending>>,
    /// Sandboxes playing a post-copy role; both roles fence the normal
    /// lifecycle drivers. Mirrored into the FC sandbox manifest for
    /// reattach.
    migration_roles: Arc<DashMap<SandboxId, crate::migration::MigrationRole>>,
    /// Destinations whose post-copy drain finished, recorded before
    /// the persisted role is cleared so a failed manifest write is
    /// retried by the next call.
    drained_dests: Arc<DashMap<SandboxId, engram_core::types::snapshot::DrainOutcome>>,
    /// Optional hint polling stops as soon as the memory drain terminates.
    hint_pollers: Arc<DashMap<SandboxId, AbortTaskOnDrop>>,
    /// The SIGTERM capture quiesce: raised once, never lowered.
    /// `capture_phase` refuses under the capture lock before anything is
    /// consumed, so a capture never walks into the runtime teardown.
    captures_quiesced: Arc<std::sync::atomic::AtomicBool>,
    /// ADR 0098 D1: the wall clock is an injected world input.
    clock: Arc<dyn engram_core::traits::Clock>,
    /// ADR 0098 P5: the durable-fs seam the records go through.
    host_fs: Arc<dyn engram_host_core::HostFs>,
}

/// The finalize job's teardown seam: upgrades the weak `PooledBackend`
/// ref at call time so the job reaches the full `destroy`. A gone
/// backend (process shutting down) is success: the teardown reconcile
/// backstops the sandbox itself.
pub(crate) struct PooledDestroyer {
    pub(crate) self_ref: Arc<std::sync::OnceLock<std::sync::Weak<PooledBackend>>>,
}

#[async_trait]
impl crate::eviction_finalize::EvictionSandbox for PooledDestroyer {
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let Some(pooled) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(());
        };
        use engram_core::traits::SandboxBackend as _;
        pooled.destroy(id).await
    }
}

/// Human-readable message for a [`crate::warm_progress::WarmViolation`] —
/// shared by `run_warm_hook`'s three watchdog-triggered failure sites.
fn warm_violation_message(v: crate::warm_progress::WarmViolation) -> String {
    use crate::warm_progress::WarmViolation;
    match v {
        WarmViolation::Stall => format!(
            "[warm] hook went silent (no output or progress line) for at least the \
             stall budget (ENGRAM_WARM_STALL_SECS, default {}s)",
            crate::warm_progress::DEFAULT_STALL_SECS,
        ),
        WarmViolation::StageDeadline => {
            "[warm] hook stage exceeded its declared deadline_secs".into()
        }
        WarmViolation::GlobalTimeout => "[warm] hook exceeded its global WarmConfig timeout".into(),
    }
}

/// Pull the free-text `msg=`/heartbeat detail (if any) out of a parsed
/// progress line, for the `CaptureProgress.detail` field.
fn progress_line_detail(line: &crate::warm_progress::WarmProgressLine) -> Option<String> {
    use crate::warm_progress::WarmEvent;
    match &line.event {
        WarmEvent::Start { msg, .. } | WarmEvent::Heartbeat { msg, .. } => msg.clone(),
        WarmEvent::Done { .. } => None,
    }
}

/// Record `engram_warm_hook_stage_seconds` for every CLOSED stage in a
/// `[warm]`-hook stage history (the still-open stage, if any — `outcome:
/// Running` — has no duration to record).
/// ADR 0088 addendum (adversarial-review fix): the pre-seed balloon
/// inflate with rollback-safe cleanup. Returns:
///
/// - `Ok(true)` — the inflate PATCH landed (whatever the guest granted,
///   including 0 MiB): the caller OWES a confirmed `balloon_release`
///   before any guest workload runs.
/// - `Ok(false)` — no balloon in play (target 0, the TYPED no-device
///   `InvalidSpec`, or a failed reclaim that was successfully
///   normalized back to deflated): dense seed, nothing to release.
/// - `Err` — the reclaim failed AND the balloon could not be confirmed
///   deflated: the capture must fail rather than run a warm hook in a
///   possibly-starved guest.
///
/// The reclaim's `InvalidSpec` is the only fail-open path; every other
/// reclaim error is treated as "the inflate target may have landed"
/// (it is PATCHed before the first statistics poll) and normalized via
/// release-and-confirm.
async fn balloon_inflate_for_seed(
    inner: &dyn SandboxBackend,
    id: SandboxId,
    target_mib: u64,
    deadline: std::time::Duration,
) -> Result<bool, SandboxError> {
    if target_mib == 0 {
        return Ok(false);
    }
    match inner.balloon_reclaim(id, target_mib, deadline).await {
        // Even a 0-MiB grant means the target PATCH landed — release.
        Ok(_granted_mib) => Ok(true),
        Err(SandboxError::InvalidSpec(msg)) => {
            tracing::info!(
                sandbox_id = %id,
                detail = %msg,
                "no balloon available; taking a dense cold-base seed",
            );
            Ok(false)
        }
        Err(reclaim_err) => {
            tracing::warn!(
                sandbox_id = %id,
                error = %reclaim_err,
                "balloon reclaim failed after the inflate may have landed; normalizing via release",
            );
            match inner.balloon_release(id).await {
                // Confirmed deflated: safe to proceed with a dense seed.
                Ok(()) => Ok(false),
                // No device ⇒ the inflate PATCH never landed either.
                Err(SandboxError::InvalidSpec(_)) => Ok(false),
                Err(release_err) => Err(SandboxError::Snapshot(format!(
                    "balloon reclaim failed ({reclaim_err}) and the normalizing release also \
                     failed ({release_err}); balloon state unknown"
                ))),
            }
        }
    }
}

fn record_warm_stage_metrics(stages: &[engram_core::types::WarmStageRecord]) {
    use engram_core::types::WarmStageOutcome;
    for stage in stages {
        let Some(ended_at) = stage.ended_at else {
            continue;
        };
        let outcome = match stage.outcome {
            WarmStageOutcome::Done => "done",
            WarmStageOutcome::Failed => "failed",
            WarmStageOutcome::Running => continue,
        };
        let secs = (ended_at - stage.started_at).num_milliseconds().max(0) as f64 / 1000.0;
        metrics::histogram!(
            crate::metrics::WARM_HOOK_STAGE_SECONDS,
            "stage" => stage.name.clone(),
            "outcome" => outcome,
        )
        .record(secs);
    }
}

/// Record `engram_warm_hook_failures_total{kind}` for a capture failure.
fn record_warm_hook_failure_metric(kind: engram_core::types::CaptureFailureKind) {
    metrics::counter!(crate::metrics::WARM_HOOK_FAILURES_TOTAL, "kind" => kind.as_str())
        .increment(1);
}

/// Abort the wrapped task on drop, including hint polling after DrainDone.
/// A leg wrapped by
/// [`spawn_leg_keepalive`] stops resending stale progress the moment
/// the guard goes out of scope — on every path, including an early
/// `?`-return, since `Drop` runs during unwind too.
struct AbortTaskOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Observe the destination's drain outcome locally, independently of the
/// coordinator's drain-wait RPC, stop the optional hot-hint poller at
/// that outcome, then run `then` (the post-copy finish). Destroy or a
/// failed restore drops the returned task, which cancels the poller, the
/// subscription, and `then`.
fn observe_post_copy_drain<F, Fut>(
    sock: PathBuf,
    poller: Option<AbortTaskOnDrop>,
    then: F,
) -> AbortTaskOnDrop
where
    F: FnOnce(engram_core::types::snapshot::DrainOutcome) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    AbortTaskOnDrop(tokio::spawn(async move {
        let observe = crate::postcopy_finish::observe_drain_until_reported(&sock);
        tokio::pin!(observe);
        let outcome = match poller {
            Some(mut poller) => tokio::select! {
                biased;
                outcome = &mut observe => outcome,
                _ = &mut poller.0 => observe.await,
            },
            None => observe.await,
        };
        then(outcome).await;
    }))
}

/// Retry only while the advisory budget remains. The owner cancels this future
/// at the local memory drain outcome, including during a fetch or backoff sleep.
async fn poll_hot_hint<F, Fut>(budget: std::time::Duration, mut fetch: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut delay = std::time::Duration::from_millis(100);
    let poll = async {
        loop {
            if fetch().await {
                return;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(2));
        }
    };
    if tokio::time::timeout(budget, poll).await.is_err() {
        tracing::info!("post-copy hot trace fetch timed out; drain continues");
    }
}

/// Issue #539 finding 1: `run_warm_hook`'s own >=30s keepalive only
/// covers the WARM phase. The BOOT leg (cold boot + chunk materialize to
/// agentd-ready) and the SNAPSHOT leg (pause/flush/chunk memory + upload
/// a multi-GB state blob to BlobStorage) can each run many minutes with
/// no `[warm]`-hook progress traffic to drive a keepalive — and the
/// coordinator's fenced `enable_jobs` write IS the capture-claim lease
/// renewal (`enable_scanner.rs` deleted the separate blind ticker on
/// exactly this premise). Silence past `lease_secs` during either leg
/// lets a peer coordinator re-claim and spawn a SECOND concurrent
/// capture — the regression the deleted ticker existed to prevent.
///
/// Wrap a leg that has no progress source of its own in this: it
/// resends `event` unchanged every
/// [`capture_keepalive_secs_from_env`][crate::warm_progress::capture_keepalive_secs_from_env]
/// (30s in production; test-shrinkable) until the returned guard is
/// dropped.
fn spawn_leg_keepalive(
    progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    event: engram_core::types::CaptureProgress,
) -> AbortTaskOnDrop {
    AbortTaskOnDrop(tokio::spawn(async move {
        let mut interval =
            engram_core::periodic(crate::warm_progress::capture_keepalive_secs_from_env());
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let _ = progress.try_send(event.clone());
        }
    }))
}

/// Publish the optional hint without changing the required disk seal.
#[cfg(any(target_os = "linux", test))]
fn publish_hot_hint(dir: &Path, hot: &[[u8; 32]]) -> std::io::Result<()> {
    let tmp = dir.join("hot-hint.json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&serde_json::json!({"hot_chunks": hot}))?,
    )?;
    std::fs::rename(tmp, dir.join("hot-hint.json"))
}

/// Parse the optional hint independently of the required disk seal.
fn parse_hot_hint(bytes: &[u8]) -> Option<Vec<[u8; 32]>> {
    let result = (|| {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        value
            .get("hot_chunks")
            .map(|hot| serde_json::from_value(hot.clone()))
            .transpose()
    })();
    match result {
        Ok(hot) => hot,
        Err(error) => {
            tracing::info!(%error, "post-copy hint malformed; drain continues");
            Some(Vec::new())
        }
    }
}

/// Atomically stage an advisory hint after state.bin has opened the load gate.
fn stage_hot_trace(path: &Path, hot: &[[u8; 32]]) -> std::io::Result<()> {
    let mut trace =
        engram_chunk_store::working_set::WorkingSetTrace::new(0, 0, chrono::DateTime::UNIX_EPOCH);
    trace.chunks = hot
        .iter()
        .map(|h| engram_chunk_store::manifest::ChunkHash::from_bytes(*h))
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&trace)?)?;
    std::fs::rename(tmp, path)
}

impl PooledBackend {
    /// Compose the pre-pause export. The dest stages each disk from the
    /// checkpoint the precopy is a change on.
    async fn prepare_migration_export(
        &self,
        id: SandboxId,
        chain_ref: engram_core::types::manifest::ManifestRef,
        chain_manifest: &engram_chunk_store::Manifest,
        peer_port: u16,
        precopy: std::collections::BTreeMap<DiskRole, Arc<DiskSeal>>,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        let advertised = |role| precopy.get(&role).map(|seal| seal.pending.previous());
        let (disk_manifest_ref, swap_manifest_ref) =
            (advertised(DiskRole::Root), advertised(DiskRole::Swap));
        let sidecar_json = self.inner.compose_live_sidecar(id, Some(chain_ref))?;
        let memory_manifest_json = serde_json::to_vec(&chain_manifest)
            .map_err(|e| SandboxError::Snapshot(format!("chain manifest json: {e}")))?;
        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        let peer_token = crate::migration::MigrationRegistry::mint_export_id();
        let pending = PendingPresetup {
            export_id: export_id.clone(),
            peer_token: peer_token.clone(),
            chain_ref,
            disk_manifest_ref,
            swap_manifest_ref,
            precopy,
            sidecar_json: sidecar_json.clone(),
            idle_marked: false,
            hot_chunks: self.read_hot_chunks(id),
            dest: Arc::default(),
        };
        self.pending_presetups.insert(id, pending.clone());
        Ok(engram_core::types::snapshot::MigrationPresetupOut {
            export_id,
            peer_token,
            peer_port,
            sidecar_json,
            memory_manifest_json,
            memory_manifest_ref: chain_ref,
            disk_manifest_ref,
            swap_manifest_ref,
            hot_chunks: pending.hot_chunks,
        })
    }

    /// Discard the sealed disk checkpoints and resume the source in place.
    async fn abort_migration_export(
        &self,
        export: crate::migration::MigrationExport,
    ) -> Result<(), SandboxError> {
        // The export is consumed: from here the cleanup runs to the end
        // even if this request is cancelled, or a retry would see
        // NotFound with the guest still paused. A detached task owns it.
        let inner = self.inner.clone();
        let volumes = self.volumes.clone();
        let migration_roles = self.migration_roles.clone();
        let id = export.sandbox_id;
        let export_id = export.export_id.clone();
        tokio::spawn(async move {
            if let Some(volumes) = volumes.as_ref() {
                for (_, seal) in export.disk_seal.into_iter().chain(export.precopy) {
                    discard_seal(volumes, seal).await;
                }
            }
            let _ = fs::remove_dir_all(&export.snapshot_dir).await;
            drop(export.capture_guard);
            inner
                .resume(id)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("migration abort resume: {e}")))?;
            // The guest runs again whether or not this manifest write
            // succeeds (`resume` retries the clear). In-memory first.
            migration_roles.remove(&id);
            inner.set_manifest_migration_role(id, None).await?;
            tracing::info!(sandbox_id = %id, export_id, "migration aborted; guest resumed in place");
            Ok(())
        })
        .await
        .map_err(|e| SandboxError::Snapshot(format!("migration abort task: {e}")))?
    }

    /// ADR 0019 / telemetry restoration (#526): read the uffd-handler's
    /// per-jail prefault-effectiveness snapshot and emit the
    /// `engram_resume_prefault_*` counters. Detached into a bounded,
    /// backgrounded poll (spawned, non-blocking — restore must never wait
    /// on it): `prefault_from_trace` runs on the handler's dedicated
    /// background thread and only writes the file at the END of that
    /// fetch, so a synchronous read raced the write and chronically
    /// misclassified healthy replayed resumes as `stats_missing`. A file
    /// that still isn't there after `PREFAULT_STATS_POLL_TIMEOUT` really
    /// does mean the handler died or the wiring didn't fire — the alarm
    /// the counter exists for. Outcome lands as attributes on a dedicated
    /// `resume.prefault_stats` span carrying `sandbox_id` — detached work
    /// gets its own span, correlated by attribute.
    fn spawn_prefault_stats_probe(&self, id: SandboxId) {
        let Some(stats_path) = self.prefault_stats_path(id) else {
            return;
        };
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let stats_bytes = read_prefault_stats_with_retry(&stats_path).await;
                let outcome = classify_prefault_outcome(stats_bytes.as_deref());
                let span = tracing::Span::current();
                span.record("outcome", outcome.label());
                if let PrefaultOutcome::Replayed { installed, skipped } = outcome {
                    span.record("installed", installed as u64);
                    span.record("skipped", skipped as u64);
                }
                // The peer-fill snapshot (post-copy migration destinations
                // only) — span attributes only, not a counter; only
                // recorded when nonzero so the common non-peer resume
                // doesn't carry four always-0 fields.
                if let Some(peer) = parse_peer_fill_snapshot(stats_bytes.as_deref())
                    .filter(PeerFillSnapshot::is_nonzero)
                {
                    span.record("peer_pulled", peer.peer_pulled);
                    span.record("peer_alt_sourced", peer.peer_alt_sourced);
                    span.record("peer_zero_chunks", peer.peer_zero_chunks);
                    span.record("peer_live_faults", peer.peer_live_faults);
                }
                tracing::info!(outcome = outcome.label(), "resume prefault effectiveness");
                emit_prefault_metrics(outcome);
            },
            tracing::info_span!(
                "resume.prefault_stats",
                sandbox_id = %id,
                outcome = tracing::field::Empty,
                installed = tracing::field::Empty,
                skipped = tracing::field::Empty,
                peer_pulled = tracing::field::Empty,
                peer_alt_sourced = tracing::field::Empty,
                peer_zero_chunks = tracing::field::Empty,
                peer_live_faults = tracing::field::Empty,
            ),
        ));
    }

    /// Capture-time egress (ADR 0080, wire v13): the capture VM gets a tap +
    /// guest IP like any sandbox, but no egress policy is registered for it,
    /// so the proxy denies its traffic as `UnknownGuest`. When the coordinator
    /// shipped a capture policy (assembled from the config's `warm.network` —
    /// the posture decision is coordinator-side now; this host never derives
    /// egress from `WarmConfig` itself), stamp the sandbox-dependent identity
    /// (this `sandbox_id` + the VM's guest IP) onto it and register it for the
    /// duration of the capture. Returns the policy's synthetic [`SessionId`],
    /// which the caller MUST pass to [`Self::unregister_capture_egress`] after
    /// teardown. No-op (returns `None`) when no policy was shipped, no local
    /// proxy is wired, or the guest has no IP — the capture then stays
    /// egress-less.
    async fn register_capture_egress(
        &self,
        id: SandboxId,
        policy: Option<SessionEgressPolicy>,
    ) -> Option<SessionId> {
        // No coordinator-granted egress → egress-less (the common case);
        // skip the guest-IP lookup entirely.
        let mut policy = policy?;
        let egress = self.egress.as_ref()?;
        let Some(guest_ip) = self
            .inner
            .guest_endpoints(id)
            .await
            .map(|ep| ep.egress_identity)
        else {
            tracing::warn!(
                sandbox_id = %id,
                "capture egress: capture VM has no guest IP; [warm] hook runs egress-less",
            );
            return None;
        };
        // The coordinator assembled the posture half; the sandbox-dependent
        // identity half only exists here (the VM was created inside this
        // call), so stamp it now.
        policy.sandbox_id = id;
        policy.guest_ip = guest_ip;
        let session_id = policy.session_id;
        let allow_all = policy.allow_all;
        let allow_hosts = policy.network_allow_hosts.clone();
        let allow_host_patterns = policy.network_allow_host_patterns.clone();
        match egress.apply_policy(policy).await {
            Ok(()) => {
                tracing::info!(
                    sandbox_id = %id,
                    %guest_ip,
                    allow_all,
                    ?allow_hosts,
                    ?allow_host_patterns,
                    "capture egress: registered policy for the [warm] hook",
                );
                Some(session_id)
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "capture egress: policy apply failed; [warm] hook runs egress-less",
                );
                None
            }
        }
    }

    /// Tear down a capture-egress registration made by
    /// [`Self::register_capture_egress`]. Safe to call with `None`.
    fn unregister_capture_egress(&self, session_id: Option<SessionId>) {
        let (Some(sid), Some(egress)) = (session_id, self.egress.as_ref()) else {
            return;
        };
        egress.unregister_session(sid);
        tracing::debug!(
            session_id = %sid,
            "capture egress: unregistered policy after capture teardown",
        );
    }

    /// Run `sync` in the guest — the base-capture quiesce (see the call
    /// site in `build_base_snapshot`). Bounded: sync of a warm image's
    /// dirty set is seconds; 120 s covers a slow writeback without letting
    /// a wedged guest hang the capture forever.
    async fn sync_guest_fs(&self, id: SandboxId) -> Result<(), SandboxError> {
        use engram_core::types::sandbox::ExecEvent;
        use futures::StreamExt;

        // Via `sh -c` so PATH/applet resolution finds sync on both
        // full images and the busybox test fixtures (no bare /bin/sync
        // there).
        let req = ExecRequest {
            // ADR 0103: this helper is used only by the warm base-image
            // capture path. Remove the warm hook's completed journals and
            // this sync command's own mkdir marker before the disk capture;
            // the wrapper deliberately tolerates losing its record and the
            // still-attached live stream carries the exact result.
            //
            // The wrapper writes its pid files and creates its output files
            // in that marker while `rm` runs, so one `rm` can fail
            // `ENOTEMPTY` on its own entry. Each failure needs a new file in
            // the marker, and the wrapper makes only a few before it waits;
            // once the marker is gone no create can succeed. So `rm` is
            // repeated until it succeeds, with a bound that only a guest that
            // fails `rm` for another reason reaches.
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "n=0; until rm -rf /var/lib/engram/execs/*; do \
                 n=$((n + 1)); [ \"$n\" -lt 20 ] || exit 1; done; sync"
                    .into(),
            ],
            stdin: None,
            env: std::collections::HashMap::new(),
            workdir: None,
            timeout: Some(std::time::Duration::from_secs(120)),
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        };
        let started = crate::time_source::metrics_now();
        let stream = self
            .exec_stream(id, req)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("pre-capture guest sync exec: {e}")))?;
        let mut events = stream.events;
        while let Some(ev) = events.next().await {
            match ev {
                ExecEvent::Exit(status) => {
                    return if status == Some(0) {
                        tracing::info!(
                            sandbox_id = %id,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "pre-capture guest sync complete",
                        );
                        Ok(())
                    } else {
                        Err(SandboxError::Snapshot(format!(
                            "pre-capture guest sync exited {status:?}"
                        )))
                    };
                }
                ExecEvent::Refused(reason) => {
                    return Err(SandboxError::Snapshot(format!(
                        "pre-capture guest sync exec refused: {reason}"
                    )));
                }
                ExecEvent::Stdout(_) | ExecEvent::Stderr(_) => {}
            }
        }
        Err(SandboxError::Snapshot(
            "pre-capture guest sync: exec stream ended without an exit status".into(),
        ))
    }

    /// Run an image's capture-time `[warm]` hook ([`WarmConfig`]) in the
    /// live capture VM, just before the base snapshot is frozen. The warm
    /// command starts a long-lived process detached (e.g. `gradle
    /// --daemon`) and exits; that process is then captured into the base
    /// snapshot so every restored session inherits it warm.
    ///
    /// Issue #539: replaces the old buffered `self.exec` + single opaque
    /// `WarmConfig::timeout()` with a streaming watchdog. A hook that
    /// emits the `::engram-warm::` progress protocol (see
    /// `warm_progress`) is killed within `ENGRAM_WARM_STALL_SECS` of
    /// going silent, or at a declared stage's `deadline_secs`, whichever
    /// is sooner; a hook that never emits a progress line keeps today's
    /// behavior verbatim (only the global timeout applies — no
    /// regression). Every failure path returns a structured
    /// `SandboxError::CaptureFailed` carrying the failing stage and the
    /// hook's last 16 KiB of combined stdout+stderr — no more "(see host
    /// logs for stderr)". On success the [`crate::warm_progress::OutputTail`]
    /// is returned too, so a LATER snapshot-phase failure can still report
    /// the warm hook's last output (the diagnosis a `status None` / a
    /// vsock-lost failure used to lose entirely).
    async fn run_warm_hook(
        &self,
        id: SandboxId,
        warm: &WarmConfig,
        env: &std::collections::HashMap<String, String>,
        progress: &tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    ) -> Result<crate::warm_progress::OutputTail, SandboxError> {
        use crate::warm_progress::{
            parse_progress_line, warm_stall_secs_from_env, OutputTail, WarmWatchdog,
            WarmWatchdogConfig, WatchdogInput,
        };
        use engram_core::types::sandbox::ExecEvent;
        use engram_core::types::{CaptureFailure, CaptureFailureKind, CapturePhase};
        use futures::StreamExt;

        // The capture VM's agentd holds NO durable session env: a capture VM
        // never gets a session bind, and `merge_session_env` is a no-op on FC
        // (the guest is already running from the snapshot, so its env can't be
        // rewritten host-side). So the manifest `[env]` must ride in the
        // ExecRequest — agentd layers `req.env` onto the exec'd child
        // (handler.rs), giving the hook JAVA_HOME/PATH/etc. Without it a
        // gradle/node warmup fails fast (the dev-brain hook exited 1 in ~40 ms
        // with no JAVA_HOME).
        let req = ExecRequest {
            command: warm.command.clone(),
            stdin: None,
            env: env.clone(),
            workdir: warm.workdir.clone(),
            // In-guest backstop unchanged: agentd SIGKILLs the child at this
            // deadline regardless of what the host-side watchdog decides.
            timeout: Some(warm.timeout()),
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        };
        tracing::info!(
            sandbox_id = %id,
            command = ?warm.command,
            timeout_secs = warm.timeout().as_secs(),
            "running capture-time [warm] hook before base-snapshot capture",
        );

        let mut stream = self.exec_stream(id, req).await.map_err(|e| {
            SandboxError::CaptureFailed(CaptureFailure {
                kind: CaptureFailureKind::WarmExecTransport,
                stage: None,
                tail: String::new(),
                message: format!(
                    "[warm] hook exec_stream failed before base-snapshot capture: {e}"
                ),
            })
        })?;

        let watchdog_cfg = WarmWatchdogConfig {
            stall: warm_stall_secs_from_env(),
            global_timeout: warm.timeout(),
        };
        let started = crate::time_source::metrics_now();
        let mut watchdog = WarmWatchdog::new(watchdog_cfg, started);
        let mut tail = OutputTail::default();
        let mut pending_stdout: Vec<u8> = Vec::new();
        let mut last_detail: Option<String> = None;

        let mut keepalive =
            engram_core::periodic(crate::warm_progress::capture_keepalive_secs_from_env());
        keepalive.tick().await; // consume the immediate first tick

        let send_progress =
            |watchdog: &WarmWatchdog, tail: &OutputTail, detail: &Option<String>| {
                let event = engram_core::types::CaptureProgress {
                    phase: CapturePhase::Warm,
                    sandbox_id: Some(id),
                    warm_stage: watchdog.current_stage_name().map(str::to_string),
                    detail: detail.clone(),
                    output_tail: tail.render(),
                    warm_stages: watchdog.stage_history(),
                };
                let _ = progress.try_send(event);
            };

        let violation_failure = |kind: CaptureFailureKind,
                                 watchdog: WarmWatchdog,
                                 tail: &OutputTail,
                                 detail: &Option<String>,
                                 message: String| {
            let stage = watchdog.current_stage_name().map(str::to_string);
            let stages = watchdog.clone().finish_failed(self.clock.now_utc());
            record_warm_stage_metrics(&stages);
            record_warm_hook_failure_metric(kind);
            let output_tail = tail.render();
            // Issue #563 review correction: this is the TERMINAL event — the
            // one carrying the failing stage + output tail an operator
            // actually needs. Unlike the routine per-line `send_progress`
            // sends, a dropped one here is worth surfacing: warn loudly
            // (rather than the usual silent `let _ =`) so a full channel
            // doesn't quietly eat the one frame that mattered.
            if let Err(e) = progress.try_send(engram_core::types::CaptureProgress {
                phase: CapturePhase::Warm,
                sandbox_id: Some(id),
                warm_stage: stage.clone(),
                detail: detail.clone(),
                output_tail: output_tail.clone(),
                warm_stages: stages,
            }) {
                tracing::warn!(
                    error = %e,
                    stage = ?stage,
                    "run_warm_hook: dropped the TERMINAL capture-progress event \
                     (progress channel full) — operator loses the live stage/tail \
                     for this failure, falling back to the CaptureFailed message",
                );
            }
            SandboxError::CaptureFailed(CaptureFailure {
                kind,
                stage,
                tail: output_tail,
                message,
            })
        };

        let exit_status = loop {
            let deadline = tokio::time::Instant::from_std(watchdog.next_deadline());
            tokio::select! {
                ev = stream.events.next() => {
                    match ev {
                        Some(ExecEvent::Stdout(bytes)) => {
                            tail.push(&bytes);
                            let now = crate::time_source::metrics_now();
                            let wall_now = self.clock.now_utc();
                            if let Some(v) = watchdog.on_event(WatchdogInput::OutputBytes, now, wall_now) {
                                return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                            }
                            pending_stdout.extend_from_slice(&bytes);
                            while let Some(pos) = pending_stdout.iter().position(|&b| b == b'\n') {
                                let line_bytes: Vec<u8> = pending_stdout.drain(..=pos).collect();
                                let line = String::from_utf8_lossy(&line_bytes);
                                let line = line.trim_end_matches(['\r', '\n']);
                                let Some(parsed) = parse_progress_line(line) else {
                                    continue;
                                };
                                last_detail = progress_line_detail(&parsed);
                                let now = crate::time_source::metrics_now();
                                let wall_now = self.clock.now_utc();
                                if let Some(v) = watchdog.on_event(WatchdogInput::Progress(parsed), now, wall_now) {
                                    return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                                }
                                send_progress(&watchdog, &tail, &last_detail);
                            }
                            // A conforming `::engram-warm::` line is always
                            // short. Newline-free output (gradle rich-console
                            // `\r` redraws, binary noise) would otherwise grow
                            // this buffer unbounded for the hook's whole
                            // 10-33 min runtime, and the `position(b'\n')`
                            // scan above re-walks it on every chunk
                            // (quadratic). Cap it at the same bound as the
                            // `OutputTail` ring buffer: past that with no
                            // newline in sight, it can't be a valid protocol
                            // line, so drop it — nothing valid is lost, and
                            // parsing resumes cleanly at the next `\n`.
                            if pending_stdout.len() > OutputTail::DEFAULT_CAP_BYTES {
                                pending_stdout.clear();
                            }
                        }
                        Some(ExecEvent::Stderr(bytes)) => {
                            tail.push(&bytes);
                            let now = crate::time_source::metrics_now();
                            let wall_now = self.clock.now_utc();
                            if let Some(v) = watchdog.on_event(WatchdogInput::OutputBytes, now, wall_now) {
                                return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                            }
                        }
                        Some(ExecEvent::Exit(status)) => break status,
                        Some(ExecEvent::Refused(reason)) => {
                            return Err(violation_failure(
                                CaptureFailureKind::WarmExitNonZero,
                                watchdog,
                                &tail,
                                &last_detail,
                                format!("[warm] hook exec refused: {reason}"),
                            ));
                        }
                        None => {
                            return Err(violation_failure(
                                CaptureFailureKind::WarmExecTransport,
                                watchdog,
                                &tail,
                                &last_detail,
                                "[warm] hook exec stream ended before an Exit event (vsock/gRPC transport lost)".into(),
                            ));
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = crate::time_source::metrics_now();
                    let wall_now = self.clock.now_utc();
                    if let Some(v) = watchdog.on_event(WatchdogInput::Tick, now, wall_now) {
                        return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                    }
                }
                _ = keepalive.tick() => {
                    send_progress(&watchdog, &tail, &last_detail);
                }
            }
        };

        match exit_status {
            Some(0) => {
                tracing::info!(sandbox_id = %id, "[warm] hook completed cleanly");
                record_warm_stage_metrics(&watchdog.stage_history());
                send_progress(&watchdog, &tail, &last_detail);
                Ok(tail)
            }
            other => {
                tracing::error!(
                    sandbox_id = %id,
                    exit_status = ?other,
                    tail = %tail.render(),
                    "[warm] hook failed; aborting base-snapshot capture",
                );
                // A `None` status means the child died to a signal — but
                // agentd's `timeout_ms` in-guest backstop (`handler.rs`)
                // is only ONE producer of that. A guest-OOM kill (one of
                // the failure causes `WarmConfig`'s own docs name) at
                // minute 2 of a 55-minute budget also reports
                // `Exit(None)`; labeling it `WarmGlobalTimeout` steers an
                // operator to raise `timeout_secs` instead of fixing
                // memory. Gate the timeout label on actually having
                // reached (near) the declared budget — some slack for
                // scheduling jitter between agentd's kill and this
                // observing it — else classify as an unattributed signal
                // kill. A stream that dies WITHOUT ever producing an Exit
                // event (handled above, `None` from
                // `stream.events.next()`) is the distinct, retryable
                // `WarmExecTransport` case.
                const TIMEOUT_SLACK: std::time::Duration = std::time::Duration::from_secs(2);
                let kind = match other {
                    Some(_) => CaptureFailureKind::WarmExitNonZero,
                    None if started.elapsed() + TIMEOUT_SLACK >= warm.timeout() => {
                        CaptureFailureKind::WarmGlobalTimeout
                    }
                    None => CaptureFailureKind::WarmKilled,
                };
                Err(violation_failure(
                    kind,
                    watchdog,
                    &tail,
                    &last_detail,
                    format!("[warm] hook exited with status {other:?} (expected 0)"),
                ))
            }
        }
    }

    /// Stage the snapshot's inputs, provision its disks, then restore.
    /// `fork_disk` starts a new disk lineage (a shared base's consumer);
    /// `fresh` swaps aux bundles to the host's current generation.
    async fn restore_with(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        fresh: bool,
        fork_disk: bool,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<(), SandboxError> {
        let new_id = id;
        let post_copy_dest = self.postcopy_dests.contains_key(&metadata.id)
            || self
                .inner
                .snapshot_path_for(metadata.id)
                .join(engram_sandbox_firecracker::MIGRATION_PEER_FILE)
                .exists();
        // Memory is served lazily (UFFD faults / peer) for a substrate
        // base-create, a UFFD resume, and every staged migration restore;
        // a File-mode restore needs a contiguous `memory.bin`. One
        // decision drives both the prefetch gate and the materialize gate.
        let memory_is_lazy = self.inner.restore_memory_is_lazy_for(fresh)
            || self
                .inner
                .snapshot_path_for(metadata.id)
                .join("migration-session-manifest.json")
                .exists();

        // A File restore reads the whole memory before the VM starts, so its
        // prefetch is on the critical path. A lazy restore faults after
        // `inner.restore`; its prefetch starts once the sidecar is local
        // (below), because the sidecar names the session's first-touch prefix.
        if let (false, Some(fill)) = (memory_is_lazy, self.fill.clone()) {
            if let Err(e) = tracing::Instrument::instrument(
                Self::prefetch_memory_chunks(&fill, metadata.memory_manifest, Class::Demand, None),
                tracing::info_span!("restore.prefetch_memory"),
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    snapshot_id = %metadata.id,
                    "memory prefetch failed; the materialize fetches serially",
                );
            }
        }

        let src = self.inner.snapshot_path_for(metadata.id);
        // A hinted peer holds the session's chunks on local disk: the disk
        // stages and every memory fetch of a lazy restore ask it before
        // the store.
        let peer = crate::peer_fill::PeerFetcher::first_live(
            metadata.peer_hints.iter().map(String::as_str),
            engram_protocol::grpc_client::PeerChunkScope::Snapshot(metadata.id),
            &self.peer_health,
        );

        // The sidecar names the memory manifest `materialize_memory_if_missing`
        // rebuilds from, so on a cross-host restore it is downloaded first.
        if let Some(chunk_store) = self.chunk_store.as_ref() {
            let blob = chunk_store.blob_storage();
            if let Err(e) = tracing::Instrument::instrument(
                materialize_state_if_missing(
                    blob.as_ref(),
                    &src,
                    metadata.state_blob_key.as_deref(),
                    metadata.sidecar_blob_key.as_deref(),
                ),
                tracing::info_span!("restore.materialize_state"),
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    src = %src.display(),
                    "state.bin/sidecar materialization failed; inner.restore will see whatever's there",
                );
            }
        }

        if let (true, Some(fill)) = (memory_is_lazy, self.fill.clone()) {
            let session = read_trace_lineage(&src).await;
            let md = metadata.clone();
            // An earlier restore's fetches for this id end before this
            // restore registers its own.
            self.drop_restore_fetches(id);
            if let Some(memory) = md.memory_manifest {
                self.sandbox_registrations
                    .insert(id, fill.register_sandbox(memory.manifest_id));
            }
            // Faults through the substrate, the prefix and the rest ask the
            // peer.
            let fill = match (peer.clone(), md.memory_manifest) {
                (Some(peer), Some(memory)) => {
                    self.peer_registrations.insert(
                        id,
                        fill.register_peer(memory.manifest_id, Arc::clone(&peer)),
                    );
                    fill.with_peer(peer)
                }
                _ => fill,
            };
            let prefetch = tokio::spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(e) = Self::prefetch_memory_chunks(
                        &fill,
                        md.memory_manifest,
                        Class::Hydrate,
                        session,
                    )
                    .await
                    {
                        tracing::warn!(
                            error = %e,
                            snapshot_id = %md.id,
                            "background memory prefetch failed; faults fetch on demand",
                        );
                    }
                },
                tracing::info_span!("restore.prefetch_memory_bg"),
            ));
            self.memory_prefetches.insert(id, AbortTaskOnDrop(prefetch));
        }

        // The handler reads the memory manifests from these copies, which
        // are written while the disks provision and removed after the restore.
        let mut staged_refs: Vec<_> = [metadata.memory_manifest, metadata.base_memory_manifest]
            .into_iter()
            .flatten()
            .collect();
        staged_refs.dedup();
        let mut staged_manifests = match (memory_is_lazy, self.fill.clone()) {
            (true, Some(fill)) => Some(AbortTaskOnDrop(tokio::spawn(
                stage_memory_manifests(fill, src.clone(), id, staged_refs.clone())
                    .instrument(tracing::info_span!("restore.stage_manifests")),
            ))),
            _ => None,
        };

        let disks = if self.volumes.is_some() {
            match tracing::Instrument::instrument(
                self.provision_for_restore(id, &metadata, &src, fresh, fork_disk, peer),
                tracing::info_span!(
                    "restore.provision_disks",
                    peer_chunks = tracing::field::Empty
                ),
            )
            .await
            {
                Ok(disks) => Some(disks),
                Err(error) => {
                    remove_staged_manifests(staged_manifests, &src, id, &staged_refs).await;
                    self.drop_failed_restore(id);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let restore = async {
            // A staged post-copy destination arms its fetch poller as soon
            // as its disks exist: the poller writes the source's sealed
            // chunks into the live volumes, then lands `state.bin` (the FC
            // load gate).
            let hint_poller = match self.postcopy_dests.remove(&metadata.id) {
                Some((_, pending)) => {
                    let live = self.live_volumes(id).await?;
                    Some(AbortTaskOnDrop(self.spawn_postcopy_fetch_poller(
                        pending,
                        src.clone(),
                        live,
                    )))
                }
                None => None,
            };

            if memory_is_lazy {
                tracing::debug!(
                    snapshot_id = %metadata.id,
                    "lazy memory restore (UFFD); skipping memory.bin materialization",
                );
                if let Some(staged) = staged_manifests.as_mut() {
                    let _ = (&mut staged.0).await;
                }
            } else if let Err(e) = self.materialize_memory_if_missing(&src).await {
                tracing::warn!(
                    error = %e,
                    src = %src.display(),
                    "memory.bin materialization failed; inner.restore will see whatever's there",
                );
            }

            // Every generation the snapshot pins, and every selected mount
            // of a fresh create, is staged before FC opens the embedded path.
            let mut pinned: Vec<AuxBundleRef> = metadata.aux_bundles.clone();
            pinned.extend(selected_mounts.iter().filter_map(|m| {
                m.sha256.as_ref().map(|sha| AuxBundleRef {
                    drive_id: m.drive_id.clone(),
                    sha256: sha.clone(),
                })
            }));
            if !pinned.is_empty() {
                match self.chunk_store.as_ref() {
                    Some(cs) => {
                        crate::bundles::BundleStore::new(
                            cs.blob_storage().clone(),
                            self.bundle_dir.clone(),
                            self.bundle_file_ext(),
                        )
                        .materialize_if_missing(&pinned)
                        .instrument(tracing::info_span!("restore.aux_bundles"))
                        .await?
                    }
                    None => tracing::debug!(
                        "snapshot pins aux bundles but no chunk store is wired; \
                         relying on locally staged generations"
                    ),
                }
            }

            // The restore runs detached from the request: a handler future
            // dropped mid-restore (client deadline, coordinator restart) must
            // not leave a half-restored VM nobody owns. The handler awaits
            // the join handle only to observe the result.
            let inner = self.inner.clone();
            let migration_roles = self.migration_roles.clone();
            let hint_pollers = self.hint_pollers.clone();
            let finish = post_copy_dest.then(|| crate::postcopy_finish::PostCopyFinish {
                id: new_id,
                inner: self.inner.clone(),
                finisher: self.finisher_with_class(Class::Background),
                source_manifest: metadata.memory_manifest,
                migration_roles: self.migration_roles.clone(),
                drained_dests: self.drained_dests.clone(),
            });
            let disks = disks.clone();
            tokio::spawn(async move {
                if fresh {
                    inner
                        .restore_fresh(new_id, metadata, disks, selected_mounts)
                        .await?
                } else {
                    inner.restore(new_id, metadata, disks).await?
                };
                if post_copy_dest {
                    migration_roles.insert(new_id, crate::migration::MigrationRole::PostCopyDest);
                }
                if let (Some(finish), Some(sock)) = (finish, inner.post_copy_control_sock(new_id)) {
                    let observed = observe_post_copy_drain(
                        sock.clone(),
                        hint_poller,
                        move |outcome| async move { finish.complete(outcome, &sock).await },
                    );
                    hint_pollers.insert(new_id, observed);
                }
                Ok(())
            })
            .await
            .map_err(|e| SandboxError::Vm(format!("restore task panicked: {e}").into()))?
        }
        .await;
        remove_staged_manifests(staged_manifests, &src, id, &staged_refs).await;
        if restore.is_err() {
            self.drop_failed_restore(id);
            if let Some(volumes) = self.volumes.as_ref() {
                if let Err(error) = volumes.release(id).await {
                    tracing::warn!(sandbox_id = %id, %error, "release after a failed restore failed");
                }
            }
        }
        restore
    }

    /// End a restore's memory fetches: cancel its background prefetch and
    /// drop its peer registration, which ends the peer's batching task.
    fn drop_restore_fetches(&self, id: SandboxId) {
        self.memory_prefetches.remove(&id);
        self.peer_registrations.remove(&id);
    }

    /// Undo what a failed restore left for the sandbox's fetches.
    fn drop_failed_restore(&self, id: SandboxId) {
        self.drop_restore_fetches(id);
        self.sandbox_registrations.remove(&id);
    }

    /// Provision a restored sandbox's disks from the snapshot's manifests.
    async fn provision_for_restore(
        &self,
        id: SandboxId,
        metadata: &SnapshotMetadata,
        src: &Path,
        fresh: bool,
        fork_disk: bool,
        peer: Option<Arc<dyn engram_fill::PeerSource>>,
    ) -> Result<SessionDisks, SandboxError> {
        let manifest = metadata.disk_manifest.ok_or_else(|| {
            SandboxError::MemoryImageUnusable("snapshot has no disk manifest".into())
        })?;
        let base = metadata.base_disk_manifest;
        let root = if fork_disk {
            Source::Fork { manifest, base }
        } else {
            Source::Continue { manifest, base }
        };
        let sidecar = read_sidecar_swap(src).await?;
        let swap = match (metadata.swap_manifest, sidecar.swap_mib) {
            (Some(_), 0) => {
                return Err(SandboxError::MemoryImageUnusable(
                    "swap manifest has no device geometry".into(),
                ));
            }
            (None, 0) => None,
            (Some(_), _) if fresh => {
                return Err(SandboxError::Snapshot(
                    "base snapshot contains swap state".into(),
                ));
            }
            (Some(manifest), _) => Some(Source::Continue {
                manifest,
                base: None,
            }),
            (None, _) if !fresh && !sidecar.base_capture => {
                return Err(SandboxError::MemoryImageUnusable(
                    "swap restore has no manifest; refusing a stale device".into(),
                ));
            }
            (None, mib) => Some(Source::Empty {
                size_bytes: mib << 20,
            }),
        };
        let peer = peer.map(crate::peer_fill::CountingPeer::new);
        let provisioned = self
            .volumes()?
            .provision_with_peer(
                id,
                root,
                swap,
                peer.clone().map(|p| p as Arc<dyn engram_fill::PeerSource>),
            )
            .await
            .map_err(|e| SandboxError::Vm(format!("provision session disks: {e}").into()));
        if let Some(peer) = &peer {
            tracing::Span::current().record("peer_chunks", peer.answered());
        }
        let provisioned = provisioned?;
        Ok(SessionDisks {
            root: provisioned.root,
            swap: provisioned.swap,
        })
    }

    pub fn new(inner: Arc<dyn SandboxBackend>) -> Self {
        let bundle_dir = inner.bundle_dir().to_path_buf();
        Self {
            inner,
            teardowns: Default::default(),
            volumes: None,
            image_cache: None,
            egress: None,
            session_bindings: Arc::new(DashMap::new()),
            unreachable_guests: Arc::new(DashMap::new()),
            dead_probe_inflight: Arc::new(DashMap::new()),
            chunk_store: None,
            fill: None,
            peer_health: crate::peer_fill::PeerHealth::new(),
            bundle_dir,
            chunk_cache: None,
            materialize_scratch: None,
            materialize_gate: Arc::new(tokio::sync::Mutex::new(())),
            oci_client: None,
            held_snapshots: DashMap::new(),
            inflight_snapshots: Arc::new(DashMap::new()),
            last_snapshot_unix_ms: Arc::new(DashMap::new()),
            first_runs: Arc::new(DashMap::new()),
            peer_registrations: Arc::new(DashMap::new()),
            sandbox_registrations: Arc::new(DashMap::new()),
            memory_prefetches: DashMap::new(),
            checkpoint_pacing: Arc::new(DashMap::new()),
            checkpoint_dir: None,
            checkpoint_recorded: Arc::new(tokio::sync::Notify::new()),
            chain_heads: None,
            checkpoint_chains: Arc::new(DashMap::new()),
            capture_gates: Arc::new(DashMap::new()),
            pending_finalizes: Arc::new(DashMap::new()),
            self_ref: Arc::new(std::sync::OnceLock::new()),
            migrations: Arc::new(crate::migration::MigrationRegistry::default()),
            migrate_peer: Arc::new(std::sync::OnceLock::new()),
            pending_presetups: Arc::new(DashMap::new()),
            postcopy_dests: Arc::new(DashMap::new()),
            migration_roles: Arc::new(DashMap::new()),
            drained_dests: Arc::new(DashMap::new()),
            hint_pollers: Arc::new(DashMap::new()),
            captures_quiesced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            host_fs: Arc::new(engram_host_core::TokioFs),
        }
    }

    /// The session disks of this host's sandboxes.
    pub fn with_session_volumes(mut self, volumes: Arc<SessionVolumes>) -> Self {
        self.volumes = Some(volumes);
        self
    }

    pub fn session_volumes(&self) -> Option<&Arc<SessionVolumes>> {
        self.volumes.as_ref()
    }

    fn volumes(&self) -> Result<&Arc<SessionVolumes>, SandboxError> {
        self.volumes
            .as_ref()
            .ok_or_else(|| SandboxError::InvalidSpec("this host has no session disk".into()))
    }

    /// A snapshot of each of the sandbox's disks while the guest runs,
    /// with the blocks it changed since the checkpoint. Nothing is left
    /// behind when one role fails.
    async fn take_precopy(
        &self,
        volumes: &SessionVolumes,
        id: SandboxId,
    ) -> Result<std::collections::BTreeMap<DiskRole, Arc<DiskSeal>>, SandboxError> {
        let mut taken: Vec<(DiskRole, PendingCheckpoint)> = Vec::new();
        let diffed = async {
            let mut precopy = std::collections::BTreeMap::new();
            for role in volumes.roles(id).await.map_err(disk_err)? {
                let pending = volumes
                    .take_precopy(id, role)
                    .instrument(tracing::info_span!("presetup.precopy_snapshot", %role))
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("{role} disk precopy: {e}")))?;
                taken.push((role, pending));
                let pending = &taken.last().expect("just pushed").1;
                let changed = volumes
                    .changed_blocks(pending, MIGRATION_DISK_BLOCK_BYTES)
                    .instrument(tracing::info_span!("presetup.precopy_diff", %role))
                    .await
                    .map_err(|e| {
                        SandboxError::Snapshot(format!("{role} disk precopy diff: {e}"))
                    })?;
                precopy.insert(role, changed);
            }
            Ok::<_, SandboxError>(precopy)
        }
        .await;
        match diffed {
            Ok(mut changed) => Ok(taken
                .into_iter()
                .map(|(role, pending)| {
                    let sealed = changed.remove(&role).expect("one diff per snapshot");
                    (
                        role,
                        Arc::new(DiskSeal {
                            pending,
                            sealed: sealed.into_iter().collect(),
                        }),
                    )
                })
                .collect()),
            Err(error) => {
                for (_, pending) in taken {
                    discard_pending(volumes, pending).await;
                }
                Err(error)
            }
        }
    }

    /// The live volumes of a sandbox, by role.
    async fn live_volumes(&self, id: SandboxId) -> Result<Vec<(DiskRole, Volume)>, SandboxError> {
        let Some(volumes) = self.volumes.as_ref() else {
            return Ok(Vec::new());
        };
        let mut live = Vec::new();
        for role in volumes.roles(id).await.map_err(disk_err)? {
            live.push((role, volumes.live(id, role).await.map_err(disk_err)?));
        }
        Ok(live)
    }

    /// ADR 0045 C2: install the page server handle (startup wiring; a
    /// second call is a startup-order bug and is ignored with a warn).
    pub fn set_migrate_peer_server(&self, server: Arc<crate::migrate_peer::PeerServer>) {
        if self.migrate_peer.set(server).is_err() {
            tracing::warn!("migrate-peer server already set; ignoring duplicate");
        }
    }

    /// ADR 0045 C2: the page server, when the host runs one.
    pub fn migrate_peer_server(&self) -> Option<Arc<crate::migrate_peer::PeerServer>> {
        self.migrate_peer.get().cloned()
    }

    /// ADR 0045 C2: a sandbox's in-flight post-copy role, if any.
    /// Lifecycle drivers (idle-evict nomination) skip roled sandboxes.
    pub fn migration_role(&self, id: SandboxId) -> Option<crate::migration::MigrationRole> {
        self.migration_roles.get(&id).map(|r| *r)
    }

    /// In-memory-only role note (reattach re-arming: the manifest
    /// already carries the role; no rewrite needed).
    pub fn note_migration_role(
        &self,
        id: SandboxId,
        role: Option<crate::migration::MigrationRole>,
    ) {
        match role {
            Some(r) => {
                self.migration_roles.insert(id, r);
            }
            None => {
                self.migration_roles.remove(&id);
            }
        }
    }

    /// Note the role in memory, then persist it to the manifest. The note
    /// is the lifecycle fence this process reads; the manifest is the hint
    /// a restart reads. A caller that SETS a role fails its operation when
    /// the persist fails (a restart must never resume a shipped source). A
    /// caller that CLEARS a role finishes its unconditional work first and
    /// reports the persist error last.
    pub async fn set_migration_role(
        &self,
        id: SandboxId,
        role: Option<crate::migration::MigrationRole>,
    ) -> Result<(), SandboxError> {
        self.note_migration_role(id, role);
        self.inner
            .set_manifest_migration_role(id, role.map(|r| r.as_str()))
            .await
    }

    /// ADR 0045 C2: the split-brain flag of a sandbox's open export
    /// (the TTL sweep's third verdict input).
    pub fn migration_state_served(&self, id: SandboxId) -> bool {
        self.migrations.state_served(id)
    }

    /// Carry out the export TTL sweep's verdict. The abort re-checks
    /// `state_served` in the same step that consumes the export, so a
    /// fetch after the verdict leaves the source paused.
    pub async fn run_export_ttl_verdict(
        &self,
        id: SandboxId,
        export_id: &str,
        verdict: crate::migration::TtlVerdict,
    ) -> Result<(), SandboxError> {
        use crate::migration::TtlVerdict;
        match verdict {
            TtlVerdict::AbortInPlace => {
                if !self.migration_abort_unserved(id, export_id).await? {
                    tracing::warn!(sandbox_id = %id, "export state served after the TTL verdict; stays paused");
                }
                Ok(())
            }
            TtlVerdict::Destroy => self.migration_commit(id, export_id).await,
            TtlVerdict::StayPaused => Ok(()),
        }
    }

    /// ADR 0028 Fix A: enable checkpoint chains, rooted at `dir`
    /// (`rolling/` + `records/` subdirs are created lazily). Without
    /// this, `snapshot()` stays pure-Full and the periodic driver
    /// no-ops.
    pub fn with_checkpoint_dir(mut self, dir: PathBuf) -> Self {
        self.chain_heads = Some(Arc::new(crate::checkpoint::ChainHeadStore::new(&dir)));
        self.checkpoint_dir = Some(dir);
        self
    }

    /// Record the sandbox→session binding directly. Production
    /// populates the map via `notify_session_policy` / registration
    /// rehydration; host-local flows with no egress policy to deliver
    /// (the checkpoint integration tests) use this.
    pub fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        self.session_bindings.insert(sandbox_id, session_id);
    }

    fn capture_gate(&self, id: SandboxId) -> Arc<CaptureGate> {
        self.capture_gates.entry(id).or_default().clone()
    }

    /// ADR 0038 B1: whether a capture or a harness spawn holds this
    /// sandbox's gate. Callers that must not queue behind either (the
    /// periodic driver, the external pause) skip on `true`.
    pub fn capture_in_flight(&self, id: SandboxId) -> bool {
        self.capture_gate(id).in_flight()
    }

    /// 2026-08-03 `chain_poisoned` alert: refuse to START any new capture
    /// from here on (see the `captures_quiesced` field doc). Called at
    /// SIGTERM before the shutdown ladder; sticky for the process
    /// lifetime.
    pub fn quiesce_captures_for_shutdown(&self) {
        self.captures_quiesced
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the SIGTERM capture quiesce is in effect. The periodic
    /// driver skips its pass outright on `true`; `capture_phase` holds
    /// the authoritative check under the capture lock.
    pub fn captures_quiesced(&self) -> bool {
        self.captures_quiesced
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The SIGTERM ladder's CaptureDrain stage: wait (bounded by
    /// `deadline`, shared across all sandboxes) for every in-flight
    /// capture to complete, so a diff capture that already consumed the
    /// KVM dirty bitmap finishes its post-processing and persists its
    /// chain-head record BEFORE the process exit cancels its tasks and
    /// poisons the chain. Callers must raise the quiesce flag first —
    /// a lock this drain acquires and releases would otherwise admit a
    /// fresh capture into the teardown window. Returns the number of
    /// stragglers (captures still running at the deadline); each will
    /// poison its chain at exit, exactly as before this stage existed.
    pub async fn drain_captures_for_shutdown(&self, deadline: std::time::Duration) -> usize {
        let start = crate::time_source::metrics_now_tokio();
        // Snapshot the lock set; the quiesce flag guarantees no NEW
        // capture can begin, so entries added after this point are
        // uncontended.
        let gates: Vec<(SandboxId, Arc<CaptureGate>)> = self
            .capture_gates
            .iter()
            .map(|e| (*e.key(), Arc::clone(e.value())))
            .collect();
        let mut stragglers = 0usize;
        for (id, gate) in gates {
            let remaining = deadline.saturating_sub(start.elapsed());
            match tokio::time::timeout(remaining, gate.serialize(id, CaptureUser::ShutdownDrain))
                .await
            {
                // Acquiring proves no capture holds the lock; release
                // immediately (the quiesce flag keeps it that way).
                Ok(_guard) => {}
                Err(_) => {
                    stragglers += 1;
                    ::metrics::counter!(crate::metrics::CAPTURE_SHUTDOWN_STRAGGLER_TOTAL)
                        .increment(1);
                    tracing::warn!(
                        sandbox_id = %id,
                        deadline_secs = deadline.as_secs_f64(),
                        "capture still in flight at the shutdown capture-drain \
                         deadline; process exit will poison its checkpoint chain \
                         (next capture is a FULL snapshot)",
                    );
                }
            }
        }
        stragglers
    }

    /// ADR 0028 Fix A: where un-acked durable checkpoint records live.
    /// ADR 0045 D5: the owned, 'static bundle of everything the snapshot
    /// POST phase (chunk + upload + chain bookkeeping) needs — so
    /// `snapshot_begin` can run it as a background task while the
    /// coordinator marks the session Idle. All fields are Arc-backed
    /// clones of the PooledBackend's.
    pub(crate) fn finisher(&self) -> SnapshotFinisher {
        SnapshotFinisher {
            volumes: self.volumes.clone(),
            chunks: self.fill.clone().map(crate::snapshot_post::Chunks::new),
            chunk_cache: self.chunk_cache.clone(),
            bundle_dir: self.bundle_dir.clone(),
            bundle_file_ext: self.bundle_file_ext(),
            inflight_snapshots: self.inflight_snapshots.clone(),
            last_snapshot_unix_ms: self.last_snapshot_unix_ms.clone(),
            checkpoint_pacing: self.checkpoint_pacing.clone(),
            checkpoint_chains: self.checkpoint_chains.clone(),
            checkpoint_dir: self.checkpoint_dir.clone(),
            checkpoint_recorded: self.checkpoint_recorded.clone(),
            chain_heads: self.chain_heads.clone(),
            session_bindings: self.session_bindings.clone(),
            clock: self.clock.clone(),
            host_fs: self.host_fs.clone(),
        }
    }

    /// A finisher whose chunk transfers are admitted at `class`. The
    /// periodic checkpoint, the cold-base seed and the post-copy chain
    /// seed take `Background` (no session waits for them); eviction, drain
    /// and migration take `Prefix` (a session waits for them).
    pub(crate) fn finisher_with_class(&self, class: Class) -> SnapshotFinisher {
        let mut f = self.finisher();
        f.chunks = f.chunks.map(|c| c.with_class(class));
        f
    }

    /// ADR 0088 addendum: the deferred-finish flavor of `snapshot()`,
    /// used by the cold-base seed so its multi-GiB upload runs
    /// CONCURRENTLY with the warm hook instead of blocking it. The
    /// capture_phase (pause → dump to local NVMe → resume) stays
    /// synchronous — the FC dirty bitmap is consumed inside it, which
    /// is exactly what makes hook-dirtied pages the final Diff's dirty
    /// set — then `finish()` (disk flush upload, memory chunk upload,
    /// portable blobs, chain seed) runs on a spawned task holding the
    /// capture-lock guard, so the final snapshot's own `capture_phase`
    /// naturally serializes behind it even if the hook is instant.
    ///
    /// Callers MUST `join()` (never drop/abort) before taking another
    /// snapshot of `id` and before reporting the capture durable:
    /// - join-before-final-snapshot ⇒ the chain is seeded
    ///   (`advance_checkpoint_state` is finish()'s last step) so the
    ///   final capture stays a Diff;
    /// - aborting mid-finish could leak a snapshot dir the
    ///   `inflight_snapshots` bookkeeping no longer tracks;
    /// - a joined `Ok` is the SAME durability barrier `snapshot()`
    ///   provides, moved in wall-clock only.
    pub(crate) async fn snapshot_deferred(
        &self,
        id: SandboxId,
    ) -> Result<DeferredSnapshot, SandboxError> {
        let (capture_guard, cap) = self
            .capture_phase(id, CaptureUser::DeferredSnapshot)
            .await?;
        self.spawn_trace_publish(id);
        // The cold-base seed's multi-GiB upload: no session waits for it.
        let finisher = self.finisher_with_class(Class::Background);
        Ok(DeferredSnapshot {
            handle: tokio::spawn(async move {
                let _guard = capture_guard;
                finisher.finish(id, cap).await
            }),
        })
    }

    /// Issue #529: install the weak self-reference `snapshot_begin`'s
    /// spawned finalize job upgrades to reach the full `destroy()` at its
    /// terminal stage. MUST be called exactly once, immediately after
    /// `Arc::new(p)` — before that, `eviction_finalizer()` builds a bundle
    /// whose `self_ref` can never upgrade, and any finalize job it drives
    /// silently skips its own destroy call (logged, not fatal — the
    /// teardown reconcile backstops it, but it's a real gap).
    /// A second call is a startup-order bug; logged and ignored rather
    /// than panicking (mirrors `set_migrate_peer_server`).
    pub fn set_self_ref(&self, arc: &Arc<PooledBackend>) {
        if self.self_ref.set(Arc::downgrade(arc)).is_err() {
            tracing::warn!("PooledBackend self_ref already set; ignoring duplicate");
        }
    }

    /// Issue #529: the eviction finalize bundle, or `None` when
    /// checkpointing is disabled (`checkpoint_dir` unset) — the same gate
    /// `snapshot_begin` checks before ever constructing one.
    pub(crate) fn eviction_finalizer(&self) -> Option<crate::eviction_finalize::EvictionFinalizer> {
        let checkpoint_dir = self.checkpoint_dir.clone()?;
        Some(crate::eviction_finalize::EvictionFinalizer::new(
            self.volumes.clone(),
            self.fill.clone(),
            self.chunk_cache.clone(),
            self.bundle_dir.clone(),
            self.bundle_file_ext(),
            checkpoint_dir,
            self.checkpoint_recorded.clone(),
            self.pending_finalizes.clone(),
            Arc::new(PooledDestroyer {
                self_ref: self.self_ref.clone(),
            }),
            self.host_fs.clone(),
            crate::eviction_finalize::max_attempts(),
        ))
    }

    /// Delete the volumes of every sandbox that is neither alive nor
    /// awaiting an eviction finalize. Runs once at startup, after the
    /// reattach pass fixes the live set and before anything can
    /// provision a sandbox.
    pub async fn reconcile_volumes(&self) {
        let Some(volumes) = &self.volumes else {
            return;
        };
        let live = match self.inner.list().await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, "volume reconcile skipped: cannot list sandboxes");
                return;
            }
        };
        let finalizer = self.eviction_finalizer();
        match crate::eviction_finalize::reconcile_volumes(volumes, finalizer.as_ref(), live).await {
            Ok(report) => {
                if !report.deleted.is_empty() || !report.failed.is_empty() {
                    tracing::info!(
                        deleted = report.deleted.len(),
                        failed = report.failed.len(),
                        "startup volume reconcile",
                    );
                }
                for (name, error) in &report.failed {
                    tracing::warn!(volume = %name, %error, "volume reconcile: delete failed");
                }
            }
            Err(error) => tracing::warn!(%error, "startup volume reconcile failed"),
        }
    }

    /// Issue #529: re-drive every un-acked eviction finalize record at
    /// host-agent startup — the whole crash story. A host-agent pod roll
    /// mid-upload now DELAYS the commit by one restart; it cannot lose
    /// it. Called from `lib.rs` alongside the checkpoint driver spawn,
    /// with the SAME `Arc<PooledBackend>` used there (so `set_self_ref`
    /// must have run first).
    pub async fn resume_pending_finalizes(self: &Arc<Self>) {
        let Some(finalizer) = self.eviction_finalizer() else {
            return;
        };
        let records = crate::eviction_finalize::EvictionFinalizeRecord::load_all(
            self.host_fs.as_ref(),
            &finalizer.finalize_dir(),
        )
        .await;
        if records.is_empty() {
            return;
        }
        tracing::info!(
            count = records.len(),
            "re-driving eviction finalize records from disk (host-agent startup, issue #529)",
        );
        for record in records {
            finalizer
                .pending_finalizes
                .insert(record.sandbox_id, record.snapshot_id);
            metrics::counter!(crate::metrics::EVICTION_FINALIZE_REDRIVEN_TOTAL).increment(1);
            let gate = self.capture_gate(record.sandbox_id);
            let f = finalizer.clone();
            tokio::spawn(async move {
                let guard = gate
                    .serialize(record.sandbox_id, CaptureUser::FinalizeRedrive)
                    .await;
                crate::eviction_finalize::run_eviction_finalize(f, record, guard).await;
            });
        }
    }

    /// Note a harness event of the sandbox. The first run that starts in
    /// this sandbox ends the first-touch prefix its captures publish, when
    /// it completes. A completion of another run (one the harness replays
    /// from before the restore) does not count.
    pub fn note_harness_event(&self, id: SandboxId, event: &engram_harness_proto::HarnessEvent) {
        // One reader loop delivers a sandbox's events in order.
        let current = self.first_runs.get(&id).map(|e| e.clone());
        if let Some(next) = first_run_after(current.as_ref(), event, || {
            self.clock.now_utc().timestamp_millis()
        }) {
            self.first_runs.insert(id, next);
        }
    }

    /// Publish this sandbox's per-jail working-set trace to the blob store
    /// under the session-stable **canonical** key, so the session's NEXT
    /// resume finds it and prefaults its working set (the replay side landed
    /// with the Tier 2 `--trace-key` fix in #517).
    ///
    /// Why the host-agent publishes it (not the handler): the `engram-uffd-
    /// handler` only `put_trace`s on a clean fault-loop exit, but eviction
    /// **SIGKILLs** the handler (`engram-sandbox-firecracker` `destroy`,
    /// ADR 0044 K2 — no `kill_on_drop`) before that runs, so in prod NO trace
    /// was ever published and prefault-on-resume was inert regardless of the
    /// key. The handler *does* write the per-jail `working-set-trace.json`
    /// periodically (the ADR 0045 C2 migration rider dump); we lift that
    /// same file into the blob store at capture time, when the handler is
    /// still alive and the file is present.
    ///
    /// **Detached + best-effort:** spawned off the (latency-critical, ADR
    /// 0045 D5) capture path so a GCS `put_trace` never blocks the
    /// user-visible eviction. A no bound session / no chunk store / absent /
    /// empty / corrupt trace is a silent no-op — the handler's on-demand
    /// fault path is the backstop. Idempotent: re-publishing the same
    /// canonical key is a harmless overwrite (a session runs one place at a
    /// time, so there are no concurrent writers).
    fn spawn_trace_publish(&self, id: SandboxId) {
        let Some(session_id) = self.session_bindings.get(&id).map(|e| *e) else {
            return; // no bound session → no canonical key to publish under
        };
        let Some(trace_path) = self.working_set_trace_path(id) else {
            return; // backend has no per-jail trace (VZ / process)
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return;
        };
        let first_run_completed = match self.first_runs.get(&id).as_deref() {
            Some(FirstRun::Completed { at_ms }) => Some(*at_ms),
            _ => None,
        };
        if let Some(dir) = trace_path.parent() {
            let file = dir.join(engram_chunk_store::FirstTouch::FILE);
            let store = chunk_store.clone();
            tokio::spawn(async move {
                publish_first_touch(&store, session_id, &file, first_run_completed).await;
            });
        }
        tokio::spawn(async move {
            let bytes = match tokio::fs::read(&trace_path).await {
                Ok(b) => b,
                Err(_) => return, // handler never wrote it / already torn down
            };
            let trace: engram_chunk_store::working_set::WorkingSetTrace =
                match serde_json::from_slice(&bytes) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(sandbox_id = %id, error = %e, "working-set trace parse failed; not published");
                        return;
                    }
                };
            if trace.chunks.is_empty() {
                return; // nothing to prefault — don't publish an empty trace
            }
            let n = trace.chunks.len();
            let trace_ref =
                engram_chunk_store::working_set::TraceRef::canonical(session_id.as_uuid());
            match chunk_store.put_trace(trace_ref, &trace).await {
                Ok(()) => tracing::info!(
                    sandbox_id = %id,
                    session_id = %session_id,
                    chunks = n,
                    "published session working-set trace (resume prefault)",
                ),
                Err(e) => tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "publish session working-set trace failed (best-effort)",
                ),
            }
        });
    }

    /// ADR 0045 D5: pause + drain + FC capture. Returns the capture lock,
    /// which the post phase must hold wherever it runs, and the artifacts
    /// the post phase consumes. The quiesce guard ends on return. A
    /// [`CaptureUser::HeldCapture`] leaves the guest paused.
    #[tracing::instrument(
        name = "host.capture_phase",
        skip_all,
        fields(
            sandbox_id = %id,
            user = user.as_str(),
            snap_type = tracing::field::Empty,
            paused_ms = tracing::field::Empty,
        )
    )]
    async fn capture_phase(
        &self,
        id: SandboxId,
        user: CaptureUser,
    ) -> Result<(tokio::sync::OwnedMutexGuard<()>, SnapshotCapture), SandboxError> {
        let hold = user == CaptureUser::HeldCapture;
        let gate = self.capture_gate(id);
        // ADR 0038 B0: time the lock wait — the gridlock signal. With
        // B1 periodic checkpoints skip rather than queue, so a long tail
        // here is an eviction/drain blocked on an in-flight capture.
        let lock_wait = crate::time_source::metrics_now();
        let capture_guard = gate.serialize(id, user).await;
        if self.held_snapshots.contains_key(&id)
            || self.migration_role(id) == Some(crate::migration::MigrationRole::HeldSource)
        {
            return Err(SandboxError::Snapshot(
                "snapshot hold already in progress; resume or destroy required".into(),
            ));
        }
        // A post-copy destination's first capture must follow its chain
        // seed: the seed relies on the dirty log that runs since load,
        // and a capture consumes it. The post-copy finish clears the role.
        if self.migration_role(id) == Some(crate::migration::MigrationRole::PostCopyDest) {
            return Err(SandboxError::Snapshot(
                "post-copy destination: captures wait for the memory drain and the chain seed"
                    .into(),
            ));
        }
        metrics::histogram!(crate::metrics::SNAPSHOT_CAPTURE_LOCK_WAIT_SECONDS)
            .record(lock_wait.elapsed().as_secs_f64());
        // 2026-08-03 `chain_poisoned` alert: the SIGTERM capture-quiesce
        // gate. Checked under the capture lock (so a capture queued
        // behind the shutdown drain's acquire-and-release cannot slip
        // past) and BEFORE the write-ahead invalidate below — nothing is
        // consumed yet, so refusing here is free: the chain, its durable
        // head record, and the guest all stay intact for the successor
        // generation to capture.
        if self.captures_quiesced() {
            return Err(SandboxError::Snapshot(
                "capture refused: host-agent is shutting down (SIGTERM capture \
                 quiesce); nothing was consumed — the successor generation \
                 captures next"
                    .into(),
            ));
        }
        // Diff-mode when a checkpoint chain exists: same coherent
        // (memory, disk) capture contract, O(dirty set) cost. The
        // chain seeds on the first (Full) capture below — so an
        // eviction on a long-running session is just the final diff
        // in its checkpoint chain, collapsing the multi-GiB-dump
        // window.
        let chain_prev = self
            .checkpoint_chains
            .get(&id)
            .map(|c| (c.manifest_ref, c.manifest.clone()));

        // Write-ahead invalidate of the durable chain-head record: BOTH
        // snapshot flavors below consume+reset the KVM dirty bitmap the
        // moment FC runs `PUT /snapshot/create`, and from that instant
        // the on-disk record — the seed source for survivor rehydrate
        // after a pod roll — would describe a baseline the bitmap no
        // longer has. Remove it BEFORE the create (under the capture
        // lock, before the pause); it is re-written only after the chain
        // durably advances (`advance_checkpoint_state`). A crash
        // anywhere between leaves no record → the survivor's next
        // capture is a safe Full. The 2026-07-13 roll tore a capture in
        // exactly this window (post-processing died at 22:23:46 with the
        // bitmap consumed), which is why the coordinator's snapshots
        // rows can never be the rehydrate seed source. A failed unlink
        // (non-NotFound) aborts the capture: nothing is paused or
        // consumed yet, and proceeding would leave a stale record the
        // create is about to falsify.
        if let Some(store) = &self.chain_heads {
            store.invalidate(id).map_err(|e| {
                SandboxError::Snapshot(format!("chain-head write-ahead invalidate: {e}"))
            })?;
        }

        // ADR 0014 issue #1/#2: if a prior snapshot for this sandbox
        // was produced but never committed (caller's downstream
        // pipeline failed or the coord pod crashed between snapshot
        // and commit), tear down its artifacts BEFORE we mint a fresh
        // SnapshotId. This is the overwrite-in-place semantic that
        // keeps the host-side disk bounded under coord-side retry
        // storms. Without it, each retry mints a fresh id and leaves
        // the prior 4 GiB dir on disk; 25 retries fill a host in
        // 13 min.
        if self.inflight_snapshots.contains_key(&id) {
            if let Err(e) = self.abort_prior_inflight_snapshot(id).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "snapshot retry: best-effort abort of prior in-flight snapshot failed; \
                     proceeding with fresh attempt anyway",
                );
            }
        }

        // ADR 0021 P1.6 follow-up: gate snapshot on agentd readiness.
        //
        // The pre-flush pause below freezes vCPUs. If the guest is still
        // in early boot (kernel → engram-init → agentd start) at that
        // moment, the snapshot captures a half-initialised kernel — in
        // particular, agentd may not have completed `bind(AF_VSOCK)` —
        // and the resumed kernel comes up with a half-initialised vsock
        // driver. `panic=1 reboot=k` then trips `KVM_EXIT_SHUTDOWN`
        // ~1 s after `load_snapshot` returns; every subsequent
        // host→guest dial fails ECONNREFUSED.
        //
        // Reachable in prod via:
        //   1. SIGTERM-during-cold-boot. `shutdown.rs` checkpoints all
        //      sandboxes from `backend.list()` after a 5 s drain — a
        //      sandbox still booting at SIGTERM gets snapshotted mid-
        //      bind without this gate.
        //   2. coord-driven `snapshot(id)` fired quickly after create
        //      (any fast-rebake / probe path).
        //
        // `wait_agent_ready` blocks until agentd has dialled the host's
        // ready port (port 1027) — proving the guest's vsock stack is
        // operational end-to-end. On warm-restored sandboxes the watch
        // is pre-set true (FC's `restore_in_jail`), so this is a no-op
        // for warm re-snapshot paths. The FC backend is the only one
        // with the per-sandbox agent_ready watch; non-FC backends
        // (Process, future) return `InvalidSpec` from the default trait
        // impl, which we treat as "no readiness concept here, proceed."
        match self
            .inner
            .wait_agent_ready(id)
            .instrument(tracing::info_span!("capture.agent_ready"))
            .await
        {
            Ok(()) => {}
            Err(SandboxError::InvalidSpec(_)) => {}
            Err(e) => return Err(SandboxError::Snapshot(format!("wait_agent_ready: {e}"))),
        }

        let _quiesce = gate.quiesce(id, user).await;
        // The pause instant is the coherence cut for memory, disk, and
        // the event log: the coordinator resolves the session_events
        // cursor against it.
        if hold {
            // Persisted BEFORE the pause: a host-agent that restarts while
            // the move is open re-adopts this VM as a frozen source (never
            // self-resumes, never a checkpoint candidate) instead of a
            // running guest the periodic capture would un-pause.
            self.set_migration_role(id, Some(crate::migration::MigrationRole::HeldSource))
                .await?;
        }
        let paused_at = self.clock.now_utc();
        let pause_started = crate::time_source::metrics_now();
        self.inner
            .pause(id)
            .instrument(tracing::info_span!("capture.pause"))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("pre-flush pause: {e}")))?;

        // Armed the instant the guest is paused: a failure or a dropped
        // handler future from here must resume it and discard what the
        // capture took.
        let mut unwind = CaptureUnwind::new(self, id);
        unwind.resume_on_drop = !hold;
        unwind.arm();

        // Under the pause, only snapshot the disks: the upload runs in the
        // finisher after the guest resumes, so the pause is O(snapshot).
        // The content is fixed here, paired with the memory capture below.
        if let Some(volumes) = self.volumes.as_ref() {
            for role in volumes.roles(id).await.map_err(disk_err)? {
                if role == DiskRole::Swap && self.inner.is_base_capture(id) {
                    continue;
                }
                let pending = volumes
                    .take(id, role)
                    .instrument(tracing::info_span!("capture.disk_take", %role))
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("{role} disk checkpoint: {e}")))?;
                unwind.pending.push(pending);
            }
        }
        // ADR 0007 Phase 6: backend owns its staging dir; we look
        // it up via snapshot_path_for after the inner call so we
        // can read/patch the on-disk artifacts the inner backend
        // wrote (memory.bin / memory.diff, manifest.json). The inner
        // call's pause-create-resume cycle is idempotent against our
        // earlier pause and brings the VM back to running on exit —
        // for the diff flavor that resume lands after O(dirty set),
        // not O(guest RAM).
        // ADR 0038 B0: time the FC memory capture (`PUT /snapshot/
        // create`) — the previously-invisible step that hung 60 s on the
        // cold Full seed. After B2, `type="full"` should vanish on the
        // resume path (chain seeded → diff).
        let snap_type = if chain_prev.is_some() { "diff" } else { "full" };
        tracing::Span::current().record("snap_type", snap_type);
        let create_start = crate::time_source::metrics_now();
        let create_res = if hold {
            self.inner.snapshot_hold(id, chain_prev.is_some()).await
        } else if chain_prev.is_some() {
            self.inner.snapshot_diff(id).await
        } else {
            self.inner.snapshot(id).await
        };
        if !hold {
            // The inner create resumed the guest before it returned.
            tracing::Span::current()
                .record("paused_ms", pause_started.elapsed().as_millis() as u64);
        }
        metrics::histogram!(
            crate::metrics::SNAPSHOT_CREATE_SECONDS,
            "type" => snap_type,
            "outcome" => if create_res.is_ok() { "success" } else { "error" },
        )
        .record(create_start.elapsed().as_secs_f64());
        let metadata = match create_res {
            Ok(m) => m,
            Err(e) => {
                // A failed Diff create may still have consumed the dirty
                // bitmap (FC reads+resets it before/while writing the
                // file; mid-write failure semantics are undefined).
                // Conservative: poison. Worst case is one unnecessary
                // Full; the alternative is silent memory corruption.
                if chain_prev.is_some() {
                    poison_checkpoint_chain_after_failed_diff(
                        &self.checkpoint_chains,
                        self.chain_heads.as_deref(),
                        id,
                        "fc snapshot_diff create",
                    );
                }
                return Err(e);
            }
        };
        let dest = self.inner.snapshot_path_for(metadata.id);
        // `create_res` was Ok ⟹ `inner.snapshot`/`snapshot_diff` brought
        // the guest back running. The guard rides into `SnapshotCapture`
        // (still armed) so the cancellation gap in `snapshot_begin` and
        // any failure before the finisher consumes it stays covered; the
        // finisher takes the pending out of the guard and defuses it.
        Ok((
            capture_guard,
            SnapshotCapture {
                metadata,
                dest,
                chain_prev,
                paused_at,
                writes_memory: self.inner.captures_guest_memory(),
                unwind,
            },
        ))
    }

    /// ADR 0045 C2 (E2B fold): read the sandbox's per-jail working-set
    /// trace (fault-order hot set) for the migration rider. Best-effort
    /// by design: an absent/corrupt file (handler predates the dump,
    /// guest never faulted, bake-profile override) returns empty.
    fn read_hot_chunks(&self, id: SandboxId) -> Vec<[u8; 32]> {
        let Some(path) = self.inner.working_set_trace_path(id) else {
            return Vec::new();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Vec::new();
        };
        match serde_json::from_slice::<engram_chunk_store::working_set::WorkingSetTrace>(&bytes) {
            Ok(trace) => trace.chunks.iter().map(|h| *h.as_bytes()).collect(),
            Err(e) => {
                tracing::debug!(sandbox_id = %id, error = %e, "hot-set trace parse failed; empty rider");
                Vec::new()
            }
        }
    }

    /// ADR 0045 C2 (destination): stage the presetup's restore package
    /// — the sidecar, the inline session manifest, and the peer spec —
    /// then arm the fetch poller (spawned inside `restore_with` once
    /// the session volumes exist). No export artifacts exist yet: the
    /// source hasn't paused.
    async fn postcopy_stage(
        &self,
        metadata: &SnapshotMetadata,
        mig: &engram_core::types::snapshot::MigrationSourceInfo,
    ) -> Result<(), SandboxError> {
        let (Some(peer_addr), Some(peer_token)) = (&mig.peer_addr, &mig.peer_token) else {
            return Err(SandboxError::InvalidSpec(
                "post-copy rider missing peer_addr/peer_token".into(),
            ));
        };
        if mig.sidecar_json.is_empty() {
            return Err(SandboxError::InvalidSpec(
                "post-copy rider missing the sidecar".into(),
            ));
        }
        let dest = self.inner.snapshot_path_for(metadata.id);
        fs::create_dir_all(&dest)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("create snapshot dir: {e}")))?;
        let hot_trace_file = format!(
            "migration-hot-{}.json",
            crate::time_source::unique_path_token()
        );
        fs::write(dest.join("manifest.json"), &mig.sidecar_json)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write sidecar: {e}")))?;
        fs::write(
            dest.join("migration-session-manifest.json"),
            &mig.memory_manifest_json,
        )
        .await
        .map_err(|e| SandboxError::Snapshot(format!("write session manifest: {e}")))?;
        let peer_spec = engram_sandbox_firecracker::MigrationPeerSpec {
            hot_trace_file: hot_trace_file.clone(),
            peer_addr: peer_addr.clone(),
            export_id: mig.export_id.clone(),
            peer_token: peer_token.clone(),
        };
        fs::write(
            dest.join(engram_sandbox_firecracker::MIGRATION_PEER_FILE),
            serde_json::to_vec(&peer_spec)
                .map_err(|e| SandboxError::Snapshot(format!("peer spec json: {e}")))?,
        )
        .await
        .map_err(|e| SandboxError::Snapshot(format!("write peer spec: {e}")))?;
        self.postcopy_dests.insert(
            metadata.id,
            PostCopyDestPending {
                hot_trace_file,
                source_addr: mig.source_addr.clone(),
                export_id: mig.export_id.clone(),
            },
        );
        Ok(())
    }

    /// The destination's fetch poller. While the source guest still
    /// runs, pull the presetup's precopy tier into the live volumes.
    /// Once the capture lands (the seal answers), pull `state.bin` and
    /// the seal tier, then write `state.bin` last: its appearance is the
    /// FC load gate, so the guest never runs on a disk that could be
    /// stale. Every error arm returns without landing `state.bin`; the
    /// load gate times out and the coordinator aborts to the source
    /// with zero loss.
    fn spawn_postcopy_fetch_poller(
        &self,
        pending: PostCopyDestPending,
        dest_dir: std::path::PathBuf,
        live: Vec<(DiskRole, Volume)>,
    ) -> tokio::task::JoinHandle<()> {
        use engram_core::types::snapshot::MigrationItem;
        let parent = tracing::Span::current();
        tokio::spawn(async move {
            let budget = std::time::Duration::from_secs(240);
            let started = crate::time_source::metrics_now();
            let tmp = dest_dir.join("postcopy-tmp");
            let _ = fs::create_dir_all(&tmp).await;

            // Dial once and poll tight over the live channel; a failed
            // attempt is a sub-ms NotFound on the pod network.
            let mut source: Option<engram_protocol::grpc_client::GrpcHostClient> = None;
            // The tiers in order; each one is polled until the source
            // serves it.
            #[derive(Clone, Copy, Debug)]
            enum Stage {
                Precopy,
                StateBin,
                Seal,
            }
            impl Stage {
                /// The span stays open until the source serves the stage,
                /// so it includes the wait for the source.
                fn span(self, parent: &tracing::Span) -> tracing::Span {
                    let blocks = tracing::field::Empty;
                    match self {
                        Stage::Precopy => {
                            tracing::info_span!(parent: parent, "move.dest_precopy", blocks)
                        }
                        Stage::StateBin => {
                            tracing::info_span!(parent: parent, "move.dest_state_bin")
                        }
                        Stage::Seal => {
                            tracing::info_span!(parent: parent, "move.dest_seal", blocks)
                        }
                    }
                }
            }
            let mut stage = Stage::Precopy;
            let mut stage_span = stage.span(&parent);
            let (mut precopy_blocks, mut sealed_blocks) = (0, 0);
            loop {
                if started.elapsed() > budget {
                    tracing::error!(
                        export_id = %pending.export_id,
                        ?stage,
                        "post-copy fetch poller timed out; the FC load gate will tear the restore down",
                    );
                    return;
                }
                let client = match &source {
                    Some(c) => c,
                    None => match Self::dial_migration_source(&pending.source_addr).await {
                        Ok(c) => source.insert(c),
                        Err(e) => {
                            tracing::debug!(error = %e, "post-copy source dial failed; retrying");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    },
                };
                let next = match stage {
                    Stage::Precopy => Self::pull_disk_tier(
                        client,
                        &pending.export_id,
                        &live,
                        MigrationItem::DiskPrecopyInfo,
                    )
                    .instrument(stage_span.clone())
                    .await
                    .map(|blocks| {
                        precopy_blocks = blocks;
                        stage_span.record("blocks", blocks);
                        Some(Stage::StateBin)
                    }),
                    Stage::StateBin => Self::fetch_postcopy_artifacts(
                        client,
                        &pending.export_id,
                        &tmp,
                        &[MigrationItem::StateBin],
                    )
                    .instrument(stage_span.clone())
                    .await
                    .map(|()| Some(Stage::Seal)),
                    Stage::Seal => Self::pull_disk_tier(
                        client,
                        &pending.export_id,
                        &live,
                        MigrationItem::DiskSealInfo,
                    )
                    .instrument(stage_span.clone())
                    .await
                    .map(|blocks| {
                        sealed_blocks = blocks;
                        stage_span.record("blocks", blocks);
                        None
                    }),
                };
                match next {
                    Ok(None) => break,
                    Ok(Some(reached)) => {
                        stage = reached;
                        stage_span = stage.span(&parent);
                    }
                    // Not served yet: the export is not open or the
                    // capture has not run.
                    Err(SandboxError::NotFound) => {
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                    // A transport failure poisons the cached channel.
                    Err(e) => {
                        tracing::debug!(error = %e, ?stage, "post-copy fetch not ready; retrying");
                        source = None;
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                }
            }
            drop(stage_span);
            let Some(client) = source.as_ref() else {
                return;
            };

            if let Err(e) = fs::rename(tmp.join("state.bin"), dest_dir.join("state.bin")).await {
                tracing::error!(error = %e, "post-copy state.bin land failed");
                return;
            }
            tracing::info!(
                export_id = %pending.export_id,
                precopy_blocks,
                sealed_blocks,
                "post-copy restore inputs staged (state.bin landed)",
            );
            // Demand faults can run now. The hint task polls for the
            // optional artifact, so idle reads do not extend blackout.
            poll_hot_hint(budget.saturating_sub(started.elapsed()), || async {
                if let Err(error) = Self::fetch_postcopy_artifacts(
                    client,
                    &pending.export_id,
                    &tmp,
                    &[engram_core::types::snapshot::MigrationItem::HotHint],
                )
                .await
                {
                    if !matches!(error, SandboxError::NotFound) {
                        tracing::info!(%error, "post-copy hint fetch failed; drain continues");
                    }
                    return false;
                }
                let bytes = match fs::read(tmp.join("hot-hint.json")).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::info!(%error, "post-copy hint read failed; drain continues");
                        return true;
                    }
                };
                let Some(hot) = parse_hot_hint(&bytes) else {
                    return false;
                };
                let path = dest_dir.join(&pending.hot_trace_file);
                // File I/O is independent of the Tokio workers serving the move.
                if let result @ (Ok(Err(_)) | Err(_)) =
                    tokio::task::spawn_blocking(move || stage_hot_trace(&path, &hot)).await
                {
                    tracing::info!(?result, "post-copy hint staging failed; drain continues");
                }
                true
            })
            .await;
        })
    }

    /// Pull one tier of disk blocks into the live volumes: the
    /// descriptor `tier` names for each role, then its blocks. Returns
    /// the block count; `NotFound` while the source does not serve the
    /// tier yet.
    async fn pull_disk_tier(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        live: &[(DiskRole, Volume)],
        tier: fn(DiskRole) -> engram_core::types::snapshot::MigrationItem,
    ) -> Result<u64, SandboxError> {
        let mut blocks = 0;
        for (role, volume) in live {
            let info = Self::fetch_disk_info(source, export_id, tier(*role)).await?;
            Self::fetch_disk_blocks(source, export_id, *role, &info.block_indices, volume).await?;
            blocks += info.block_indices.len() as u64;
        }
        Ok(blocks)
    }

    /// One descriptor item, read whole.
    async fn fetch_disk_info(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        item: engram_core::types::snapshot::MigrationItem,
    ) -> Result<engram_core::types::snapshot::DiskSealInfo, SandboxError> {
        use futures::StreamExt;
        let mut stream = source.migration_fetch(export_id, vec![item]).await?;
        let mut bytes = Vec::new();
        while let Some(frame) = stream.next().await {
            bytes.extend_from_slice(&frame?.data);
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| SandboxError::Snapshot(format!("disk seal info: {e}")))
    }

    /// Write the source's blocks of one disk into the live volume, a few
    /// batches in flight at once, and sync it.
    async fn fetch_disk_blocks(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        role: DiskRole,
        blocks: &[u64],
        volume: &Volume,
    ) -> Result<(), SandboxError> {
        use futures::stream::FuturesUnordered;
        use futures::StreamExt;
        // 16 MiB per request at the 64 KiB block size.
        const BATCH: usize = 256;
        const IN_FLIGHT: usize = 8;
        let mut in_flight = FuturesUnordered::new();
        let mut failed = None;
        for batch in blocks.chunks(BATCH) {
            if in_flight.len() >= IN_FLIGHT {
                if let Some(Err(e)) = in_flight.next().await {
                    failed = Some(e);
                    break;
                }
            }
            in_flight.push(Self::fetch_block_batch(
                source, export_id, role, batch, volume,
            ));
        }
        // Every batch lands or fails before this returns: a write still
        // in flight would land under whatever the caller does next.
        while let Some(done) = in_flight.next().await {
            if let Err(e) = done {
                failed.get_or_insert(e);
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        volume
            .sync()
            .await
            .map_err(|e| SandboxError::Snapshot(format!("sync {role} volume: {e}")))
    }

    async fn fetch_block_batch(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        role: DiskRole,
        batch: &[u64],
        volume: &Volume,
    ) -> Result<(), SandboxError> {
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        let items = batch
            .iter()
            .map(|index| MigrationItem::DiskBlockAt(role, *index))
            .collect();
        // Contiguous blocks land in one write of up to this many bytes.
        const MAX_RUN: usize = 4 * 1024 * 1024;
        let mut stream = source.migration_fetch(export_id, items).await?;
        let mut items = ItemAssembler::new(batch.len());
        let mut run: Option<(u64, Vec<u8>)> = None;
        while let Some(frame) = stream.next().await {
            let Some((idx, bytes)) = items.push(frame?)? else {
                continue;
            };
            // The assembler returns only requested items.
            let index = batch[idx as usize];
            let start = index * MIGRATION_DISK_BLOCK_BYTES;
            let want = MIGRATION_DISK_BLOCK_BYTES.min(volume.size_bytes.saturating_sub(start));
            if bytes.len() as u64 != want {
                return Err(SandboxError::Snapshot(format!(
                    "sealed disk block {index}: {} bytes, want {want}",
                    bytes.len()
                )));
            }
            match &mut run {
                Some((run_start, buf))
                    if *run_start + buf.len() as u64 == start && buf.len() < MAX_RUN =>
                {
                    buf.extend_from_slice(&bytes);
                }
                _ => {
                    if let Some((run_start, buf)) = run.replace((start, bytes)) {
                        Self::write_disk_run(volume, run_start, buf).await?;
                    }
                }
            }
        }
        if let Some((run_start, buf)) = run {
            Self::write_disk_run(volume, run_start, buf).await?;
        }
        items.finish()
    }

    async fn write_disk_run(
        volume: &Volume,
        start: u64,
        bytes: Vec<u8>,
    ) -> Result<(), SandboxError> {
        let len = bytes.len();
        volume.write_range(start, bytes).await.map_err(|e| {
            SandboxError::Snapshot(format!("write sealed disk range {start}+{len}: {e}"))
        })
    }

    /// Dial a migration source's gRPC endpoint.
    async fn dial_migration_source(
        source_addr: &str,
    ) -> Result<engram_protocol::grpc_client::GrpcHostClient, SandboxError> {
        let channel = tonic::transport::Endpoint::from_shared(source_addr.to_string())
            .map_err(|e| SandboxError::InvalidSpec(format!("bad source_addr: {e}")))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| SandboxError::Snapshot(format!("dial migration source: {e}")))?;
        Ok(engram_protocol::grpc_client::GrpcHostClient::new(channel))
    }

    /// Fetch the disk seal and VM state into `dir` (each under its
    /// canonical filename). One-shot; errors when the export isn't
    /// open yet (the poller's retry signal). Takes a CONNECTED client
    /// — the poller reuses one channel across attempts instead of a
    /// TCP+HTTP/2 handshake per poll.
    async fn fetch_postcopy_artifacts(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        dir: &std::path::Path,
        item_specs: &[engram_core::types::snapshot::MigrationItem],
    ) -> Result<(), SandboxError> {
        use engram_core::types::snapshot::MigrationItem;
        let mut stream = source
            .migration_fetch(export_id, item_specs.to_vec())
            .await?;
        use futures::StreamExt;
        let mut items = ItemAssembler::new(item_specs.len());
        while let Some(frame) = stream.next().await {
            if let Some((idx, current)) = items.push(frame?)? {
                let name = match item_specs.get(idx as usize) {
                    Some(MigrationItem::StateBin) => "state.bin",
                    Some(MigrationItem::HotHint) => "hot-hint.json",
                    _ => {
                        return Err(SandboxError::Snapshot(
                            "fetch_postcopy_artifacts: unexpected item".into(),
                        ));
                    }
                };
                fs::write(dir.join(name), &current)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("write {name}: {e}")))?;
            }
        }
        items.finish()
    }

    /// ADR 0045: expired migration exports for the TTL sweep —
    /// `(sandbox, bound session, export_id)` per export past
    /// [`crate::migration::EXPORT_TTL`].
    pub fn expired_migration_exports(&self) -> Vec<(SandboxId, Option<SessionId>, String)> {
        self.migrations
            .expired()
            .into_iter()
            .filter_map(|sandbox_id| {
                let export_id = self.migrations.export_id_of(sandbox_id)?;
                let session = self.session_bindings.get(&sandbox_id).map(|e| *e);
                Some((sandbox_id, session, export_id))
            })
            .collect()
    }

    /// The session a sandbox is bound to, when known (populated by
    /// `notify_session_policy` / registration rehydration).
    pub fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId> {
        self.session_bindings.get(&id).map(|e| *e)
    }

    /// ADR 0090: repopulate a binding the local table missed (e.g. a
    /// pidfd-reattached survivor the registration ack did not list).
    /// Source of truth is the coordinator's `sandbox_owner` answer;
    /// recording it locally restores the reconciler's fast path without
    /// another RPC.
    pub fn record_session_binding(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.session_bindings.insert(sandbox_id, session_id);
    }

    /// ADR 0091: record a control-plane-dead guest (checkpoint driver's
    /// 3/3-probe verdict). Re-advertised every heartbeat until cleared.
    pub fn mark_guest_unreachable(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.unreachable_guests.insert(sandbox_id, session_id);
    }

    /// ADR 0091: a successful capture (or destroy) clears the suspicion.
    pub fn clear_guest_unreachable(&self, sandbox_id: SandboxId) {
        self.unreachable_guests.remove(&sandbox_id);
    }

    /// ADR 0091 probe gate (engrams review, #835): claim the one
    /// dead-guest-probe slot for `sandbox_id`. `true` = caller owns the
    /// probe and MUST call [`Self::end_dead_probe`] when it finishes;
    /// `false` = a probe is already in flight, skip spawning another.
    pub fn try_begin_dead_probe(&self, sandbox_id: SandboxId) -> bool {
        self.dead_probe_inflight.insert(sandbox_id, ()).is_none()
    }

    /// Release the probe slot claimed by [`Self::try_begin_dead_probe`].
    pub fn end_dead_probe(&self, sandbox_id: SandboxId) {
        self.dead_probe_inflight.remove(&sandbox_id);
    }

    /// ADR 0091: the heartbeat's unreachable-guest advert.
    pub fn unreachable_guests(&self) -> Vec<(SandboxId, SessionId)> {
        self.unreachable_guests
            .iter()
            .map(|e| (*e.key(), *e.value()))
            .collect()
    }

    pub fn checkpoint_records_dir(&self) -> Option<PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("records"))
    }

    /// Signalled after each record lands in `checkpoint_records_dir()`.
    pub fn checkpoint_recorded(&self) -> Arc<tokio::sync::Notify> {
        self.checkpoint_recorded.clone()
    }

    /// Test-only view of a sandbox's in-RAM chain head.
    #[cfg(test)]
    pub(crate) fn chain_head_for_test(
        &self,
        id: SandboxId,
    ) -> Option<engram_core::types::manifest::ManifestRef> {
        self.checkpoint_chains.get(&id).map(|c| c.manifest_ref)
    }

    /// Re-seed checkpoint chains for VMs that survived a host-agent
    /// restart (pidfd reattach, ADR 0044 K2 / ADR 0090), from the
    /// durable [`crate::checkpoint::ChainHeadRecord`]s — and GC records
    /// whose sandbox did NOT survive. Called once at startup, after the
    /// reattach pass and `set_self_ref`, BEFORE anything that can start
    /// a capture (checkpoint driver, eviction redrive, coordinator
    /// registration): a survivor's first post-roll capture then rides
    /// the O(dirty-set) diff path instead of a FULL multi-GiB re-chunk
    /// (the 2026-07-13 incident's 40-minute evict).
    ///
    /// Torn-capture safety is inherited from the record's write-ahead
    /// protocol (see the record's type doc): a record only exists if no
    /// FC snapshot create ran since the chain durably advanced, so
    /// seeding from it is exactly as sound as never having lost the
    /// DashMap.
    pub async fn rehydrate_chain_heads(&self) {
        let Some(store) = self.chain_heads.clone() else {
            return;
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return;
        };
        let live: std::collections::HashSet<SandboxId> = match self.inner.list().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(error = %e, "chain-head rehydrate: backend list failed; skipping");
                return;
            }
        };
        let dir = store.dir().to_path_buf();
        for record in crate::checkpoint::ChainHeadRecord::load_all(&dir).await {
            let id = record.sandbox_id;
            if !live.contains(&id) {
                // The sandbox didn't survive (node reboot, destroyed
                // while the record write raced teardown) — the record
                // is unreachable; sweep it.
                store.remove_best_effort(id);
                continue;
            }
            // Serialize against any capture already running for this
            // sandbox: a capture that won the lock first has already
            // write-ahead-removed the record, so the re-read below
            // no-ops — without the lock, "read record → capture
            // invalidates + creates (bitmap reset) → seed stale head"
            // would rebuild exactly the corrupt diff this protocol
            // exists to prevent.
            let _guard = self
                .capture_gate(id)
                .serialize(id, CaptureUser::ChainRehydrate)
                .await;
            if self.checkpoint_chains.contains_key(&id) {
                continue;
            }
            let Some(record) = crate::checkpoint::ChainHeadRecord::load(&dir, id).await else {
                continue;
            };
            match chunk_store.get_manifest(record.manifest_ref).await {
                Ok(manifest) => {
                    self.checkpoint_chains.insert(
                        id,
                        crate::checkpoint::CheckpointChain {
                            manifest_ref: record.manifest_ref,
                            manifest,
                        },
                    );
                    self.last_snapshot_unix_ms
                        .insert(id, record.updated_at.timestamp_millis());
                    tracing::info!(
                        sandbox_id = %id,
                        session_id = ?record.session_id,
                        manifest = %record.manifest_ref,
                        "chain head rehydrated from durable record; survivor's next capture will diff",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %id,
                        manifest = %record.manifest_ref,
                        error = %e,
                        "chain-head rehydrate: manifest fetch failed; next capture falls back to Full",
                    );
                }
            }
        }
    }

    /// ADR 0101 B: sandboxes due for a periodic checkpoint — each
    /// sandbox's due-interval comes from its last epoch's observed
    /// dirty rate ([`crate::checkpoint::next_epoch_after`]), clamped to
    /// `[cfg.min_interval, cfg.interval]`. A sandbox with no pacing
    /// sample yet (fresh bind, chain seed pending, Full-only history)
    /// keeps the max-interval backstop cadence. (The flat-interval
    /// predecessor is retired — this is the only candidacy surface.)
    pub fn checkpoint_candidates_adaptive(
        &self,
        cfg: &crate::checkpoint::CheckpointConfig,
    ) -> Vec<(SandboxId, SessionId)> {
        let Some(max_interval) = cfg.interval else {
            return Vec::new();
        };
        if self.checkpoint_dir.is_none() {
            return Vec::new();
        }
        let now_ms = self.clock.now_utc().timestamp_millis();
        self.session_bindings
            .iter()
            .filter_map(|e| {
                let id = *e.key();
                // A sandbox in a move is never a candidate: its role fences
                // captures.
                if self.migration_role(id).is_some() {
                    return None;
                }
                let last = self.last_snapshot_unix_ms.get(&id).map(|v| *v).unwrap_or(0);
                let due_after = match self.checkpoint_pacing.get(&id).map(|v| *v) {
                    Some(s) => crate::checkpoint::next_epoch_after(
                        s.epoch,
                        s.dirty_bytes,
                        cfg.min_interval,
                        max_interval,
                        cfg.target_epoch_bytes,
                    ),
                    None => cfg.first_interval.min(max_interval),
                };
                (now_ms - last >= due_after.as_millis() as i64).then_some((id, *e.value()))
            })
            .collect()
    }

    /// Whether a capture on this host has stamped this sandbox's
    /// last-checkpoint time.
    pub(crate) fn has_checkpoint_stamp(&self, id: SandboxId) -> bool {
        self.last_snapshot_unix_ms.contains_key(&id)
    }

    /// ADR 0028 Fix A: one periodic checkpoint — the same capture
    /// `snapshot()` runs for evictions (diff-flavored once the chain
    /// is seeded), self-committed because the durable record +
    /// heartbeat re-advertise own the reference, not a coord
    /// commit/abort pipeline.
    pub async fn checkpoint_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<SnapshotMetadata, SandboxError> {
        use engram_core::traits::SandboxBackend as _;
        let metadata = self
            .snapshot_with_class(
                id,
                // No session waits for a periodic checkpoint: its uploads
                // yield to every other transfer on the host.
                Class::Background,
                CaptureUser::PeriodicCheckpoint,
            )
            .await?;
        let _ = self.commit_snapshot(id).await;
        Ok(metadata)
    }

    /// Capture and upload under the capture lock, using the caller's upload class.
    pub(crate) async fn snapshot_with_class(
        &self,
        id: SandboxId,
        class: Class,
        user: CaptureUser,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let (_capture_guard, cap) = self.capture_phase(id, user).await?;
        // Lift the per-jail working-set trace into the blob store under the
        // session-canonical key so the next resume prefaults it (#517 keyed
        // the replay; the handler can't publish it itself — SIGKILLed on
        // destroy). Detached + best-effort; never blocks the capture.
        self.spawn_trace_publish(id);
        self.finisher_with_class(class).finish(id, cap).await
    }

    /// [`Self::with_fill`] with an engine of default limits.
    pub fn with_chunk_store(self, chunk_store: ChunkStore) -> Self {
        self.with_fill(FillEngine::new(chunk_store, engram_fill::Config::default()))
    }

    /// Attach the host's fill engine and the chunk store under it.
    pub fn with_fill(mut self, fill: FillEngine) -> Self {
        self.chunk_store = Some(fill.store().clone());
        self.fill = Some(fill);
        self
    }

    /// ADR 0080 §C: attach the image-materialize scratch root (the
    /// `MaterializeImage` RPC's working space; same volume as the
    /// chunk cache so the statvfs headroom check measures the disk
    /// that actually fills). Without it, `materialize_image` errors
    /// InvalidSpec — this host can't take enable-time materializes.
    pub fn with_materialize_scratch(mut self, dir: PathBuf) -> Self {
        self.materialize_scratch = Some(dir);
        self
    }

    /// Attach an NVMe-backed `ChunkCache`. Optional; chains on top
    /// of `with_chunk_store`. Production hosts wire one to
    /// amortise repeated chunk reads across manifests; dev /
    /// single-host setups can skip it without breaking the
    /// materialization path.
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// ADR 0111 + ADR 0121: replay the policies the previous process
    /// persisted beside its binding records into the node-local egress
    /// daemon, as ONE `SyncPolicies` full replace. Run after reattach
    /// (the live set is complete) and before coordinator registration,
    /// and again by the supervisor after a daemon respawn. Policies
    /// whose sandbox did not survive are skipped, and the sync's prune
    /// side drops anything the daemon still holds for them (an adopted
    /// daemon's registry can carry entries for VMs that died with the
    /// old pod). Recovery is a local read + one UDS round-trip: no
    /// coordinator involvement, no retries.
    ///
    /// A live capture registration (transient, never persisted) is
    /// pruned by this replace too; the capture's egress then fails and
    /// the capture job retries — captures are quiesced across shutdown
    /// anyway, so this only touches a daemon-crash-mid-capture edge.
    pub async fn rebuild_egress_from_policies(
        &self,
        policies: Vec<engram_core::types::egress::SessionEgressPolicy>,
    ) {
        use engram_core::traits::sandbox::SandboxBackend as _;
        let Some(egress) = self.egress.as_ref() else {
            return;
        };
        let live: std::collections::HashSet<SandboxId> =
            self.list().await.unwrap_or_default().into_iter().collect();
        let mut survivors = Vec::new();
        for policy in policies {
            if !live.contains(&policy.sandbox_id) {
                continue;
            }
            // The sandbox→session map the Phase B publisher resolver
            // reads must repopulate exactly as a live apply would.
            self.session_bindings
                .insert(policy.sandbox_id, policy.session_id);
            tracing::info!(
                session_id = %policy.session_id,
                sandbox_id = %policy.sandbox_id,
                "replaying persisted egress policy (ADR 0111)",
            );
            survivors.push(policy);
        }
        let count = survivors.len();
        match egress.sync_policies(survivors).await {
            Ok(()) => tracing::info!(count, "egress daemon synced from persisted policies"),
            Err(e) => tracing::warn!(
                count,
                error = %e,
                "egress daemon sync failed; survivors keep the daemon's current registry",
            ),
        }
    }

    /// Attach an image cache. Calling this after `new()` lets
    /// `create()` resolve `spec.image_uri` to a cached on-disk path
    /// before forwarding to the inner backend.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Attach the `OciClient` that `MaterializeImage` pulls with when
    /// the request carries no registry credentials. Without it, that
    /// pull is anonymous.
    pub fn with_oci_client(mut self, client: engram_oci::OciClient) -> Self {
        self.oci_client = Some(client);
        self
    }

    /// Attach a host-egress proxy. Once set, incoming
    /// `notify_session_policy` calls register against this proxy's
    /// registry and `destroy` unregisters. The CA PEM is available
    /// to substrate-builders via `host_egress.ca_cert_pem`.
    pub fn with_egress(mut self, egress: Arc<HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Make the memory manifest's chunks resident in the chunk cache at
    /// `class`. With `first`, every chunk that session touched last time
    /// (its first-touch prefix, then the chunks it touched after them) is
    /// fetched first, in touch order, at `Prefix`: a resume touches most of
    /// the previous life's later chunks before its own first run starts.
    /// Then the rest of the manifest, in manifest order, at `class`.
    /// Best-effort for the caller: a failure leaves the faults (or the
    /// serial materialize) to fetch on demand.
    async fn prefetch_memory_chunks(
        fill: &FillEngine,
        memory: Option<engram_core::types::manifest::ManifestRef>,
        class: Class,
        first: Option<SessionId>,
    ) -> Result<engram_fill::FillStats, SandboxError> {
        let Some(mref) = memory else {
            return Ok(engram_fill::FillStats::default());
        };
        let manifest =
            fill.store().get_manifest(mref).await.map_err(|e| {
                SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
            })?;
        if let Some(session) = first {
            let key = engram_chunk_store::FirstTouch::key(session.as_uuid());
            let rest_key = engram_chunk_store::FirstTouch::rest_key(session.as_uuid());
            let (mut touched, rest) = tokio::join!(
                first_touch_hashes(fill.store(), session, &key, &manifest),
                first_touch_hashes(fill.store(), session, &rest_key, &manifest),
            );
            touched.extend(rest);
            if !touched.is_empty() {
                match fill.fill(touched, Class::Prefix).await {
                    Ok(stats) => tracing::info!(?stats, "memory first-touch prefix resident"),
                    Err(e) => tracing::warn!(error = %e, "memory first-touch prefix failed"),
                }
            }
        }
        // `fill` skips what the prefix already made resident.
        let order = manifest.chunks.iter().map(|c| c.hash).collect();
        let stats = fill
            .fill(order, class)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("memory prefetch: {e}")))?;
        tracing::debug!(manifest = %mref, ?stats, "memory chunks resident");
        Ok(stats)
    }

    /// Write `<src>/memory.bin` from the `memory_manifest` in
    /// `<src>/manifest.json` when the file is absent (a cross-host File
    /// restore). Does nothing when there is no chunk store, no sidecar or
    /// no memory manifest. The VM waits for the file, so its chunks are
    /// read at `Demand`.
    async fn materialize_memory_if_missing(
        &self,
        src: &std::path::Path,
    ) -> Result<(), SandboxError> {
        let mem_path = src.join("memory.bin");
        if fs::metadata(&mem_path).await.is_ok() {
            return Ok(());
        }
        let Some(fill) = self.fill.as_ref() else {
            return Ok(());
        };
        let chunk_store = fill.store().clone().with_class(Class::Demand);
        let manifest_json = src.join("manifest.json");
        let Ok(bytes) = fs::read(&manifest_json).await else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Ok(());
        };
        let mref_value = value.get("memory_manifest").filter(|v| !v.is_null());
        let Some(mref_value) = mref_value else {
            return Ok(());
        };
        let mref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mref_value.clone()).map_err(|e| {
                SandboxError::Snapshot(format!(
                    "parse memory_manifest from {}: {e}",
                    manifest_json.display(),
                ))
            })?;
        // Ensure the destination dir exists (cross-host stages may
        // hand us a path that's missing intermediate components).
        if let Some(parent) = mem_path.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                SandboxError::Snapshot(format!("create snapshot dir {}: {e}", parent.display(),))
            })?;
        }
        let manifest = chunk_store
            .get_manifest(mref)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("get_manifest {mref}: {e}")))?;
        chunk_store
            .materialize_to_file_from(&manifest, &mem_path, fill)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("materialize memory.bin: {e}")))?;
        tracing::info!(
            manifest = %mref,
            path = %mem_path.display(),
            "materialised memory.bin from chunked memory manifest",
        );
        Ok(())
    }

    /// ADR 0014 issue #1/#2 + ADR 0028 addendum: tear down the LOCAL
    /// artifacts of a snapshot whose downstream pipeline failed (or
    /// whose successor `snapshot()` call is about to overwrite it).
    /// Removes only the per-snapshot local dir at
    /// `<work_dir>/sandboxes/snapshots/<snapshot_id>/` (4+ GiB on FC;
    /// 25 leaked dirs fill a 99 GB host) — host-disk hygiene, and
    /// rebuildable from
    /// BlobStorage on the next restore.
    ///
    /// It does NOT delete the per-snapshot BlobStorage objects
    /// (state.bin / sidecar.json / working_set.json) or the chunked
    /// manifests. Those are governed by the pin-set GC model
    /// (`coordinator::snapshot_blob_gc` for the portable blobs, the
    /// chunk GC for manifests): a blob is deleted only when no
    /// `snapshots` row references it, after a grace period. Deleting
    /// them inline here was the engine of the recurring
    /// "recoverable=true but blobs gone" brick — a concurrent producer
    /// (a periodic checkpoint vs an eviction sharing this one
    /// per-sandbox inflight slot) would abort a snapshot whose row was
    /// already recorded, deleting its durable blobs out from under a
    /// resume. Now no producer ever deletes a durable blob.
    ///
    /// Best-effort: a local-dir removal error is logged, never
    /// propagated. Clears the inflight tracking entry on entry so a
    /// double-call doesn't try to clean twice.
    async fn abort_prior_inflight_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let Some((_, snapshot_id)) = self.inflight_snapshots.remove(&id) else {
            return Ok(());
        };
        let dest = self.inner.snapshot_path_for(snapshot_id);
        match fs::remove_dir_all(&dest).await {
            Ok(()) => tracing::info!(
                sandbox_id = %id,
                snapshot_id = %snapshot_id,
                dest = %dest.display(),
                "abort_snapshot: removed local snapshot dir (blobs are GC-governed)",
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                sandbox_id = %id,
                snapshot_id = %snapshot_id,
                dest = %dest.display(),
                error = %e,
                "abort_snapshot: rm -rf of snapshot dir failed (best-effort)",
            ),
        }
        Ok(())
    }
}

/// Write a copy of each manifest in `refs` beside the snapshot in `dir`,
/// where the UFFD handler reads it instead of the blob store. A manifest
/// that is not staged is read from the store, so a failure here only warns.
async fn stage_memory_manifests(
    fill: FillEngine,
    dir: PathBuf,
    sandbox_id: SandboxId,
    refs: Vec<engram_core::types::manifest::ManifestRef>,
) {
    let stage = |r| {
        let (fill, dir) = (fill.clone(), dir.clone());
        async move {
            let manifest = fill
                .store()
                .get_manifest(r)
                .await
                .map_err(|e| e.to_string())?;
            let bytes = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
            let path = engram_sandbox_firecracker::staged_manifest_path(&dir, sandbox_id, &r);
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, bytes).await.map_err(|e| e.to_string())?;
            fs::rename(&tmp, &path).await.map_err(|e| e.to_string())
        }
    };
    for (r, result) in refs
        .iter()
        .zip(futures::future::join_all(refs.iter().map(|r| stage(*r))).await)
    {
        if let Err(error) = result {
            tracing::warn!(manifest = %r, %error, "manifest copy not staged; the handler reads the store");
        }
    }
}

/// Wait for the staging task of a restore, then remove its manifest
/// copies. The wait comes first: a blocking file write outlives an abort.
/// A lazy restore has already awaited the task, and a `JoinHandle` must
/// not be polled again after it completes.
async fn remove_staged_manifests(
    staging: Option<AbortTaskOnDrop>,
    dir: &Path,
    sandbox_id: SandboxId,
    refs: &[engram_core::types::manifest::ManifestRef],
) {
    let Some(mut staging) = staging else {
        return;
    };
    if !staging.0.is_finished() {
        let _ = (&mut staging.0).await;
    }
    for r in refs {
        let path = engram_sandbox_firecracker::staged_manifest_path(dir, sandbox_id, r);
        let _ = fs::remove_file(path.with_extension("json.tmp")).await;
        let _ = fs::remove_file(path).await;
    }
}

/// ADR 0014 cross-host restore helper: download `state.bin` and the
/// FC sidecar JSON from `BlobStorage` into the snapshot staging
/// directory iff (a) the metadata carries portable blob keys and
/// (b) the local files aren't already there. Idempotent — same-
/// host restore is a pair of `fs::metadata` short-circuits.
async fn materialize_state_if_missing(
    blob: &dyn engram_core::traits::storage::BlobStorage,
    src: &std::path::Path,
    state_blob_key: Option<&str>,
    sidecar_blob_key: Option<&str>,
) -> Result<(), SandboxError> {
    // The dest paths mirror the inner FC backend's snapshot layout
    // (`<snapshot_dir>/{state.bin,manifest.json}`). Cross-host
    // restore may hand us a path whose parent doesn't exist yet.
    fs::create_dir_all(src).await.map_err(|e| {
        SandboxError::Snapshot(format!("create snapshot dir {}: {e}", src.display()))
    })?;
    // ADR 0101 A: the two artifacts are independent blob objects — fetch
    // concurrently (a cold cross-host resume previously paid the two
    // downloads back-to-back on its critical path).
    let state_path = src.join("state.bin");
    let sidecar_path = src.join("manifest.json");
    let state_fut = async {
        if let Some(key) = state_blob_key {
            if fs::metadata(&state_path).await.is_err() {
                engram_chunk_store::snapshot_blob::download_file(blob, key, &state_path)
                    .await
                    .map_err(|e| {
                        SandboxError::Snapshot(format!("download state.bin from {key}: {e}"))
                    })?;
                tracing::info!(
                    path = %state_path.display(),
                    key = key,
                    "materialised state.bin from BlobStorage",
                );
            }
        }
        Ok::<(), SandboxError>(())
    };
    let sidecar_fut = async {
        if let Some(key) = sidecar_blob_key {
            if fs::metadata(&sidecar_path).await.is_err() {
                engram_chunk_store::snapshot_blob::download_file(blob, key, &sidecar_path)
                    .await
                    .map_err(|e| {
                        SandboxError::Snapshot(format!("download sidecar.json from {key}: {e}"))
                    })?;
                tracing::info!(
                    path = %sidecar_path.display(),
                    key = key,
                    "materialised sidecar.json from BlobStorage",
                );
            }
        }
        Ok::<(), SandboxError>(())
    };
    let (state_res, sidecar_res) = tokio::join!(state_fut, sidecar_fut);
    state_res?;
    sidecar_res?;
    Ok(())
}

/// ADR 0008 Phase 5 final piece: ensure the disk `Manifest`
/// referenced by `cached.bundle.disk_manifest` is reachable in
/// the host's `BlobStorage`. For chunked-OCI images, the bake-
/// side wrote the Manifest to its own BlobStorage namespace; the
/// runtime host's namespace may be empty. We synthesize from the
/// bootstrap sidecar (which arrived with the OCI pull) and
/// persist via `put_manifest`. Idempotent — skips when the
/// manifest is already in BlobStorage, or when the artifact is
/// not chunked-OCI shaped (legacy bundle.json / no bundle).
///
/// Called from `create` before the image's manifest seeds a session
/// volume.
pub async fn ensure_chunked_manifest_in_blob_storage(
    chunk_store: &ChunkStore,
    cached: &CachedImage,
) -> Result<(), SandboxError> {
    // Three short-circuits — non-chunked-OCI images don't need
    // this path at all.
    let Some(bundle) = cached.bundle.as_ref() else {
        return Ok(());
    };
    let Some(bs_path) = cached.disk_bootstrap_path.as_ref() else {
        return Ok(());
    };
    let manifest_ref = bundle.disk_manifest;

    // Already there? Most common case after the first session on a
    // given image: cheap stat-shape lookup, no-op.
    match chunk_store.get_manifest(manifest_ref).await {
        Ok(_) => return Ok(()),
        Err(engram_chunk_store::ChunkStoreError::Blob(engram_core::error::BlobError::NotFound)) => {
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: probing for existing manifest hit unexpected error",
            );
            return Err(SandboxError::Vm(
                format!("probe manifest {manifest_ref}: {e}").into(),
            ));
        }
    }

    // Synthesize from the bootstrap on disk and persist. The
    // bootstrap was written by `pull_image` (`bootstrap.disk.json`
    // sidecar); it carries every byte of Manifest info we need.
    let bytes = fs::read(bs_path).await.map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: read bootstrap failed");
        SandboxError::Vm(format!("read bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: parse bootstrap failed");
        SandboxError::Vm(format!("parse bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let synthesized = bootstrap.to_manifest();
    tracing::info!(
        manifest_ref = %manifest_ref,
        chunks = synthesized.chunks.len(),
        total_bytes = synthesized.total_bytes,
        kind = ?synthesized.kind,
        "chunked-OCI: synthesizing manifest from bootstrap + persisting to BlobStorage",
    );
    match chunk_store.put_manifest(manifest_ref, &synthesized).await {
        Ok(_) => Ok(()),
        // VersionConflict means another concurrent caller raced us
        // to put the same manifest. Synthesis is deterministic
        // (same bootstrap → same chunk list at same offsets), so
        // their version is identical to ours. Treat as success.
        Err(engram_chunk_store::ChunkStoreError::VersionConflict { .. }) => {
            tracing::debug!(
                manifest_ref = %manifest_ref,
                "chunked-OCI: concurrent put_manifest beat us; using their copy",
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: put_manifest of synthesized manifest failed",
            );
            Err(SandboxError::Vm(
                format!("persist synthesized manifest {manifest_ref}: {e}").into(),
            ))
        }
    }
}

/// What `capture_phase` hands the post phase.
pub(crate) struct SnapshotCapture {
    metadata: SnapshotMetadata,
    dest: PathBuf,
    chain_prev: Option<(
        engram_core::types::manifest::ManifestRef,
        engram_chunk_store::Manifest,
    )>,
    paused_at: chrono::DateTime<chrono::Utc>,
    /// The backend's captures write guest memory and its restore files
    /// ([`SandboxBackend::captures_guest_memory`]).
    writes_memory: bool,
    /// Owns the pending disk checkpoints and resumes the guest if this
    /// capture is dropped before the finisher takes it over.
    unwind: CaptureUnwind,
}

/// Incident 2026-07-10: FC's `PUT /snapshot/create` (Diff) consumes —
/// and resets — the KVM dirty-page bitmap the moment it runs. If
/// anything after that point fails (the create itself mid-write, the
/// sparse re-chunk, the eviction finalize-record persist), the consumed
/// dirty set is GONE: the chain's manifest never advanced, so a
/// subsequent Diff against the stale chain entry would silently exclude
/// every page the failed capture consumed — and a guest restored from
/// that chain reads pre-capture bytes at those pages (memory
/// corruption). Drop the chain entry instead: the next capture finds no
/// chain and takes a FULL snapshot — slower, correct. (The eviction
/// flavor is immune only AFTER its finalize record is durable; before
/// that instant it has the same hole, hence the `snapshot_begin` call
/// sites.)
fn poison_checkpoint_chain_after_failed_diff(
    chains: &DashMap<SandboxId, crate::checkpoint::CheckpointChain>,
    chain_heads: Option<&crate::checkpoint::ChainHeadStore>,
    id: SandboxId,
    failed_step: &str,
) {
    // The durable chain-head record was already write-ahead-removed
    // before the FC create (capture_phase / migration_capture_postcopy), so this
    // is the defensive double-unlink — the poison must never leave a
    // record a post-roll rehydrate could seed from. (It also bumps the
    // store epoch, fencing any straggling persist.)
    if let Some(store) = chain_heads {
        store.remove_best_effort(id);
    }
    if chains.remove(&id).is_some() {
        metrics::counter!(crate::metrics::CHECKPOINT_CHAIN_POISONED_TOTAL).increment(1);
        tracing::warn!(
            sandbox_id = %id,
            failed_step,
            "diff capture failed after FC consumed the dirty bitmap; \
             checkpoint chain dropped — next capture will be a FULL snapshot",
        );
    }
}

/// ADR 0088 addendum: a snapshot whose `capture_phase` completed
/// synchronously but whose `finish()` runs on a spawned task — see
/// [`PooledBackend::snapshot_deferred`] for the contract (always
/// `join()`, never drop).
pub(crate) struct DeferredSnapshot {
    handle: tokio::task::JoinHandle<Result<SnapshotMetadata, SandboxError>>,
}

impl DeferredSnapshot {
    pub(crate) async fn join(self) -> Result<SnapshotMetadata, SandboxError> {
        match self.handle.await {
            Ok(result) => result,
            Err(e) => Err(SandboxError::Snapshot(format!(
                "deferred snapshot finish task died: {e}"
            ))),
        }
    }
}

/// Arc-clones of the fields the snapshot post phase and the chain
/// bookkeeping touch, so the phase can run detached from the RPC.
#[derive(Clone)]
pub(crate) struct SnapshotFinisher {
    volumes: Option<Arc<SessionVolumes>>,
    chunks: Option<crate::snapshot_post::Chunks>,
    chunk_cache: Option<ChunkCache>,
    bundle_dir: PathBuf,
    /// The backend's staged-bundle extension, so `publish` opens the
    /// same staged filename the backend attaches.
    bundle_file_ext: &'static str,
    inflight_snapshots: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    last_snapshot_unix_ms: Arc<DashMap<SandboxId, i64>>,
    checkpoint_pacing: Arc<DashMap<SandboxId, EpochPacingSample>>,
    checkpoint_chains: Arc<DashMap<SandboxId, crate::checkpoint::CheckpointChain>>,
    checkpoint_dir: Option<PathBuf>,
    checkpoint_recorded: Arc<tokio::sync::Notify>,
    chain_heads: Option<Arc<crate::checkpoint::ChainHeadStore>>,
    session_bindings: Arc<DashMap<SandboxId, SessionId>>,
    clock: Arc<dyn engram_core::traits::Clock>,
    host_fs: Arc<dyn engram_host_core::HostFs>,
}

/// Where a sandbox's checkpoint chain starts.
#[derive(Clone, Copy)]
pub(crate) enum ChainSeed<'a> {
    /// A durable memory manifest that this sandbox's memory equals: the
    /// resumed snapshot's, or the one a Full capture just published.
    Manifest(engram_core::types::manifest::ManifestRef),
    /// A shared manifest (a create's image base), published again under a
    /// new lineage that only this sandbox writes. Without it, each session's
    /// first diff would race to publish the next version of the shared id.
    Fork(engram_core::types::manifest::ManifestRef),
    /// A post-copy destination: the source's chain manifest with the pages
    /// the peer supplied (`sealed`) written over it, under a new lineage.
    PostCopy {
        source: engram_core::types::manifest::ManifestRef,
        sealed: &'a Path,
    },
}

impl ChainSeed<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Manifest(_) => "manifest",
            Self::Fork(_) => "fork",
            Self::PostCopy { .. } => "post_copy",
        }
    }
}

/// ADR 0101 B: what the last capture observed about a sandbox's memory
/// dirty rate — the adaptive checkpoint controller's only input.
/// `dirty_bytes: None` means the capture carried no rate signal (a Full
/// capture, or a diff-less flavor) — the controller then falls back to
/// the max-interval backstop.
#[derive(Clone, Copy, Debug)]
pub struct EpochPacingSample {
    pub dirty_bytes: Option<u64>,
    pub epoch: std::time::Duration,
}

impl SnapshotFinisher {
    fn post_phase(&self) -> crate::snapshot_post::PostPhase<'_> {
        crate::snapshot_post::PostPhase {
            volumes: self.volumes.as_deref(),
            chunks: self.chunks.as_ref(),
            chunk_cache: self.chunk_cache.as_ref(),
            bundle_dir: &self.bundle_dir,
            bundle_file_ext: self.bundle_file_ext,
            fs: self.host_fs.as_ref(),
        }
    }

    /// ADR 0045 D5: the shared post phase ([`crate::snapshot_post`]),
    /// then the chain bookkeeping. On failure the staging dir is removed:
    /// an eviction retried every ~30 s would otherwise leak a guest's
    /// memory size of disk per try.
    #[tracing::instrument(name = "host.snapshot_finish", skip_all, fields(sandbox_id = %id))]
    pub(crate) async fn finish(
        &self,
        id: SandboxId,
        cap: SnapshotCapture,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let finish_start = crate::time_source::metrics_now();
        let SnapshotCapture {
            mut metadata,
            dest,
            chain_prev,
            paused_at,
            writes_memory,
            mut unwind,
        } = cap;
        let flavor = if chain_prev.is_some() { "diff" } else { "full" };
        // The finisher owns the capture from here: the pending disk
        // checkpoints leave the guard and the guard must not resume.
        let pending = std::mem::take(&mut unwind.pending);
        unwind.defuse();
        drop(unwind);
        // The exact pause instant: the coordinator's eviction path resolves
        // the session_events coherence cursor from it.
        metadata.paused_at = Some(paused_at);
        let had_chain = chain_prev.is_some();
        // Only a chain-seeded backend takes a diff, so a diff always has
        // memory.
        let memory = match chain_prev {
            Some((prev_ref, prev)) => Some(crate::snapshot_post::Memory::Diff {
                prev_ref,
                prev: Some(prev),
                extents: None,
            }),
            None => writes_memory.then_some(crate::snapshot_post::Memory::Full),
        };
        let blobs = memory.is_some();
        let aux_bundles = metadata.aux_bundles.clone();
        let phase = self.post_phase();
        let post = phase.run(crate::snapshot_post::Work {
            snapshot_id: metadata.id,
            dest: &dest,
            disks: Some(crate::snapshot_post::Disks::Held(pending)),
            memory,
            blobs,
            trace_lineage: self.session_bindings.get(&id).map(|e| *e),
            aux_bundles: &aux_bundles,
            source: "snapshot_finish",
        });
        // The capture files reach the disk before the record names this
        // snapshot: `state.bin` here, the sidecar in the post phase. A
        // consumed memory file is retired durably, as the chunk store
        // holds its copy; one the phase did not consume stays the restore
        // input, so it is synced.
        let sync = crate::snapshot_post::sync_capture(
            self.host_fs.as_ref(),
            &dest,
            crate::snapshot_post::SyncScope::State,
        );
        let (post, sync) = tokio::join!(post, sync);
        let post = match (post, sync) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(SandboxError::Snapshot(format!("sync capture files: {e}"))),
            (Ok(published), Ok(())) => match &published.memory {
                Some(memory) => crate::snapshot_post::retire_consumed(
                    self.host_fs.clone(),
                    memory.consumed.clone(),
                )
                .await
                .map(|()| published)
                .map_err(|e| SandboxError::Snapshot(format!("retire memory file: {e}"))),
                None => crate::snapshot_post::sync_capture(
                    self.host_fs.as_ref(),
                    &dest,
                    crate::snapshot_post::SyncScope::All,
                )
                .await
                .map(|()| published)
                .map_err(|e| SandboxError::Snapshot(format!("sync capture files: {e}"))),
            },
        };

        let result = match post {
            Ok(published) => {
                metadata.disk_manifest = published.root;
                metadata.swap_manifest = published.swap;
                if let Some(keys) = published.blobs {
                    metadata.state_blob_key = Some(keys.state);
                    metadata.sidecar_blob_key = Some(keys.sidecar);
                    metadata.source_sandbox_id = Some(id);
                }
                // ADR 0101 B: the diff's dirty bytes are the epoch's rate
                // signal; a Full capture carries none.
                let mut epoch_dirty_bytes = None;
                let mut next_manifest_for_chain = None;
                if let Some(memory) = published.memory {
                    metadata.memory_manifest = Some(memory.manifest_ref);
                    epoch_dirty_bytes = memory.dirty_bytes;
                    next_manifest_for_chain = memory.next;
                }
                // ADR 0014 issue #1/#2: record the snapshot_id so a
                // future commit/abort RPC can clean up the per-snapshot
                // artifacts even though the caller only knows the
                // sandbox_id. Also lets a retry of this same sandbox's
                // snapshot() find and tear down the prior attempt.
                self.inflight_snapshots.insert(id, metadata.id);
                // Stamped after post-processing succeeded: a snapshot
                // whose state.bin / sidecar upload failed is not a
                // durable capture and must not advance the pacing epoch.
                let now_ms = self.clock.now_utc().timestamp_millis();
                let prev_ms = self.last_snapshot_unix_ms.insert(id, now_ms);
                // ADR 0101 B: pacing sample for the adaptive checkpoint
                // controller — the epoch is the gap since the previous
                // successful capture (any flavor; that's what the dirty
                // set accumulated over). Without a previous stamp there
                // is no epoch and no sample: the controller keeps the
                // first epoch.
                if let Some(prev_ms) = prev_ms.filter(|p| *p > 0 && *p <= now_ms) {
                    let epoch = std::time::Duration::from_millis((now_ms - prev_ms) as u64);
                    self.checkpoint_pacing.insert(
                        id,
                        EpochPacingSample {
                            dirty_bytes: epoch_dirty_bytes,
                            epoch,
                        },
                    );
                    if let Some(dirty) = epoch_dirty_bytes {
                        metrics::histogram!(crate::metrics::CHECKPOINT_EPOCH_BYTES)
                            .record(dirty as f64);
                        metrics::histogram!(crate::metrics::CHECKPOINT_EPOCH_SECONDS)
                            .record(epoch.as_secs_f64());
                    }
                }
                // ADR 0028 Fix A: seed/advance the rolling chain +
                // persist the durable host-owned record. Best-effort
                // beyond the capture: a seed failure means the next
                // capture is Full again; a record failure means only
                // PG (if the caller's pipeline survives) knows this
                // checkpoint. Both safe, both logged inside.
                self.advance_checkpoint_state(id, &metadata, paused_at, next_manifest_for_chain)
                    .instrument(tracing::info_span!("finish.record"))
                    .await;
                Ok(metadata)
            }
            Err(e) => {
                // The FC Diff already ran (capture_phase succeeded), so
                // the dirty bitmap is consumed and the memory.diff we're
                // about to delete is its only record — a later Diff off
                // the unadvanced chain would silently miss these pages.
                // Poison the chain so the next capture is a Full.
                if had_chain {
                    poison_checkpoint_chain_after_failed_diff(
                        &self.checkpoint_chains,
                        self.chain_heads.as_deref(),
                        id,
                        "snapshot post-processing",
                    );
                }
                // ADR 0014 cleanup hygiene: rm -rf the FC-written
                // snapshot dir before propagating. Idle-evict retry
                // (every ~30s) without this leaks 4 GiB per try and
                // fills the host disk inside an hour.
                match tokio::fs::remove_dir_all(&dest).await {
                    Ok(_) => tracing::warn!(
                        sandbox_id = %id,
                        dest = %dest.display(),
                        error = %e,
                        "PooledBackend::snapshot post-inner failed; orphan dir cleaned",
                    ),
                    Err(rm_err) => tracing::warn!(
                        sandbox_id = %id,
                        dest = %dest.display(),
                        snapshot_error = %e,
                        rm_error = %rm_err,
                        "PooledBackend::snapshot post-inner failed; rm -rf of orphan dir also failed",
                    ),
                }
                Err(e)
            }
        };
        metrics::histogram!(
            crate::metrics::SNAPSHOT_FINISH_SECONDS,
            "type" => flavor,
            "outcome" => if result.is_ok() { "success" } else { "error" },
        )
        .record(finish_start.elapsed().as_secs_f64());
        result
    }
    fn checkpoint_records_dir(&self) -> Option<PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("records"))
    }

    /// Durably mirror the in-RAM chain head at `manifest_ref` (which
    /// MUST already be published in the chunk store — every caller sits
    /// after a successful `put_manifest` or a fetch of an existing
    /// manifest). Best-effort: a failed write just means a post-roll
    /// rehydrate finds no record and the survivor's next capture is a
    /// Full — safe, slower, and self-healing at the next checkpoint.
    /// Routed through the [`crate::checkpoint::ChainHeadStore`] so a
    /// cancelled write's detached tail can never outlive a later
    /// invalidate.
    async fn persist_chain_head(
        &self,
        id: SandboxId,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) {
        let Some(store) = &self.chain_heads else {
            return;
        };
        let record = crate::checkpoint::ChainHeadRecord {
            sandbox_id: id,
            manifest_ref,
            session_id: self.session_bindings.get(&id).map(|s| *s),
            updated_at: self.clock.now_utc(),
        };
        if let Err(e) = store.persist(record).await {
            tracing::warn!(
                sandbox_id = %id,
                manifest = %manifest_ref,
                error = %e,
                "durable chain-head write failed; a pod roll before the next \
                 checkpoint costs this sandbox one Full capture",
            );
        }
    }

    async fn advance_checkpoint_state(
        &self,
        id: SandboxId,
        metadata: &SnapshotMetadata,
        paused_at: chrono::DateTime<chrono::Utc>,
        next_manifest: Option<engram_chunk_store::Manifest>,
    ) {
        if self.checkpoint_dir.is_none() {
            return;
        }
        let Some(memory_ref) = metadata.memory_manifest else {
            // No chunked memory (no chunk store / non-FC backend):
            // nothing to chain.
            return;
        };

        match next_manifest {
            // Diff capture: the sparse re-chunk already ran in the post
            // block; just advance the chain's manifest pointer, then
            // re-commit the durable chain-head record the capture's
            // write-ahead invalidate removed.
            Some(next) => {
                if let Some(mut chain) = self.checkpoint_chains.get_mut(&id) {
                    chain.manifest_ref = memory_ref;
                    chain.manifest = next;
                }
                self.persist_chain_head(id, memory_ref).await;
            }
            // Full capture: seed the chain manifest-only from the manifest
            // we just published (ADR 0039 — no local rolling image; the
            // memory.bin was chunked + removed in the post block).
            // Subsequent captures ride the sparse diff path. (The seed
            // persists the chain-head record itself.)
            None => {
                let _ = self.seed_chain(id, ChainSeed::Manifest(memory_ref)).await;
            }
        }

        // Durable record — only for session-bound sandboxes (anonymous
        // base-snapshot captures have no session to reconcile).
        let Some(session_id) = self.session_bindings.get(&id).map(|s| *s) else {
            return;
        };
        let Some(records_dir) = self.checkpoint_records_dir() else {
            return;
        };
        let record = crate::checkpoint::CheckpointRecord {
            snapshot_id: metadata.id,
            session_id,
            sandbox_id: id,
            image_version: metadata.image_version.clone(),
            size_bytes: metadata.size_bytes,
            swap_manifest: metadata.swap_manifest,
            disk_manifest: metadata.disk_manifest,
            memory_manifest: metadata.memory_manifest,
            aux_bundles: metadata.aux_bundles.clone(),
            paused_at,
            captured_at: metadata.created_at,
            // This is `advance_checkpoint_state` — the composed
            // `snapshot()`/periodic-checkpoint path. The eviction flavor's
            // OWN terminal record write (`run_eviction_finalize`) stamps
            // `EvictionFinal` explicitly; this call site never runs for it
            // (it uses `snapshot_begin`, not `snapshot()`).
            kind: engram_protocol::heartbeat::CheckpointKind::Periodic,
        };
        match record
            .persist(&engram_host_core::TokioFs, &records_dir)
            .await
        {
            Ok(()) => self.checkpoint_recorded.notify_one(),
            Err(e) => tracing::warn!(
                sandbox_id = %id,
                snapshot_id = %metadata.id,
                error = %e,
                "durable checkpoint record write failed; PG row (if the caller's \
                 pipeline survives) is the only reference",
            ),
        }
    }

    /// A seeded chain means the memory matches a durable manifest now, so
    /// the next checkpoint is not due yet. A capture's own stamp wins.
    fn stamp_seeded(&self, id: SandboxId) {
        let now_ms = self.clock.now_utc().timestamp_millis();
        self.last_snapshot_unix_ms.entry(id).or_insert(now_ms);
    }

    /// Start the checkpoint chain of `id` from `seed`, so the next
    /// checkpoint diffs against a durable manifest instead of reading all
    /// of guest memory (a Full capture). Returns the chain head. Refuses
    /// when checkpoints are off or a chain exists; the caller's capture or
    /// restore does not fail on an error, the next checkpoint is Full.
    pub(crate) async fn seed_chain(
        &self,
        id: SandboxId,
        seed: ChainSeed<'_>,
    ) -> Result<engram_core::types::manifest::ManifestRef, String> {
        let kind = seed.kind();
        let result = self
            .seed_chain_inner(id, seed)
            .instrument(tracing::info_span!("checkpoint.seed_chain", kind))
            .await;
        match &result {
            Ok(head) => tracing::info!(sandbox_id = %id, kind, %head, "checkpoint chain seeded"),
            Err(error) => tracing::warn!(
                sandbox_id = %id,
                kind,
                %error,
                "checkpoint chain seed failed; the first checkpoint is Full",
            ),
        }
        result
    }

    async fn seed_chain_inner(
        &self,
        id: SandboxId,
        seed: ChainSeed<'_>,
    ) -> Result<engram_core::types::manifest::ManifestRef, String> {
        use engram_core::types::manifest::ManifestRef;
        let Some(crate::snapshot_post::Chunks { store, fill }) = self.chunks.as_ref() else {
            return Err("no chunk store".into());
        };
        if self.checkpoint_dir.is_none() {
            return Err("checkpoints are off".into());
        }
        if self.checkpoint_chains.contains_key(&id) {
            return Err("a chain already exists".into());
        }
        let get = |r: ManifestRef| async move {
            store
                .get_manifest(r)
                .await
                .map_err(|e| format!("manifest {r}: {e}"))
        };
        let (head, manifest) = match seed {
            ChainSeed::Manifest(r) => (r, get(r).await?),
            ChainSeed::Fork(r) => (ManifestRef::new(), get(r).await?),
            ChainSeed::PostCopy { source, sealed } => {
                let path = sealed.to_path_buf();
                let ranges = tokio::task::spawn_blocking(move || {
                    std::fs::File::open(&path).and_then(|f| engram_host_core::data_extents(&f))
                })
                .await
                .map_err(|e| format!("sealed ranges join: {e}"))?
                .map_err(|e| format!("sealed ranges: {e}"))?;
                let manifest = store
                    .update_for_dirty_ranges_sparse_from(
                        &get(source).await?,
                        sealed,
                        &ranges,
                        Arc::new(fill.clone()),
                    )
                    .await
                    .map_err(|e| format!("re-chunk sealed chunks: {e}"))?;
                (ManifestRef::new(), manifest)
            }
        };
        if !matches!(seed, ChainSeed::Manifest(_)) {
            store
                .put_manifest(head, &manifest)
                .await
                .map_err(|e| format!("put manifest {head}: {e}"))?;
        }
        self.checkpoint_chains.insert(
            id,
            crate::checkpoint::CheckpointChain {
                manifest_ref: head,
                manifest,
            },
        );
        self.persist_chain_head(id, head).await;
        self.stamp_seeded(id);
        Ok(head)
    }
}

/// ADR 0019 / telemetry restoration (#526): the on-disk shape of the
/// uffd-handler's `prefault-stats.json` (`PrefaultStats` in
/// `engram-uffd-handler::runtime`), duck-typed here rather than shared
/// via a cross-crate dependency — the file format is the contract
/// between the two binaries, not a Rust type (the writer is Linux-only
/// internally; this reader must build on every host-agent target).
/// `#[serde(default)]` on the count fields means a stats file that only
/// carries `trace_loaded` (shouldn't happen from this repo's writer,
/// but keeps a future schema-superset — e.g. `prefault-admission-
/// control`'s convergent gate file — from breaking this reader) still
/// parses.
#[derive(Debug, serde::Deserialize)]
struct PrefaultStatsFile {
    trace_loaded: bool,
    #[serde(default)]
    installed: usize,
    #[serde(default)]
    skipped: usize,
}

/// Review finding 6: the peer-fill fields `PrefaultStats` carries for
/// post-copy migration destinations (issue step (d)3 — "uffd-handler
/// live peer page-serves are recorded in the same per-jail stats file").
/// Parsed independently of `PrefaultStatsFile`/`classify_prefault_outcome`
/// (which stay focused on the `engram_resume_prefault_*` counter
/// contract) — these are **span attributes only for now**, not a new
/// counter family; `epic-gcs-free-resume` owns promoting them. Every
/// field defaults to `0` — both for a genuinely non-peer resume (the
/// fields are simply absent from the JSON) and for a peer resume whose
/// drain happened to move nothing, so this is only recorded on the span
/// when at least one field is nonzero (see `restore()`).
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
struct PeerFillSnapshot {
    #[serde(default)]
    peer_pulled: u64,
    #[serde(default)]
    peer_alt_sourced: u64,
    #[serde(default)]
    peer_zero_chunks: u64,
    #[serde(default)]
    peer_live_faults: u64,
}

impl PeerFillSnapshot {
    fn is_nonzero(&self) -> bool {
        self.peer_pulled != 0
            || self.peer_alt_sourced != 0
            || self.peer_zero_chunks != 0
            || self.peer_live_faults != 0
    }
}

fn parse_peer_fill_snapshot(stats_bytes: Option<&[u8]>) -> Option<PeerFillSnapshot> {
    serde_json::from_slice(stats_bytes?).ok()
}

/// The three effectiveness outcomes `engram_resume_prefault_total` is
/// labeled with. See the module doc on [`crate::metrics::RESUME_PREFAULT_TOTAL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefaultOutcome {
    /// A trace was requested + loaded, and the prefault ran.
    /// `installed == 0` is itself worth alarming on downstream (a
    /// stale/mismatched trace) but is still a "replayed" outcome, not
    /// an error — the counter split lives in `RESUME_PREFAULT_CHUNKS_TOTAL`.
    Replayed { installed: usize, skipped: usize },
    /// No trace was requested/loaded for this resume (base image,
    /// migration dest with no prior life, or the prior life's publish
    /// never landed) — expected, not an error.
    NoTrace,
    /// A stats file was expected (this backend has a
    /// `prefault_stats_path`) but wasn't there — the alarm condition.
    /// This is the exact class of bug that went inert 3x silently
    /// (d0e5ecf3, cf6e4d32, 7c2a7226): the handler died, or the wiring
    /// silently didn't fire, and nothing said so.
    StatsMissing,
}

impl PrefaultOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Replayed { .. } => "replayed",
            Self::NoTrace => "no_trace",
            Self::StatsMissing => "stats_missing",
        }
    }
}

/// Review finding 1: how long / how often to retry the prefault-stats
/// read before conceding `stats_missing`. The write races a background
/// thread whose fetch duration is unbounded in principle (chunk count x
/// NVMe/GCS latency) but "seconds" in the evidence pass's worst observed
/// case (migration-dest: drain-then-trace-prefault). 20s at a 250ms
/// cadence is generous relative to that without holding the poll open
/// indefinitely on a genuinely dead handler; this loop is spawned
/// off `restore()`'s return path, so it never adds latency to the
/// resume itself.
const PREFAULT_STATS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const PREFAULT_STATS_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Poll for the uffd-handler's `prefault-stats.json` until it appears or
/// [`PREFAULT_STATS_POLL_TIMEOUT`] elapses. `prefault_from_trace` writes
/// the file exactly once, atomically (temp+rename), at the END of its
/// background fetch — so there is no "partial write" state to worry
/// about: the file is either not there yet (keep polling) or fully
/// there (done). Returns `None` if the timeout is reached without ever
/// seeing the file — the same `stats_missing` alarm as before, but now
/// only after a generous wait instead of on the first instant.
async fn read_prefault_stats_with_retry(stats_path: &std::path::Path) -> Option<Vec<u8>> {
    read_prefault_stats_with_retry_bounded(
        stats_path,
        PREFAULT_STATS_POLL_INTERVAL,
        PREFAULT_STATS_POLL_TIMEOUT,
    )
    .await
}

/// Parameterized half of [`read_prefault_stats_with_retry`], split out so
/// tests can exercise the "appears mid-poll" and "never appears" cases
/// with millisecond bounds instead of the real 20s production timeout.
async fn read_prefault_stats_with_retry_bounded(
    stats_path: &std::path::Path,
    interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    let deadline = crate::time_source::metrics_now_tokio() + timeout;
    loop {
        if let Ok(bytes) = fs::read(stats_path).await {
            return Some(bytes);
        }
        if crate::time_source::metrics_now_tokio() >= deadline {
            return None;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Pure stats-file-bytes → outcome mapping (the testable half of the
/// read). `None` input means the file didn't exist / couldn't be read
/// (both collapse to `StatsMissing` — a read error other than "not
/// found" is just as much an alarm as absence). A corrupt/unparseable
/// file (the atomic temp+rename write should prevent partial reads, but
/// defend anyway) is treated the same way.
fn classify_prefault_outcome(stats_bytes: Option<&[u8]>) -> PrefaultOutcome {
    let Some(bytes) = stats_bytes else {
        return PrefaultOutcome::StatsMissing;
    };
    match serde_json::from_slice::<PrefaultStatsFile>(bytes) {
        Ok(s) if s.trace_loaded => PrefaultOutcome::Replayed {
            installed: s.installed,
            skipped: s.skipped,
        },
        Ok(_) => PrefaultOutcome::NoTrace,
        Err(_) => PrefaultOutcome::StatsMissing,
    }
}

/// Emit `engram_resume_prefault_total` + `engram_resume_prefault_chunks_total`
/// for one resume's prefault outcome.
fn emit_prefault_metrics(outcome: PrefaultOutcome) {
    metrics::counter!(
        crate::metrics::RESUME_PREFAULT_TOTAL,
        "outcome" => outcome.label(),
    )
    .increment(1);
    if let PrefaultOutcome::Replayed { installed, skipped } = outcome {
        metrics::counter!(
            crate::metrics::RESUME_PREFAULT_CHUNKS_TOTAL,
            "result" => "installed",
        )
        .increment(installed as u64);
        metrics::counter!(
            crate::metrics::RESUME_PREFAULT_CHUNKS_TOTAL,
            "result" => "skipped",
        )
        .increment(skipped as u64);
    }
}

#[async_trait]
impl SandboxBackend for PooledBackend {
    fn captures_guest_memory(&self) -> bool {
        self.inner.captures_guest_memory()
    }
    // Capability methods proxy to the wrapped backend — the pool is
    // a thin caching layer; whatever Process / VZ / FC reports
    // about itself is what callers see.
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        self.inner.harness_dial()
    }

    fn restore_memory_is_lazy_for(&self, fresh: bool) -> bool {
        self.inner.restore_memory_is_lazy_for(fresh)
    }

    fn bundle_dir(&self) -> &std::path::Path {
        // Delegates to the inner backend — the one source of truth this wrapper
        // also materializes into (`self.bundle_dir`, copied from here in `new`).
        self.inner.bundle_dir()
    }

    fn bundle_file_ext(&self) -> &'static str {
        // Delegates to the inner backend so the BundleStore materializes/sweeps
        // the SAME filename the backend attaches (squashfs, ADR 0096).
        self.inner.bundle_file_ext()
    }

    async fn guest_memory_stats(&self) -> Option<engram_core::traits::sandbox::GuestMemoryStats> {
        self.inner.guest_memory_stats().await
    }

    async fn create(&self, id: SandboxId, mut spec: SandboxSpec) -> Result<(), SandboxError> {
        let phase_total = crate::time_source::metrics_now();
        let mut image_resolve = std::time::Duration::ZERO;
        let mut materialize = std::time::Duration::ZERO;
        let result: Result<(), SandboxError> = async {
            // An explicit rootfs manifest (the disk-only cold-boot recovery)
            // wins over the image; `spec.image_uri` stays as record-keeping.
            let cached = match (&self.image_cache, spec.image_uri.clone()) {
                (Some(cache), Some(uri)) if spec.rootfs_manifest.is_none() => {
                    let t = crate::time_source::metrics_now();
                    let cached = cache.ensure_image(&uri).await.map_err(|e| {
                        SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                    })?;
                    image_resolve += t.elapsed();
                    tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                    Some((uri, cached))
                }
                _ => None,
            };
            let t = crate::time_source::metrics_now();
            let Some(volumes) = self.volumes.as_ref() else {
                // No session disk on this host: the inner backend opens
                // the image's flat rootfs file and nothing checkpoints it.
                if let Some((_, cached)) = cached.as_ref() {
                    spec.rootfs_source = Some(legacy_rootfs_path(cached)?);
                }
                materialize += t.elapsed();
                return self.inner.create(id, spec).await;
            };
            let root = match spec.rootfs_manifest {
                Some(manifest) => Source::Continue {
                    manifest,
                    base: None,
                },
                None => {
                    let Some((uri, cached)) = cached.as_ref() else {
                        return Err(SandboxError::InvalidSpec(
                            "create needs a rootfs manifest or an image".into(),
                        ));
                    };
                    let Some(chunk_store) = self.chunk_store.as_ref() else {
                        return Err(SandboxError::InvalidSpec(
                            "session disks need a chunk store".into(),
                        ));
                    };
                    ensure_chunked_manifest_in_blob_storage(chunk_store, cached).await?;
                    let Some(bundle) = cached.bundle.as_ref() else {
                        return Err(SandboxError::InvalidSpec(format!(
                            "image {uri} has no chunked disk manifest"
                        )));
                    };
                    Source::Image {
                        manifest: bundle.disk_manifest,
                    }
                }
            };
            let swap = match (spec.swap_manifest, spec.swap_mib.filter(|m| *m > 0)) {
                (Some(manifest), _) => Some(Source::Continue {
                    manifest,
                    base: None,
                }),
                (None, Some(mib)) => Some(Source::Empty {
                    size_bytes: u64::from(mib) << 20,
                }),
                (None, None) => None,
            };
            let provisioned = volumes
                .provision(id, root, swap)
                .await
                .map_err(|e| SandboxError::Vm(format!("provision session disks: {e}").into()))?;
            materialize += t.elapsed();
            spec.rootfs_source = Some(provisioned.root);
            spec.rootfs_manifest = Some(provisioned.root_manifest);
            spec.swap_source = provisioned.swap;
            spec.swap_manifest = provisioned.swap_manifest;
            let created = self.inner.create(id, spec).await;
            if created.is_err() {
                if let Err(error) = volumes.release(id).await {
                    tracing::warn!(sandbox_id = %id, %error, "release after a failed create failed");
                }
            }
            created
        }
        .await;

        let outcome = match &result {
            Ok(_) => "success",
            Err(SandboxError::InvalidSpec(_)) => "invalid_spec",
            Err(_) => "fc_error",
        };
        for (phase, elapsed) in [
            ("image_resolve", image_resolve),
            ("materialize", materialize),
            ("create_total", phase_total.elapsed()),
        ] {
            metrics::histogram!(
                crate::metrics::SANDBOX_BOOT_SECONDS,
                "phase" => phase,
                "outcome" => outcome,
                "kind" => "cold",
            )
            .record(elapsed.as_secs_f64());
        }
        result
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.inner.exec_stream(id, cmd).await
    }

    async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        self.inner.cancel_exec(id, exec_id).await
    }

    async fn write_file(
        &self,
        id: SandboxId,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<SessionFileMetadata, SandboxError> {
        self.inner.write_file(id, spec, bytes).await
    }

    async fn read_file(
        &self,
        id: SandboxId,
        path: String,
    ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
        self.inner.read_file(id, path).await
    }

    // ADR 0066: the port relay reaches agentd through the wrapped backend's
    // vsock (FC) — load-bearing in prod, where `self.inner` is FC.
    async fn open_guest_stream(
        &self,
        id: SandboxId,
        port: u32,
    ) -> Result<Option<engram_core::traits::sandbox::HarnessByteStream>, SandboxError> {
        self.inner.open_guest_stream(id, port).await
    }

    async fn snapshot_hold(
        &self,
        id: SandboxId,
        _diff: bool,
    ) -> Result<SnapshotMetadata, SandboxError> {
        if let Some(metadata) = self.held_snapshots.get(&id) {
            return Ok(metadata.clone());
        }
        let (_guard, cap) = self.capture_phase(id, CaptureUser::HeldCapture).await?;
        let metadata = self
            .finisher_with_class(Class::Prefix)
            .finish(id, cap)
            .await?;
        self.held_snapshots.insert(id, metadata.clone());
        Ok(metadata)
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // A Snapshot RPC serves a waiting session.
        self.snapshot_with_class(id, Class::Prefix, CaptureUser::SnapshotRpc)
            .await
    }

    /// The eviction flavor: capture, re-pause the guest (it is being
    /// torn down), persist the finalize record, and return. The finalize
    /// is then a host-owned job that is a pure function of what was
    /// persisted: the pending disk checkpoints on the session disk and
    /// the FC staging dir. A fresh host-agent process re-drives it.
    ///
    /// VZ/Process (`!supports_diff_checkpoints()`) and hosts with
    /// checkpointing disabled surface `InvalidSpec`; the coordinator
    /// falls through to the composed `snapshot()` pipeline.
    async fn snapshot_begin(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        if !self.inner.supports_diff_checkpoints()
            || !self.inner.captures_guest_memory()
            || self.checkpoint_dir.is_none()
        {
            return Err(SandboxError::InvalidSpec(
                "host-durable eviction finalize requires diff-checkpoint support, guest memory \
                 captures and a wired checkpoint_dir"
                    .into(),
            ));
        }
        // Idempotency: a scanner-retried eviction (coord-side timeout /
        // 409 / restart before the coordinator's own row-watcher noticed
        // completion) re-observes the still-in-flight finalize's
        // snapshot_id instead of re-capturing and racing itself.
        if let Some(existing) = self.pending_finalizes.get(&id) {
            return Ok(*existing);
        }
        let Some(session_id) = self.session_bindings.get(&id).map(|e| *e) else {
            return Err(SandboxError::InvalidSpec(
                "host-durable eviction finalize requires a session-bound sandbox".into(),
            ));
        };

        let (capture_guard, cap) = self.capture_phase(id, CaptureUser::EvictionCapture).await?;
        // Publish the working-set trace for the next resume's prefault (see
        // `spawn_trace_publish`); detached, never blocks the eviction.
        self.spawn_trace_publish(id);
        // Idempotent re-pause; best-effort (a failure leaves the orphan
        // running until destroy, which is today's behavior).
        if let Err(e) = self.inner.pause(id).await {
            tracing::debug!(sandbox_id = %id, error = %e, "post-capture re-pause failed (benign)");
        }

        // The gate above admits only a backend whose captures write guest
        // memory, and the finalize always publishes it.
        let SnapshotCapture {
            metadata,
            dest,
            chain_prev,
            paused_at,
            writes_memory: _,
            mut unwind,
        } = cap;
        let snapshot_id = metadata.id;

        // The pending disk checkpoints stay on the session disk under
        // their transient names; the finalize job finds them by name and
        // publishes them. From here the record and the job own the
        // capture's recovery state.
        let disk_pending = unwind
            .pending
            .drain(..)
            .map(|p| crate::eviction_finalize::PendingDisk {
                role: p.role(),
                previous: p.previous(),
            })
            .collect();
        unwind.defuse();
        drop(unwind);
        let mut record = crate::eviction_finalize::EvictionFinalizeRecord {
            snapshot_id,
            session_id,
            sandbox_id: id,
            image_version: metadata.image_version.clone(),
            size_bytes: metadata.size_bytes,
            paused_at,
            captured_at: metadata.created_at,
            dest: dest.clone(),
            chain_prev_ref: chain_prev.map(|(r, _)| r),
            disk_pending,
            aux_bundles: metadata.aux_bundles.clone(),
            stage: crate::eviction_finalize::FinalizeStage::default(),
            attempts: 0,
            swap_manifest: None,
            disk_manifest: None,
            memory_manifest: None,
            diff_extents: None,
        };
        let Some(finalizer) = self.eviction_finalizer() else {
            // Gated above; unreachable in practice (checkpoint_dir just
            // got checked), but never destroy a fresh capture on a
            // defensive None — clean up and fail loudly instead.
            if record.chain_prev_ref.is_some() {
                poison_checkpoint_chain_after_failed_diff(
                    &self.checkpoint_chains,
                    self.chain_heads.as_deref(),
                    id,
                    "eviction finalizer unavailable",
                );
            }
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return Err(SandboxError::InvalidSpec(
                "checkpoint_dir disappeared between the gate check and record construction".into(),
            ));
        };
        if let Err(e) = record
            .persist_captured(finalizer.fs.as_ref(), &finalizer.finalize_dir())
            .await
        {
            // Like the `None` arm above, a failure here must not leak the
            // multi-GiB staging dir: the eviction scanner retries ~30 s
            // apart, each attempt with a fresh snapshot_id and `dest`.
            // The removal deletes memory.diff, whose dirty set the Diff
            // already consumed — poison the chain so the retry is a Full.
            if record.chain_prev_ref.is_some() {
                poison_checkpoint_chain_after_failed_diff(
                    &self.checkpoint_chains,
                    self.chain_heads.as_deref(),
                    id,
                    "eviction finalize-record persist",
                );
            }
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return Err(SandboxError::Snapshot(format!(
                "sync the capture and persist its finalize record: {e}"
            )));
        }
        metrics::counter!(crate::metrics::EVICTION_FINALIZE_PERSISTED_TOTAL).increment(1);
        self.pending_finalizes.insert(id, snapshot_id);

        tokio::spawn(async move {
            // Chain bookkeeping is not concurrent-safe per sandbox, so the
            // capture lock spans the whole finalize job.
            crate::eviction_finalize::run_eviction_finalize(finalizer, record, capture_guard).await;
        });
        Ok(snapshot_id)
    }

    /// ADR 0045 C2: the inner backend's answer, unchanged — the pooled
    /// layer adds nothing to "what file is this guest's RAM mapped from".
    fn post_copy_source_view(
        &self,
        id: SandboxId,
    ) -> Option<engram_core::traits::sandbox::PostCopySourceView> {
        self.inner.post_copy_source_view(id)
    }

    /// ADR 0045 C2: the pre-pause presetup. Mints the export identity
    /// + packages the restore inputs derivable BEFORE the pause: the
    /// sidecar (live composition — nothing in it changes across the
    /// freeze), the inline CHAIN manifest (the session manifest ref is
    /// the chain's CURRENT durable ref: post-copy never re-chunks at
    /// capture, sealed chunks override via the peer at fault time, and
    /// the dest publishes them over this manifest after the drain), the
    /// live disk ref, and the hot set. NO pause, no fence — the guest
    /// runs until `migration_capture_postcopy`.
    async fn migration_presetup(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        let Some(peer) = self.migrate_peer_server() else {
            return Err(SandboxError::InvalidSpec(
                "no page server on this host — use snapshot-rehome".into(),
            ));
        };
        let Some((chain_ref, chain_manifest)) = self
            .checkpoint_chains
            .get(&id)
            .map(|c| (c.manifest_ref, c.manifest.clone()))
        else {
            return Err(SandboxError::InvalidSpec(
                "no checkpoint chain for this sandbox — use snapshot-rehome".into(),
            ));
        };
        if self.migrations.validate_open(id) {
            return Err(SandboxError::AlreadyExists);
        }
        // A lingering pending presetup is an ABANDONED move (the
        // coordinator died between the halves; nothing else can hold
        // one — the session lease serializes movers). Last-write-wins:
        // refusing here would brick every future move for this sandbox
        // until a host-agent restart.
        if let Some((_, stale)) = self.pending_presetups.remove(&id) {
            tracing::warn!(
                sandbox_id = %id,
                stale_export = %stale.export_id,
                "presetup superseding an abandoned pending presetup",
            );
            if let Some(volumes) = self.volumes.as_ref() {
                async {
                    for (_, seal) in stale.precopy {
                        discard_seal(volumes, seal).await;
                    }
                }
                .instrument(tracing::info_span!("presetup.discard_stale"))
                .await;
            }
        }
        // The guest RAM must be a private mapping of a file (the
        // per-image memfile or the substrate base shm): the pagemap
        // scan tells file-backed (clean) from anonymous (dirtied) pages
        // and the dest resolves clean chunks from the chain. A cold boot
        // is anonymous and cannot post-copy. Decided HERE, pause-free.
        let Some(view) = self.inner.post_copy_source_view(id) else {
            return Err(SandboxError::InvalidSpec(
                "guest RAM is not file-backed (cold boot) — use snapshot-rehome".into(),
            ));
        };
        #[cfg(target_os = "linux")]
        {
            // The same VMA scan the capture runs, on the running guest
            // (guest-RAM VMAs never change after load), so a backing that
            // does not match the mapping refuses here instead of inside
            // the blackout.
            let vmas = crate::dirty_map::guest_vmas(view.fc_pid, &view.memory_backing)
                .map_err(|e| SandboxError::Snapshot(format!("guest vmas: {e}")))?;
            if vmas.is_empty() {
                return Err(SandboxError::InvalidSpec(format!(
                    "FC process maps no guest VMA backed by {} — use snapshot-rehome",
                    view.memory_backing.display()
                )));
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = &view;
        // The disk moves in three tiers: the destination continues the
        // checkpoint lineages advertised here, pulls the precopy taken
        // now while the guest runs, and after the pause pulls only what
        // changed since. A sandbox without a session disk cannot post-copy.
        let volumes = self.volumes()?;
        let precopy = self.take_precopy(volumes, id).await?;
        if !precopy.contains_key(&DiskRole::Root) {
            for (_, seal) in precopy {
                discard_seal(volumes, seal).await;
            }
            return Err(SandboxError::InvalidSpec(
                "the sandbox has no root volume — use snapshot-rehome".into(),
            ));
        }
        let precopy_blocks: u64 = precopy.values().map(|s| s.sealed.len() as u64).sum();
        let out = match self
            .prepare_migration_export(id, chain_ref, &chain_manifest, peer.port(), precopy.clone())
            .instrument(tracing::info_span!("presetup.export"))
            .await
        {
            Ok(out) => out,
            Err(error) => {
                for (_, seal) in precopy {
                    discard_seal(volumes, seal).await;
                }
                return Err(error);
            }
        };
        tracing::info!(sandbox_id = %id, export_id = %out.export_id, precopy_blocks,
            "post-copy presetup took the disk precopy");
        #[cfg(target_os = "linux")]
        {
            let pending_presetups = self.pending_presetups.clone();
            let export_id = out.export_id.clone();
            // Marking is advisory too: never make presetup wait for procfs or sysfs.
            tokio::task::spawn_blocking(move || {
                let started = crate::time_source::metrics_now();
                let idle_marked = crate::dirty_map::guest_vmas(view.fc_pid, &view.memory_backing)
                    .and_then(|vmas| crate::page_idle::mark(view.fc_pid, &vmas))
                    .inspect_err(crate::page_idle::note_unavailable)
                    .is_ok();
                if let Some(mut pending) = pending_presetups.get_mut(&id) {
                    if pending.export_id == export_id {
                        pending.idle_marked = idle_marked;
                    }
                }
                tracing::info!(sandbox_id = %id, idle_mark_ms = started.elapsed().as_millis() as u64,
                    idle_marked, "post-copy idle mark complete");
            });
        }
        Ok(out)
    }

    /// ADR 0045 C2: the blackout half. Pause → take the disk seal (kept
    /// local) → fork-v3 vmstate-only package →
    /// pagemap scan → register the export (page server SEALS; the
    /// parked dest handler unblocks). The guest stays paused serving
    /// pages until commit (post-drain) or abort.
    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, export_id);
            Err(SandboxError::InvalidSpec(
                "post-copy capture is Linux-only".into(),
            ))
        }
        #[cfg(target_os = "linux")]
        {
            use engram_core::types::snapshot::PostCopyCaptureOut;
            // The guest runs while the destination provisions its disks
            // and pulls the precopy; the pause holds only what follows.
            let dest = self
                .pending_presetups
                .get(&id)
                .filter(|p| p.export_id == export_id)
                .map(|p| p.dest.clone());
            if let Some(dest) = dest {
                let started = crate::time_source::metrics_now();
                let ready = dest.contacted.wait(DEST_CONTACT_WAIT).await
                    && dest.ready.wait(DEST_READY_WAIT).await;
                tracing::info!(
                    sandbox_id = %id,
                    export_id,
                    ready,
                    dest_wait_ms = started.elapsed().as_millis() as u64,
                    "post-copy capture waited for the destination",
                );
            }
            // Issue #222: remove *only* if the export id matches. A plain
            // `remove(&id).filter(...)` evicts the entry unconditionally and
            // only then discards the returned value on mismatch — so a stale
            // straggler `capture(id, E1)` arriving after presetup re-minted E2
            // (last-write-wins, see `migration_presetup`) would delete the live
            // E2 entry and then fail, dooming the legitimate `capture(id, E2)`
            // to "no matching presetup". `remove_if` removes-and-returns only
            // when the predicate holds, atomically, so a mismatched straggler
            // leaves the live presetup intact and just errors.
            let Some((_, pending)) = self
                .pending_presetups
                .remove_if(&id, |_, p| p.export_id == export_id)
            else {
                return Err(SandboxError::InvalidSpec(
                    "no matching presetup for this capture (export id skew?)".into(),
                ));
            };
            // The consumed presetup is the unwind's from here: an exit
            // before the export exists re-inserts it, so a coordinator
            // retry finds it; the guest is resumed only once it is paused.
            let mut unwind = CaptureUnwind::new(self, id);
            unwind.resume_on_drop = false;
            unwind.arm();
            unwind.presetup_restore = Some((id, pending.clone(), self.pending_presetups.clone()));
            let Some((chain_ref, chain_manifest)) = self
                .checkpoint_chains
                .get(&id)
                .map(|c| (c.manifest_ref, c.manifest.clone()))
            else {
                return Err(SandboxError::InvalidSpec(
                    "checkpoint chain vanished since presetup".into(),
                ));
            };
            if chain_ref != pending.chain_ref {
                // A checkpoint landed between presetup and capture: the
                // dest is restoring against a STALE manifest — its
                // resolve() view would skew vs. ALT_SOURCE. Refuse; the
                // coordinator retries the whole move.
                return Err(SandboxError::InvalidSpec(
                    "chain advanced between presetup and capture — retry the move".into(),
                ));
            }
            let Some(peer) = self.migrate_peer_server() else {
                return Err(SandboxError::InvalidSpec("page server vanished".into()));
            };
            let Some(view) = self.inner.post_copy_source_view(id) else {
                return Err(SandboxError::InvalidSpec(
                    "no post-copy source view (substrate mapping gone?)".into(),
                ));
            };
            if self.migrations.validate_open(id) {
                return Err(SandboxError::AlreadyExists);
            }

            // Checkpoint fence: held for the export's lifetime.
            let gate = self.capture_gate(id);
            let capture_guard = gate.serialize(id, CaptureUser::PostCopyCapture).await;
            let _quiesce = gate.quiesce(id, CaptureUser::PostCopyCapture).await;

            // Blackout leg 1: pause. (PR 10 decomposition.)
            let t_pause = crate::time_source::metrics_now();
            let paused_at = self.clock.now_utc();
            self.inner
                .pause(id)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("post-copy pause: {e}")))?;
            let pause_ms = t_pause.elapsed().as_millis() as u64;
            unwind.resume_on_drop = true;

            // Blackout leg 3: vmstate. Fork v3: state.bin + sidecar
            // only — the memory artifact never materializes (the whole
            // point).
            let t_vmstate = crate::time_source::metrics_now();
            let (snapshot_id, export_dir) = self
                .inner
                .snapshot_vmstate_only_package(id, &pending.sidecar_json)
                .await?;
            let _ = snapshot_id;
            let vmstate_ms = t_vmstate.elapsed().as_millis() as u64;

            // The pagemap dirty map (blackout-critical; measured).
            let scan_started = crate::time_source::metrics_now();
            let vmas = crate::dirty_map::guest_vmas(view.fc_pid, &view.memory_backing)
                .map_err(|e| SandboxError::Snapshot(format!("guest vmas: {e}")))?;
            if vmas.is_empty() {
                return Err(SandboxError::Snapshot(format!(
                    "FC process maps no guest VMA backed by {} — cannot post-copy",
                    view.memory_backing.display()
                )));
            }
            let chunk_size = chain_manifest.chunk_size.as_u64();
            let total_bytes = chain_manifest.total_bytes;
            let seal =
                crate::dirty_map::scan_dirty_chunks(view.fc_pid, &vmas, chunk_size, total_bytes)
                    .map_err(|e| SandboxError::Snapshot(format!("pagemap scan: {e}")))?;
            let scan_ms = scan_started.elapsed().as_millis() as u64;
            let sealed_chunks = seal.count_ones();
            let total_chunks = seal.chunk_count;

            // durable_at: chunk idx -> the chain's hash at that offset
            // (ALT_SOURCE comparisons).
            let mut durable_at: Vec<Option<engram_chunk_store::manifest::ChunkHash>> =
                vec![None; total_bytes.div_ceil(chunk_size) as usize];
            for c in &chain_manifest.chunks {
                let idx = (c.offset / chunk_size) as usize;
                if let Some(slot) = durable_at.get_mut(idx) {
                    *slot = Some(c.hash);
                }
            }

            // The disk seal: a snapshot of each disk under the pause and
            // the blocks that changed since the presetup's snapshot. The
            // guard owns the seals until the export is registered.
            let t_seal = crate::time_source::metrics_now();
            let mut disk_seal = std::collections::BTreeMap::new();
            let volumes = self.volumes()?;
            for role in volumes.roles(id).await.map_err(disk_err)? {
                let checkpoint = volumes
                    .take(id, role)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("{role} disk seal: {e}")))?;
                unwind.pending.push(checkpoint);
                let checkpoint = unwind.pending.last().expect("just pushed");
                let advertised = match role {
                    DiskRole::Root => pending.disk_manifest_ref,
                    DiskRole::Swap => pending.swap_manifest_ref,
                };
                if advertised != Some(checkpoint.previous()) {
                    return Err(SandboxError::InvalidSpec(format!(
                        "{role} disk checkpoint advanced between presetup and capture — retry the move"
                    )));
                }
                let Some(precopy) = pending.precopy.get(&role) else {
                    return Err(SandboxError::InvalidSpec(format!(
                        "{role} disk gained a volume between presetup and capture — retry the move"
                    )));
                };
                let changed = volumes
                    .changed_since(&precopy.pending, checkpoint, MIGRATION_DISK_BLOCK_BYTES)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("{role} disk seal diff: {e}")))?;
                let checkpoint = unwind.pending.pop().expect("just pushed");
                let seal = Arc::new(DiskSeal {
                    sealed: changed.into_iter().collect(),
                    pending: checkpoint,
                });
                unwind.disk_seal.push(seal.clone());
                disk_seal.insert(role, seal);
            }
            let sealed_disk_blocks = disk_seal
                .values()
                .map(|seal| seal.sealed.len() as u64)
                .sum::<u64>();
            let precopy_disk_blocks = pending
                .precopy
                .values()
                .map(|seal| seal.sealed.len() as u64)
                .sum::<u64>();
            let disk_drain_ms = t_seal.elapsed().as_millis() as u64;

            // Register BOTH exports under the same identity, then the
            // role. Page-server registration is what unparks the dest
            // handler (the SEAL push) — last, after everything that
            // could still fail.
            let state_served = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let last_activity = std::sync::Arc::new(std::sync::Mutex::new(self.clock.now_mono()));
            // Issue #216 Gap 2: the peer page server shares THIS clock so
            // its TCP-only NeedAt serves refresh the same TTL
            // anchor the dumb-host sweep reads via `expired()`.
            let peer_last_activity = last_activity.clone();
            let inserted = self.migrations.insert(crate::migration::MigrationExport {
                export_id: export_id.to_string(),
                sandbox_id: id,
                snapshot_dir: export_dir.clone(),
                disk_seal,
                precopy: pending.precopy.clone(),
                clock: self.clock.clone(),
                state_served,
                last_activity,
                capture_guard,
            });
            if !inserted {
                return Err(SandboxError::AlreadyExists);
            }
            // The export owns the seals and the dest is about to load:
            // abort discards them, commit destroys the sandbox. The
            // presetup is not restored past this point.
            if let Err(error) = self
                .set_migration_role(id, Some(crate::migration::MigrationRole::PostCopySource))
                .await
            {
                // The export must not outlive a role the manifest does not
                // carry: take it back so the unwind guard resumes a guest
                // no export holds, and a restart sees no source role.
                drop(self.migrations.remove_validated(id, export_id));
                self.note_migration_role(id, None);
                return Err(error);
            }
            unwind.defuse();
            peer.register(crate::migrate_peer::PeerExport {
                export_id: export_id.to_string(),
                token: pending.peer_token,
                sandbox_id: id,
                fc_pid: view.fc_pid,
                vmas: vmas.clone(),
                seal,
                durable_at,
                chunk_size,
                total_bytes,
                serve: Default::default(),
                last_activity: peer_last_activity,
                clock: self.clock.clone(),
            });

            let fallback = pending.hot_chunks;
            let idle_marked = pending.idle_marked;
            tokio::task::spawn_blocking(move || {
                let started = crate::time_source::metrics_now();
                let mut pages = Vec::new();
                let captured = !idle_marked
                    || crate::dirty_map::walk_pagemap(view.fc_pid, &vmas, |offset, entry| {
                        if offset < total_bytes {
                            if let Some(pfn) = crate::page_idle::present_pfn(entry) {
                                pages.push((offset, pfn));
                            }
                        }
                        Ok(())
                    })
                    .inspect_err(crate::page_idle::note_unavailable)
                    .is_ok();
                let pfn_capture_ms = started.elapsed().as_millis() as u64;
                let offsets = if idle_marked && captured {
                    match crate::page_idle::scan(&pages, chunk_size) {
                        Ok(offsets) => offsets,
                        Err(error) => {
                            crate::page_idle::note_unavailable(&error);
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                let hot = crate::page_idle::hot_hashes(&chain_manifest, &offsets, &fallback);
                tracing::info!(sandbox_id = %id, idle_scan_ms = started.elapsed().as_millis() as u64,
                    pfn_capture_ms, hot_chunks = hot.len(), "post-copy idle scan complete after export registration");
                if let Err(error) = publish_hot_hint(&export_dir, &hot) {
                    tracing::info!(%error, "post-copy hot hint unavailable; drain continues");
                }
            });

            tracing::info!(
                sandbox_id = %id,
                export_id,
                sealed_chunks,
                total_chunks,
                sealed_disk_blocks,
                precopy_disk_blocks,
                "post-copy capture sealed; guest frozen as page server",
            );
            Ok(PostCopyCaptureOut {
                sealed_chunks,
                total_chunks,
                pause_ms,
                disk_drain_ms,
                vmstate_ms,
                scan_ms,
                sealed_disk_blocks,
                precopy_disk_blocks,
                paused_at_unix_ms: paused_at.timestamp_millis(),
            })
        }
    }

    /// Await the memory drain's terminal outcome over the handler's
    /// control socket. The disk landed before `state.bin` did, so the
    /// source is released when memory no longer depends on it.
    /// `PeerLost` pauses (poisons) the dest VM before returning. The
    /// destination role is the post-copy finish's to clear
    /// (`postcopy_finish`). A caller may drop this future at any time;
    /// the read thread then ends within one read timeout.
    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        use engram_core::types::snapshot::DrainOutcome;
        if let Some(done) = self.drained_dests.get(&id).map(|d| d.clone()) {
            return Ok(done);
        }
        let Some(sock) = self.inner.post_copy_control_sock(id) else {
            return Err(SandboxError::InvalidSpec(
                "no post-copy control socket for this sandbox".into(),
            ));
        };
        struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _cancel = CancelOnDrop(cancelled.clone());
        let outcome = tokio::task::spawn_blocking(move || -> Result<DrainOutcome, String> {
            const DRAIN_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);
            let mut stream = std::os::unix::net::UnixStream::connect(&sock)
                .map_err(|e| format!("dial control sock: {e}"))?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .map_err(|e| format!("control sock timeout: {e}"))?;
            let started = crate::time_source::metrics_now();
            loop {
                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err("drain wait cancelled".into());
                }
                if started.elapsed() > DRAIN_WAIT_BUDGET {
                    return Err("drain wait timed out".into());
                }
                let msg: engram_migrate_proto::HandlerControl =
                    match engram_migrate_proto::read_frame(&mut stream) {
                        Ok(msg) => msg,
                        // A frame is written whole under the backlog lock,
                        // so a timeout falls between frames.
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(e) => return Err(format!("control frame: {e}")),
                    };
                match msg {
                    engram_migrate_proto::HandlerControl::DrainDone {
                        pulled,
                        alt_sourced,
                        zero_chunks,
                        ms,
                        faults,
                        fault_us,
                        fault_max_us,
                        fault_around_chunks_installed,
                    } => {
                        // Restore-tail attribution: the fault-path
                        // totals are the serial P2P cost inside the FC
                        // load + early guest execution (cross-ref with
                        // the load_ms log).
                        tracing::info!(
                            faults,
                            fault_ms = fault_us / 1000,
                            fault_max_us,
                            fault_around_chunks_installed,
                            "post-copy memory drain done (handler fault-path totals)",
                        );
                        return Ok(DrainOutcome::Done {
                            pulled,
                            alt_sourced,
                            zero_chunks,
                            ms,
                        });
                    }
                    engram_migrate_proto::HandlerControl::PeerLost { remaining, detail } => {
                        return Ok(DrainOutcome::PeerLost { remaining, detail });
                    }
                    engram_migrate_proto::HandlerControl::Sealed { .. }
                    | engram_migrate_proto::HandlerControl::DrainProgress { .. } => continue,
                }
            }
        })
        .await
        .map_err(|e| SandboxError::Snapshot(format!("drain wait join: {e}")))?
        .map_err(SandboxError::Snapshot)?;

        if let DrainOutcome::PeerLost { remaining, detail } = &outcome {
            tracing::error!(
                sandbox_id = %id,
                remaining,
                detail,
                "post-copy drain lost its peer — poisoning (pausing) the dest VM",
            );
            if let Err(e) = self.inner.pause(id).await {
                tracing::warn!(sandbox_id = %id, error = %e, "poison-pause failed");
            }
        } else {
            self.drained_dests.insert(id, outcome.clone());
        }
        Ok(outcome)
    }

    /// ADR 0045: stream an export's artifacts. Allowlist-gated.
    async fn migration_fetch(
        &self,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<engram_core::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        use engram_core::types::snapshot::{MigrationFrame, MigrationItem};
        // Resolve + validate under the registry ref, then drop it (the
        // stream must not hold a dashmap guard). Before the capture the
        // presetup serves its precopy; everything else is not yet there.
        let (snapshot_dir, disk_seal, precopy) = match self.migrations.find_by_export_id(export_id)
        {
            Some(export) => {
                // Advisory hints never extend the export lease, even when absent.
                // Required artifacts and disk blocks still count as activity.
                if items
                    .iter()
                    .any(|item| !matches!(item, MigrationItem::HotHint))
                {
                    export.touch();
                }
                // Once state.bin is served, the TTL sweep must not resume us.
                if items.iter().any(|i| matches!(i, MigrationItem::StateBin)) {
                    export
                        .state_served
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                (
                    Some(export.snapshot_dir.clone()),
                    export.disk_seal.clone(),
                    export.precopy.clone(),
                )
            }
            None => {
                let Some(presetup) = self
                    .pending_presetups
                    .iter()
                    .find(|p| crate::migration::constant_time_str_eq(&p.export_id, export_id))
                else {
                    return Err(SandboxError::NotFound);
                };
                presetup.dest.contacted.set();
                if items.iter().any(|i| matches!(i, MigrationItem::StateBin)) {
                    presetup.dest.ready.set();
                }
                (None, Default::default(), presetup.precopy.clone())
            }
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<MigrationFrame, SandboxError>>(16);
        tokio::spawn(async move {
            const FRAME: usize = 1024 * 1024;
            for (idx, item) in items.into_iter().enumerate() {
                let idx = idx as u32;
                let info = |seal: &Arc<DiskSeal>| {
                    serde_json::to_vec(&seal.info())
                        .map(bytes::Bytes::from)
                        .map_err(|e| SandboxError::Snapshot(format!("disk seal info: {e}")))
                };
                let bytes: Result<bytes::Bytes, SandboxError> = match (item, &snapshot_dir) {
                    // Not captured yet: the dest keeps polling.
                    (
                        MigrationItem::StateBin
                        | MigrationItem::HotHint
                        | MigrationItem::DiskSealInfo(_),
                        None,
                    ) => Err(SandboxError::NotFound),
                    (MigrationItem::StateBin, Some(dir)) => fs::read(dir.join("state.bin"))
                        .await
                        .map(bytes::Bytes::from)
                        .map_err(|e| SandboxError::Snapshot(format!("read state.bin: {e}"))),
                    (MigrationItem::HotHint, Some(dir)) => fs::read(dir.join("hot-hint.json"))
                        .await
                        .map(bytes::Bytes::from)
                        .map_err(|e| {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                SandboxError::NotFound
                            } else {
                                SandboxError::Snapshot(format!("read hot hint: {e}"))
                            }
                        }),
                    (MigrationItem::DiskSealInfo(role), Some(_)) => match disk_seal.get(&role) {
                        Some(seal) => info(seal),
                        None => Err(SandboxError::InvalidSpec(format!(
                            "this export has no {role} disk seal"
                        ))),
                    },
                    (MigrationItem::DiskPrecopyInfo(role), _) => match precopy.get(&role) {
                        Some(seal) => info(seal),
                        None => Err(SandboxError::InvalidSpec(format!(
                            "this export has no {role} disk precopy"
                        ))),
                    },
                    // The seals are the allowlist; the newer snapshot
                    // wins for a block both list.
                    (MigrationItem::DiskBlockAt(role, index), _) => {
                        let tier = [disk_seal.get(&role), precopy.get(&role)]
                            .into_iter()
                            .flatten()
                            .find(|seal| seal.sealed.contains(&index));
                        match tier {
                            Some(seal) => seal
                                .read_block(index)
                                .await
                                .map(bytes::Bytes::from)
                                .map_err(|e| {
                                    SandboxError::Snapshot(format!(
                                        "read sealed disk block {index}: {e}"
                                    ))
                                }),
                            None => Err(SandboxError::InvalidSpec(format!(
                                "{role} disk block {index} is not sealed on this export"
                            ))),
                        }
                    }
                };
                match bytes {
                    Ok(bytes) => {
                        let total = bytes.len();
                        let item_hash = engram_migrate_proto::wire_hash(&bytes);
                        let mut off = 0usize;
                        loop {
                            let end = (off + FRAME).min(total);
                            let last = end == total;
                            let frame = MigrationFrame {
                                item_idx: idx,
                                offset: off as u64,
                                data: bytes.slice(off..end),
                                last,
                                item_hash: last.then_some(item_hash),
                            };
                            if tx.send(Ok(frame)).await.is_err() {
                                return;
                            }
                            if end == total {
                                break;
                            }
                            off = end;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        use futures::StreamExt;
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }

    /// ADR 0045: the move landed — drop the export (releasing both
    /// fences) and destroy the frozen VM + its local snapshot dir.
    async fn migration_commit(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        // Atomic validate+remove: nothing serializes this RPC against
        // the dumb-host TTL sweep, so a non-atomic validate-then-remove
        // would let both racers pass `validate` and panic the loser's
        // `remove().expect()`. `remove_validated` makes the race loser
        // see `None` → clean NotFound (callers already handle Err).
        let Some(export) = self.migrations.remove_validated(id, export_id) else {
            return Err(SandboxError::NotFound);
        };
        // ADR 0045 C2: a post-copy commit also retires the page-server
        // export (the dest reported DrainDone — or the coordinator gave
        // up on it) and the source's role fence.
        if let Some(peer) = self.migrate_peer_server() {
            peer.remove(export_id);
        }
        self.note_migration_role(id, None);
        let snapshot_dir = export.snapshot_dir.clone();
        // The export is consumed, so a retry cannot reach its directory:
        // remove it whether or not the destroy succeeds.
        drop(export); // releases the capture guard (checkpoint fence)
        let destroyed = self.destroy(id).await;
        let _ = fs::remove_dir_all(&snapshot_dir).await;
        destroyed?;
        tracing::info!(sandbox_id = %id, "migration committed; source destroyed");
        Ok(())
    }

    /// The move failed: discard the sealed checkpoints and un-pause in
    /// place. Zero loss.
    async fn migration_abort(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        // ADR 0045 C2 split-brain note: an EXPLICIT abort carries the
        // coordinator's knowledge that the dest provably never loaded
        // the shipped state (the postcopy-never-loaded marker), so it
        // is allowed even after StateBin was fetched. The forbidden
        // arm is the dumb-host TTL's self-resume, which goes through
        // `migration_abort_unserved`, never this RPC.
        //
        // Atomic validate+remove (see migration_commit): the TTL sweep
        // and a coordinator abort RPC race with nothing serializing
        // them; `remove_validated` ensures exactly one consumes the
        // export and the loser gets a clean NotFound instead of a panic.
        let Some(export) = self.migrations.remove_validated(id, export_id) else {
            return Err(SandboxError::NotFound);
        };
        if let Some(peer) = self.migrate_peer_server() {
            peer.remove(export_id);
        }
        self.abort_migration_export(export).await
    }

    async fn migration_abort_unserved(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<bool, SandboxError> {
        use crate::migration::UnservedRemoval;
        let export = match self.migrations.remove_unserved(id, export_id) {
            UnservedRemoval::Removed(export) => export,
            UnservedRemoval::Served => return Ok(false),
            UnservedRemoval::Missing => return Err(SandboxError::NotFound),
        };
        if let Some(peer) = self.migrate_peer_server() {
            peer.remove(export_id);
        }
        // The answer is final once the export is gone: no destination can
        // load the state. A failed resume is left to the caller's rollback,
        // whose `resume` is idempotent.
        if let Err(error) = self.abort_migration_export(export).await {
            tracing::warn!(sandbox_id = %id, %error, "unserved migration abort did not finish");
        }
        Ok(true)
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0014 issue #1/#2: caller's downstream pipeline
        // (record_snapshot → destroy → mark Idle) succeeded; the
        // snapshot is now owned by the `snapshots` row. Clear our
        // tracking so a subsequent snapshot() for this sandbox treats
        // the prior artifacts as no-longer-our-responsibility (the
        // committed snapshot stays on disk and in BlobStorage; PG holds
        // the durable reference).
        if self.inflight_snapshots.remove(&id).is_none() {
            tracing::debug!(
                sandbox_id = %id,
                "commit_snapshot called with no in-flight snapshot tracked; no-op",
            );
        }
        Ok(())
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0014 issue #1/#2: caller's downstream pipeline failed;
        // tear down the snapshot artifacts we just produced before
        // returning. Idempotent: if there's no in-flight snapshot for
        // this sandbox (commit already ran, or abort already ran, or
        // we never produced one), this is a clean no-op.
        if !self.inflight_snapshots.contains_key(&id) {
            return Ok(());
        }
        self.abort_prior_inflight_snapshot(id).await
    }

    fn working_set_trace_path(&self, id: SandboxId) -> Option<std::path::PathBuf> {
        self.inner.working_set_trace_path(id)
    }

    fn prefault_stats_path(&self, id: SandboxId) -> Option<std::path::PathBuf> {
        self.inner.prefault_stats_path(id)
    }

    fn snapshot_path_for(&self, snapshot_id: engram_core::types::SnapshotId) -> std::path::PathBuf {
        self.inner.snapshot_path_for(snapshot_id)
    }

    // ADR 0028 Fix A: defer to the wrapped backend — a PooledBackend
    // over FC checkpoints, over VZ/Process doesn't.
    fn supports_diff_checkpoints(&self) -> bool {
        self.inner.supports_diff_checkpoints()
    }

    /// ADR 0018 commit 12m: forward pause to the wrapped backend.
    /// PooledBackend doesn't have its own pause concept — it just
    /// delegates to whatever VMM is underneath. This is the EXTERNAL
    /// orchestration surface (the coordinator's rung-2 park rides it);
    /// every internal capture path quiesces via `self.inner.pause`
    /// directly, under its own capture lock.
    ///
    /// ADR 0091: refuse (typed, retryable) while a capture holds this
    /// sandbox's capture lock instead of racing it on FC's single-
    /// threaded API socket. This race — park pause vs the periodic
    /// checkpoint's own pause/flush/resume — made rung-2 parking fail
    /// on EVERY observed idle eviction (2026-07-11 campaign: 4/4 fell
    /// through to a 15-19 min full checkpoint). The caller (idle
    /// evictor) treats it like the checkpoint driver's own skip: retry
    /// next nomination.
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        if self.capture_in_flight(id) {
            ::metrics::counter!(
                crate::metrics::RUNG2_PARK_FAILED_TOTAL,
                "reason" => "capture_in_flight",
            )
            .increment(1);
            // `Unavailable` round-trips the gRPC boundary as the typed
            // RETRYABLE variant (ADR 0050 C), which is exactly the
            // caller contract: try the park again next nomination.
            return Err(SandboxError::Unavailable(format!(
                "pause {id}: a capture is in flight; retry after it completes"
            )));
        }
        let res = self.inner.pause(id).await;
        if let Err(e) = &res {
            ::metrics::counter!(
                crate::metrics::RUNG2_PARK_FAILED_TOTAL,
                "reason" => "vmm_pause",
            )
            .increment(1);
            tracing::warn!(%id, error = %e, "external pause failed at the VMM");
        }
        res
    }

    /// Forward resume. Symmetric with pause.
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.resume(id).await?;
        self.held_snapshots.remove(&id);
        // A resumed guest carries no source role. Idempotent, and the retry
        // path for a role clear that failed inside an earlier abort or hold.
        self.set_migration_role(id, None).await?;
        Ok(())
    }

    fn swap_device(&self, id: SandboxId) -> Option<PathBuf> {
        self.inner.swap_device(id)
    }

    async fn restore(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        _disks: Option<engram_core::types::sandbox::SessionDisks>,
    ) -> Result<(), SandboxError> {
        // ADR 0038 B2: capture the source's memory manifest before
        // `metadata` is moved, then seed the chain sparse after the VM
        // is up — so the first post-resume periodic checkpoint is a
        // cheap diff, not a Full re-read of guest RAM (the UFFD fault
        // storm). Fresh creates seed the same way now (ADR 0045
        // seed-at-create) — see `restore_fresh` / `restore_base_for_session`.
        let mut metadata = metadata;
        let migration = metadata.migration_source.take();
        if let Some(mig) = &migration {
            if !mig.post_copy {
                return Err(SandboxError::InvalidSpec(
                    "live restore requires post-copy".into(),
                ));
            }
            // ADR 0045 C2: the post-copy destination. Stage the
            // presetup's package (no export exists yet — the source
            // hasn't paused), arm the role fence, and spawn the
            // fetch poller that lands state.bin + the drained disk
            // manifest once the capture seals. The FC load gate
            // (restore_in_jail) waits on those files; the handler
            // parks at the page server for the SEAL.
            self.postcopy_stage(&metadata, mig).await?;
        }
        let memory_ref = metadata.memory_manifest;
        // Resume keeps the snapshot's pinned mounts — no per-session selection.
        self.restore_with(
            id,
            metadata,
            /*fresh=*/ false,
            /*fork_disk_at_attach=*/ false,
            Vec::new(),
        )
        .await?;
        // Peer pages differ from the source chain, so a post-copy
        // destination seeds after its drain (`postcopy_finish`).
        if migration.is_none() {
            if let Some(memory_ref) = memory_ref {
                let _ = self
                    .finisher()
                    .seed_chain(id, ChainSeed::Manifest(memory_ref))
                    .await;
            }
        }
        self.spawn_prefault_stats_probe(id);
        Ok(())
    }

    async fn restore_fresh(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        _disks: Option<engram_core::types::sandbox::SessionDisks>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<(), SandboxError> {
        // ADR 0045 seed-at-create: guest RAM right after a fresh restore
        // is byte-identical to the base snapshot's memory manifest, and
        // FC dirty-page tracking runs from the restore — exactly the
        // resume-seeding argument. Seeding here makes the session's
        // FIRST capture a diff (pages it actually dirtied) instead of a
        // Full dump+re-chunk of all guest RAM. FORKED seed: the source
        // manifest is shared across sessions, so the chain must own a
        // fresh lineage (`ChainSeed::Fork`).
        let memory_ref = metadata.memory_manifest;
        self.restore_with(
            id,
            metadata,
            /*fresh=*/ true,
            /*fork_disk_at_attach=*/ true,
            selected_mounts,
        )
        .await?;
        if let Some(memory_ref) = memory_ref {
            let _ = self
                .finisher()
                .seed_chain(id, ChainSeed::Fork(memory_ref))
                .await;
        }
        Ok(())
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // A held source forgets its swap policy and metadata with the VM.
        self.held_snapshots.remove(&id);
        self.hint_pollers.remove(&id);
        self.memory_prefetches.remove(&id);
        self.teardowns
            .run_or_join(id, || {
                let session_bindings = self.session_bindings.clone();
                let unreachable_guests = self.unreachable_guests.clone();
                let egress = self.egress.clone();
                let inner = self.inner.clone();
                let last_snapshot_unix_ms = self.last_snapshot_unix_ms.clone();
                let first_runs = self.first_runs.clone();
                let peer_registrations = self.peer_registrations.clone();
                let sandbox_registrations = self.sandbox_registrations.clone();
                let checkpoint_pacing = self.checkpoint_pacing.clone();
                let dead_probe_inflight = self.dead_probe_inflight.clone();
                let checkpoint_chains = self.checkpoint_chains.clone();
                let chain_heads = self.chain_heads.clone();
                let capture_gates = self.capture_gates.clone();
                let migration_roles = self.migration_roles.clone();
                let drained_dests = self.drained_dests.clone();
                let volumes = self.volumes.clone();
                let pending_finalizes = self.pending_finalizes.clone();
                let pending_presetups = self.pending_presetups.clone();
                let migrations = self.migrations.clone();
                let migrate_peer = self.migrate_peer.get().cloned();
                Box::pin(async move {
                    // Unregister from the egress proxy first, so a still-alive
                    // guest's traffic stops being rewritten. The binding goes
                    // whether or not a proxy is wired.
                    let removed_session = session_bindings.remove(&id).map(|(_, sid)| sid);
                    unreachable_guests.remove(&id);
                    migration_roles.remove(&id);
                    drained_dests.remove(&id);
                    // An open export's page-server entry reads the VM's
                    // process, so it goes before the VM.
                    let export = migrations.remove(id);
                    if let (Some(export), Some(peer)) = (export.as_ref(), migrate_peer.as_ref()) {
                        peer.remove(&export.export_id);
                    }
                    if let Some(egress) = egress.as_ref() {
                        if let Some(session_id) = removed_session {
                            egress.unregister_session(session_id);
                        }
                    }
                    let result = inner.destroy(id).await;
                    // The disks go after the VM has closed them. A pending
                    // eviction finalize still publishes from them; it runs
                    // this destroy again at its terminal step. A move holds
                    // snapshots of them that the release cannot remove while
                    // they are open: an export's seals and precopy, or the
                    // precopy of a presetup that never reached its capture.
                    if let Some(export) = export {
                        let _ = fs::remove_dir_all(&export.snapshot_dir).await;
                        if let Some(volumes) = volumes.as_ref() {
                            for (_, seal) in export.disk_seal.into_iter().chain(export.precopy) {
                                discard_seal(volumes, seal).await;
                            }
                        }
                    }
                    if let Some(volumes) = volumes.as_ref() {
                        if let Some((_, presetup)) = pending_presetups.remove(&id) {
                            for (_, seal) in presetup.precopy {
                                discard_seal(volumes, seal).await;
                            }
                        }
                        if !pending_finalizes.contains_key(&id) {
                            if let Err(error) = volumes.release(id).await {
                                tracing::warn!(sandbox_id = %id, %error, "release of the session disks failed");
                            }
                        }
                    }
                    let _ = last_snapshot_unix_ms.remove(&id);
                    let _ = first_runs.remove(&id);
                    let _ = peer_registrations.remove(&id);
                    let _ = sandbox_registrations.remove(&id);
                    let _ = checkpoint_pacing.remove(&id);
                    let _ = dead_probe_inflight.remove(&id);
                    // Durable checkpoint records survive destroy: an eviction's
                    // final checkpoint stays re-advertisable until the
                    // coordinator acks it.
                    let _ = checkpoint_chains.remove(&id);
                    if let Some(store) = &chain_heads {
                        store.remove_best_effort(id);
                    }
                    let _ = capture_gates.remove(&id);
                    result.map_err(Arc::new)
                })
            })
            .await
            .map_err(|error| SandboxError::Vm(Box::new(error)))
    }

    async fn start_agent(&self, id: SandboxId, mut agent: AgentSpec) -> Result<(), SandboxError> {
        let _quiesce = self
            .capture_gate(id)
            .quiesce(id, CaptureUser::StartAgent)
            .await;
        // ADR 0021 P1.2: only the host-agent knows the per-host egress-
        // proxy CA, so it stamps the PEM onto the AgentSpec right
        // before the backend sees it. Each backend rides it into the
        // guest on the same `SpawnHarness` frame that spawns the
        // harness (2026-07 core-ops fold — one first-contact RPC
        // installs the CA and spawns, instead of a separate round
        // trip). Coord-supplied specs always arrive with
        // `host_ca_pem = None`; the host-agent fills it in here.
        if agent.host_ca_pem.is_none() {
            agent.host_ca_pem = self.egress.as_ref().map(|e| e.ca_cert_pem.clone());
        }
        self.inner.start_agent(id, agent).await
    }

    /// ADR 0084 §B: cold-base / warm-overlay split.
    ///
    /// `req.cold_base_plan` (computed entirely coordinator-side —
    /// `ColdBasePlan`'s doc) drives the stage plan:
    ///
    /// - `NotApplicable` (non-FC claiming host, or an FC host with no
    ///   `fc_snapshot_version` reported): unchanged single-stage path —
    ///   cold-boot (or the caller never even reaches this since a
    ///   non-diffing backend can't have been claimed a `Hit`/`Miss`; see
    ///   the hard-error gate below), run the hook if any, ONE snapshot
    ///   call at the end. Warm-less images take this same single path
    ///   naturally (`warm.is_none()` just skips the hook step) — no
    ///   special-casing needed since there's only ever one snapshot call
    ///   in this arm.
    /// - `Miss { content_key }`: cold-boot, and — ONLY if this is a WARM
    ///   image (a warm-less image's single final snapshot IS the cold
    ///   base, see above) — take an extra Full snapshot BEFORE the hook
    ///   runs (chain auto-seeds off it, `self.snapshot`'s own
    ///   `advance_checkpoint_state` post-capture bookkeeping — no new
    ///   plumbing needed), then run the hook, then the final overlay
    ///   snapshot (now a cheap Diff, chain already seeded).
    /// - `Hit { content_key, snapshot }`: `self.restore(snapshot)`
    ///   instead of cold-booting (chain auto-seeds sparse off the
    ///   restored base's OWN memory manifest — the same machinery a
    ///   plain session resume uses), run the hook, take the final
    ///   overlay snapshot (a cheap Diff). No cold boot at all.
    ///
    /// Every arm ends with exactly one snapshot call whose metadata
    /// becomes [`engram_core::types::capture_job::CaptureJobResult::
    /// snapshot`] — the artifact the enabled image points at.
    #[tracing::instrument(name = "host.build_base_snapshot", skip_all)]
    async fn build_base_snapshot(
        &self,
        req: engram_core::traits::sandbox::BuildBaseSnapshotRequest,
        progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    ) -> Result<engram_core::types::capture_job::CaptureJobResult, SandboxError> {
        use engram_core::types::capture_job::{CaptureJobResult, CapturedColdBase, ColdBasePlan};

        let engram_core::traits::sandbox::BuildBaseSnapshotRequest {
            mut spec,
            warm,
            capture_env,
            capture_egress,
            cold_base_plan,
        } = req;

        spec.env.insert("ENGRAM_BASE_CAPTURE".into(), "1".into());

        // ADR 0084 decision 11: a `Hit`/`Miss` plan means placement
        // pinned this host as FC-capable (`fc_snapshot_version` +
        // `supports_diff_checkpoints`). If the actual backend can't
        // diff, that's a placement bug — hard error, never a silent
        // single-stage fallback.
        if !matches!(cold_base_plan, ColdBasePlan::NotApplicable)
            && !self.inner.supports_diff_checkpoints()
        {
            return Err(SandboxError::CaptureFailed(
                engram_core::types::CaptureFailure {
                    kind: engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch,
                    stage: Some("assigned".to_string()),
                    tail: String::new(),
                    message: "claim carried a cold-base plan but this backend does not support \
                          diff memory snapshots (supports_diff_checkpoints() == false); this \
                          is a placement bug, not a transient condition"
                        .to_string(),
                },
            ));
        }

        // ADR 0021 P1.5: no stub-harness attach — the harness lives
        // in the rootfs of the image being captured, so the snapshot
        // is already complete without any second virtio-blk drive.

        // The config `[env]` (JAVA_HOME, PATH, …) the `[warm]` hook needs,
        // MERGED with the resolved capture-time env (`capture_env` — literals
        // plus the coordinator's SecretStore-resolved refs). capture_env wins
        // on a key collision: it's the operator's explicit capture override.
        // Captured before `spec` is moved into `create`; passed to the warm
        // hook below as its exec env so it runs with the image's environment
        // plus whatever secrets the warm boot needs.
        let mut session_env = spec.env.clone();
        session_env.extend(capture_env);
        let is_warm = warm.is_some();
        // Captured before `spec` moves into `create` — the balloon
        // reclaim target below derives from guest RAM size.
        let guest_mem_mib = u64::from(spec.memory.max_mib);

        // ---- stage 1: a LIVE VM at (or converging toward) agentd-ready
        // — restore the cold base on a Hit (no cold boot at all), else
        // cold-boot fresh (Miss / NotApplicable). ----
        //
        // ADR 0084 P1b: the teardown reconcile no longer exempts capture
        // VMs via a `base_captures` DashMap tracked here — the host-
        // agent's capture-job executor (`capture_job.rs`) tracks
        // `(job_id, sandbox_id)` for the whole job lifetime and the
        // reconcile consults IT instead (`CaptureJobExecutor::
        // is_live_sandbox`), so the exemption survives exactly as long
        // as a live job record says it should, not as long as this one
        // call happens to run.
        let id = match &cold_base_plan {
            ColdBasePlan::Hit { snapshot, .. } => {
                let memory_ref = snapshot.memory_manifest;
                // The cold base is SHARED across every capture that hits
                // it, so BOTH lineages must fork a private chain (same
                // rule as session-create off a shared base template):
                //
                // - memory: seed FORKED, not sparse — the first capture's
                //   overlay already published `base_id@v2`, so a second
                //   hit's diff would collide ("version conflict: attempted
                //   v2, latest is v2" — the exact FC-lane CI failure the
                //   forked seed fixed).
                // - disk: `fork_disk_at_attach=true` — the warm hook's
                //   writes land in the session volume, and an unforked
                //   attach would continue (and race) the cold base's
                //   shared disk manifest lineage exactly the same way.
                //
                // `fresh=false`: this is a resume of the cold base's
                // pinned device model, not a session fresh-create — no
                // per-session mount swap, resume memory semantics.
                let id = SandboxId::new();
                self.restore_with(
                    id,
                    (**snapshot).clone(),
                    /*fresh=*/ false,
                    /*fork_disk_at_attach=*/ true,
                    Vec::new(),
                )
                .await?;
                if let Some(memory_ref) = memory_ref {
                    let _ = self
                        .finisher()
                        .seed_chain(id, ChainSeed::Fork(memory_ref))
                        .await;
                }
                // ADR 0088 addendum: a balloon-era cold base was dumped
                // with the balloon INFLATED — the deflate happens inside
                // the teardown-covered capture block below (so a deflate
                // failure destroys the restored VM instead of leaking it
                // to the reconcile).
                self.spawn_prefault_stats_probe(id);
                id
            }
            ColdBasePlan::Miss { .. } | ColdBasePlan::NotApplicable => {
                let id = SandboxId::new();
                self.create(id, spec).await?;
                id
            }
        };

        // Capture-time egress (ADR 0080): register the coordinator-assembled
        // policy for the capture VM's guest IP so the `[warm]` hook can reach
        // the network (e.g. OIDC discovery) — without it the proxy denies the
        // capture VM as an unknown guest. No-op when the coordinator granted
        // no egress. Torn down after the destroy below, on every path.
        let capture_egress = self.register_capture_egress(id, capture_egress).await;

        // Issue #539: `phase=boot` — the capture VM exists and is
        // booting (or, on a Hit, already live from the restore) toward
        // agentd-ready. Best-effort; a slow/dropped consumer must not
        // stall the capture.
        let boot_event = engram_core::types::CaptureProgress {
            phase: engram_core::types::CapturePhase::Boot,
            sandbox_id: Some(id),
            warm_stage: None,
            detail: None,
            output_tail: String::new(),
            warm_stages: Vec::new(),
        };
        let _ = progress.try_send(boot_event.clone());

        // Drive the capture to a snapshot, then ALWAYS tear the VM down —
        // a capture VM has no session and must not linger.
        let captured: Result<CaptureJobResult, SandboxError> = async {
            // Finding 1: a slow cold boot / chunk materialize has no
            // progress source of its own to renew the capture-claim lease
            // — resend the boot frame every 30s until agentd-ready (or we
            // bail). Dropped (stopping the ticker) the instant this leg
            // ends, on every path, including the early `?`-returns below.
            let boot_keepalive = spawn_leg_keepalive(progress.clone(), boot_event);
            // Wait for the guest to reach agentd-ready (bootstrap on
            // accept(), harness unmounted — the option-D capture point).
            // On a Hit restore the watch is already pre-set true (FC's
            // restore path); on VZ this returns InvalidSpec (no
            // readiness concept there).
            match self.inner.wait_agent_ready(id).await {
                Ok(()) => {}
                Err(SandboxError::InvalidSpec(_)) => {}
                Err(e) => return Err(e),
            }
            drop(boot_keepalive);

            // ADR 0088 addendum (adversarial-review fix): a balloon-era
            // cold base was dumped with the balloon INFLATED, so a Hit
            // restore comes up ballooned — deflate AND CONFIRM
            // (`balloon_release`'s contract) before anything runs in the
            // guest. Only the TYPED no-device outcome (`InvalidSpec` — a
            // legacy balloon-less base, or a non-FC backend) is a no-op;
            // any other failure fails the capture (inside this block, so
            // the teardown below destroys the VM rather than leaking it).
            if matches!(cold_base_plan, ColdBasePlan::Hit { .. }) {
                match self.inner.balloon_release(id).await {
                    Ok(()) => {}
                    Err(SandboxError::InvalidSpec(msg)) => {
                        tracing::debug!(
                            sandbox_id = %id,
                            detail = %msg,
                            "cold-base restore: no balloon device to deflate (legacy base)",
                        );
                    }
                    Err(e) => {
                        return Err(SandboxError::CaptureFailed(
                            engram_core::types::CaptureFailure {
                                kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                                stage: Some("booting".to_string()),
                                tail: String::new(),
                                message: format!(
                                    "balloon deflate after cold-base restore failed: {e} — \
                                     refusing to run the warm hook in a possibly-starved guest"
                                ),
                            },
                        ));
                    }
                }
            }

            // ADR 0084 §B3: a WARM image on a MISS needs its OWN cold
            // base minted before the hook runs (the hook must land on
            // top of an established base, not fold into the artifact's
            // only capture) — take a Full snapshot now. `self.snapshot`
            // takes Full here because no checkpoint chain exists yet for
            // this freshly-created sandbox; its own post-capture
            // bookkeeping (`advance_checkpoint_state`) auto-seeds the
            // chain off the Full's memory manifest, so the LATER overlay
            // snapshot below is automatically a cheap Diff — no new
            // pause/resume/seed plumbing needed here.
            //
            // Every other combination skips this: `NotApplicable` (no
            // cold-base concept), `Hit` (already have one — the restore
            // above seeded the chain sparse off IT), and warm-less
            // (its single final snapshot below IS the cold base).
            // ADR 0088 addendum: only the DUMP blocks the hook now. The
            // seed's finish() (the 40s–11.5min chunk+upload leg measured
            // in prod) runs on a spawned task concurrent with the warm
            // hook and is joined — success AND failure paths — before
            // the final snapshot below (join-before-final keeps the
            // final capture a Diff and keeps the durability barrier
            // ahead of the CaptureJobResult, exactly as the inline
            // shape did).
            let deferred_cold_base = if is_warm
                && matches!(cold_base_plan, ColdBasePlan::Miss { .. })
            {
                let cold_base_event = engram_core::types::CaptureProgress {
                    phase: engram_core::types::CapturePhase::Snapshot,
                    sandbox_id: Some(id),
                    warm_stage: None,
                    detail: Some("cold-base memory dump".to_string()),
                    output_tail: String::new(),
                    warm_stages: Vec::new(),
                };
                let _ = progress.try_send(cold_base_event.clone());
                let cold_base_keepalive = spawn_leg_keepalive(progress.clone(), cold_base_event);

                // ADR 0088 addendum: shrink the seed. Inflating the
                // balloon hands the guest's free pages back to the host
                // (`MADV_DONTNEED`), so the dense dump reads zeros there
                // and the all-zero 512 KiB elision drops them from the
                // memory manifest (~guest-RAM → ~touched-pages).
                // Fail-open is limited to the TYPED no-balloon outcome
                // (kill switch, legacy kernel, VZ) — any other reclaim
                // failure means the inflate target may have landed, and
                // `balloon_inflate_for_seed` normalizes (release-and-
                // confirm) or fails the capture rather than ever running
                // the warm hook in a possibly-starved guest
                // (adversarial-review fix). The reserve keeps the
                // paused-adjacent guest comfortably functional while
                // inflated.
                const BALLOON_RESERVE_MIB: u64 = 1536;
                let balloon_target = guest_mem_mib.saturating_sub(BALLOON_RESERVE_MIB);
                let inflated = balloon_inflate_for_seed(
                    self.inner.as_ref(),
                    id,
                    balloon_target,
                    std::time::Duration::from_secs(30),
                )
                .await
                .map_err(|e| {
                    SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                        kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                        stage: Some("booting".to_string()),
                        tail: String::new(),
                        message: format!(
                            "balloon state could not be normalized before the cold-base dump: \
                             {e} — refusing to run the warm hook in a possibly-starved guest"
                        ),
                    })
                })?;

                let deferred = self.snapshot_deferred(id).await.map_err(|e| {
                    SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                        kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                        stage: Some("booting".to_string()),
                        tail: String::new(),
                        message: format!("cold-base Full capture failed: {e}"),
                    })
                });
                drop(cold_base_keepalive);
                let deferred = deferred?;

                // FAIL-LOUD deflate: a warm hook in a balloon-starved
                // guest (~1.5 GiB effective) is a guaranteed slow OOM-
                // flavored failure 20 minutes later — better to fail in
                // seconds here. `balloon_release` CONFIRMS actual==0
                // before returning; `InvalidSpec` here is ALSO fatal
                // (the device demonstrably existed at inflate time).
                // Only reached when the inflate landed.
                if inflated {
                    if let Err(e) = self.inner.balloon_release(id).await {
                        // The deferred finish must still be settled
                        // (await-never-abort) before surfacing.
                        let seed = deferred.join().await;
                        tracing::warn!(sandbox_id = %id, seed_ok = seed.is_ok(), "balloon deflate failed; seed settled before aborting");
                        return Err(SandboxError::CaptureFailed(
                            engram_core::types::CaptureFailure {
                                kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                                stage: Some("booting".to_string()),
                                tail: String::new(),
                                message: format!(
                                    "balloon deflate after the cold-base dump failed: {e} — \
                                     refusing to run the warm hook in a memory-starved guest"
                                ),
                            },
                        ));
                    }
                }
                Some(deferred)
            } else {
                None
            };

            // Capture-time prewarm hook (image `[warm]`): run the warm
            // command in the live VM BEFORE the (overlay) snapshot, so a
            // process it leaves running (e.g. a `gradle --daemon`) is
            // frozen into the snapshot and every restored session
            // inherits it warm. The command must start its daemon
            // detached and exit; we run it to completion and gate the
            // capture on a clean exit.
            //
            // FAIL-LOUD: a non-zero exit or timeout aborts the capture
            // (and thus the enable) — we never ship a "cold" base snapshot
            // that a `[warm]` hook claimed to warm. The hook runs with the
            // manifest `[env]` MERGED with the resolved `capture_env` (the
            // capture VM's agentd has no durable session env, so both ride
            // the exec). capture_env carries the dev secrets a warm boot
            // needs (e.g. an `op` token, resolved coordinator-side); still
            // NO per-session secrets — those are session policy, injected
            // post-restore, not at capture.
            //
            // `warm_tail` survives a successful hook so a LATER
            // snapshot-phase failure can still report the hook's last
            // output — the diagnosis a `status None` / vsock-lost failure
            // used to lose entirely.
            let mut warm_tail = crate::warm_progress::OutputTail::default();
            // The hook result is NOT `?`-returned before the seed join
            // below — the deferred finish must always be awaited (never
            // dropped/aborted; see `snapshot_deferred`'s contract).
            let hook_result = match &warm {
                Some(warm) => self
                    .run_warm_hook(id, warm, &session_env, &progress)
                    .await
                    .map(Some),
                None => Ok(None),
            };

            // ---- JOIN BARRIER (ADR 0088 addendum) ----
            // Settle the deferred cold-base seed on success AND failure
            // paths. Under its own keepalive: the upload may still have
            // minutes left when a short hook finishes, and the claim
            // lease must not expire while we wait it out.
            let seed_result: Option<Result<SnapshotMetadata, SandboxError>> =
                match deferred_cold_base {
                    Some(deferred) => {
                        let upload_event = engram_core::types::CaptureProgress {
                            phase: engram_core::types::CapturePhase::Snapshot,
                            sandbox_id: Some(id),
                            warm_stage: None,
                            detail: Some("cold-base upload".to_string()),
                            output_tail: String::new(),
                            warm_stages: Vec::new(),
                        };
                        let _ = progress.try_send(upload_event.clone());
                        let upload_keepalive =
                            spawn_leg_keepalive(progress.clone(), upload_event);
                        let joined = deferred.join().await;
                        drop(upload_keepalive);
                        Some(joined)
                    }
                    None => None,
                };

            // Error priority: the hook's failure is the actionable one
            // (it aborts today too); a concurrent seed failure is logged
            // alongside rather than masking it.
            match hook_result {
                Ok(Some(tail)) => warm_tail = tail,
                Ok(None) => {}
                Err(e) => {
                    if let Some(Err(seed_err)) = &seed_result {
                        tracing::warn!(
                            sandbox_id = %id,
                            error = %seed_err,
                            "cold-base seed upload also failed while the warm hook was failing",
                        );
                    }
                    return Err(e);
                }
            }
            let minted_cold_base = match seed_result {
                None => None,
                Some(Ok(meta)) => Some(meta),
                Some(Err(e)) => {
                    return Err(SandboxError::CaptureFailed(
                        engram_core::types::CaptureFailure {
                            kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                            stage: Some("booting".to_string()),
                            tail: warm_tail.render(),
                            message: format!("cold-base capture failed: {e}"),
                        },
                    ))
                }
            };

            if warm.is_some() {
                // Incident 2026-07-10 mitigation: flush the guest's dirty
                // page cache to the (durably captured) disk BEFORE the
                // final snapshot. The memory image is REQUIRED to carry
                // dirty page cache faithfully — the fidelity gate
                // (`unsynced_warm_writes_survive_uffd_restore_and_cache_drop`)
                // asserts it does — but a base snapshot fans out to every
                // session of an image, so its DISK must not depend on
                // that: with the sync, the captured disk stands alone
                // (fsck-able, drop_caches-recoverable) even if a
                // memory-fidelity regression slips through. Warm captures
                // only: the hook is the sole producer of meaningful
                // unsynced state at capture (docker layers, PG pages, git
                // metadata — a warm-less capture's dirty window is bare
                // boot state). Fail-loud like the hook itself: a guest
                // that cannot sync is a guest we must not ship as a base.
                self.sync_guest_fs(id).await?;
            }
            // `phase=snapshot` — the warm hook (if any) is done; pause/flush/
            // chunk is next. Carries the warm tail forward so a live watcher
            // still sees it during the (usually short) snapshot phase.
            let snapshot_event = engram_core::types::CaptureProgress {
                phase: engram_core::types::CapturePhase::Snapshot,
                sandbox_id: Some(id),
                warm_stage: None,
                detail: None,
                output_tail: warm_tail.render(),
                warm_stages: Vec::new(),
            };
            let _ = progress.try_send(snapshot_event.clone());
            // Finding 1: pause/flush/chunk memory + upload a multi-GB
            // state blob to BlobStorage has historically run many minutes
            // on dev-brain with zero progress traffic once the warm hook
            // (if any) is done — same lease-staleness risk as the boot
            // leg above.
            let snapshot_keepalive = spawn_leg_keepalive(progress.clone(), snapshot_event);
            // Capture: pause → flush disk → chunk memory + upload
            // state.bin/sidecar to BlobStorage. This is the portable
            // artifact `create_session` restores from — a Diff overlay
            // when a chain was seeded above (warm Miss's pre-hook Full,
            // or the Hit restore's sparse seed), a Full when neither ran
            // (warm-less, or `NotApplicable`).
            let final_meta = self.snapshot(id).await.map_err(|e| {
                SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                    kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                    stage: None,
                    tail: warm_tail.render(),
                    message: e.to_string(),
                })
            });
            drop(snapshot_keepalive);
            let final_meta = final_meta?;

            let cold_base = match &cold_base_plan {
                ColdBasePlan::NotApplicable => None,
                ColdBasePlan::Hit {
                    content_key,
                    snapshot,
                } => Some(CapturedColdBase {
                    content_key: content_key.clone(),
                    // The (unchanged) existing row's own identity — NOT
                    // `final_meta` (the fresh overlay this attempt just
                    // took). `finalize_capture_job` must not re-upsert
                    // `cold_bases` for a Hit; this field is reported for
                    // symmetry/debuggability, not consumed as a write.
                    snapshot: (**snapshot).clone(),
                    freshly_captured: false,
                    miss_reason: None,
                }),
                ColdBasePlan::Miss {
                    content_key,
                    reason,
                } => Some(CapturedColdBase {
                    content_key: content_key.clone(),
                    // Warm Miss: the pre-hook Full minted above. Warm-less
                    // Miss: no pre-hook capture ran — `final_meta` IS the
                    // cold base (the single Full path, ADR §B3).
                    snapshot: minted_cold_base.unwrap_or_else(|| final_meta.clone()),
                    freshly_captured: true,
                    miss_reason: Some(*reason),
                }),
            };

            Ok(CaptureJobResult {
                snapshot: final_meta,
                cold_base,
            })
        }
        .await;

        // Best-effort teardown: a destroy failure must not mask a
        // successful capture (the metadata is already durable in
        // BlobStorage); the host reconcile pass GCs any leak.
        if let Err(e) = self.destroy(id).await {
            tracing::warn!(
                sandbox_id = %id,
                error = %e,
                "base-snapshot capture: destroy of capture VM failed; reconcile will GC",
            );
        }
        // Drop the capture-egress allowlist now that the VM is gone (no-op if
        // none was registered).
        self.unregister_capture_egress(capture_egress);
        captured
    }

    /// ADR 0080 §C: the `MaterializeImage` engine — validation, the
    /// ≤1-concurrent gate, and the pipeline all live in
    /// [`crate::materialize::run`]; this method owns only the pieces
    /// that are per-host state (the scratch root, the chunk store, the
    /// ambient OCI client, the gate).
    #[tracing::instrument(name = "host.materialize_image", skip_all, fields(image_uri))]
    async fn materialize_image(
        &self,
        image_uri: &str,
        platform_os: &str,
        platform_arch: &str,
        registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth>,
        min_disk_gib: u32,
        progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
    ) -> Result<engram_core::types::MaterializedImage, SandboxError> {
        let Some(scratch) = self.materialize_scratch.clone() else {
            return Err(SandboxError::InvalidSpec(
                "this host has no materialize scratch dir wired (with_materialize_scratch)".into(),
            ));
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return Err(SandboxError::InvalidSpec(
                "this host has no chunk store wired; cannot materialize images".into(),
            ));
        };
        // Enable-flow chunking is bulk work no session waits for.
        let chunk_store = chunk_store.with_class(Class::Background);
        // ≤1 concurrent materialize per host: an image pull + flatten +
        // pack saturates NVMe/network; queueing a second behind it
        // just serializes with extra memory pressure. `try_lock` (not
        // `lock`) so the second caller gets the retryable `busy` and
        // the coordinator re-picks a host.
        let gate = self.materialize_gate.clone();
        let Ok(_permit) = gate.try_lock() else {
            return Err(SandboxError::MaterializeFailed(
                engram_core::types::MaterializeFailure {
                    kind: engram_core::types::MaterializeFailureKind::Busy,
                    message: "a materialize is already running on this host (≤1 concurrent); \
                              retry re-picks a host"
                        .into(),
                },
            ));
        };
        crate::materialize::run(
            self.oci_client.clone(),
            &chunk_store,
            &scratch,
            image_uri,
            platform_os,
            platform_arch,
            registry_auth,
            min_disk_gib,
            progress,
        )
        .await
    }

    #[tracing::instrument(name = "host.restore_base_for_session", skip_all)]
    async fn restore_base_for_session(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<(), SandboxError> {
        // 1. Restore the base snapshot (cross-host materialize +
        //    load_snapshot). The VM comes up running with the bake-time
        //    harness baked into the rootfs at /opt/engram/harness/.
        //    Fresh flavor (ADR 0035 §3): aux bundles swap to the host's
        //    current generation so new sessions run the latest skills.
        let memory_ref = metadata.memory_manifest;
        self.restore_with(
            id,
            metadata,
            /*fresh=*/ true,
            /*fork_disk_at_attach=*/ true,
            selected_mounts,
        )
        .await?;
        // ADR 0045 seed-at-create: the session's RAM == the base
        // manifest at this instant (see `restore_fresh`); seed the
        // chain so the first eviction diffs instead of Full-dumping.
        // FORKED: the base manifest is shared across every session of
        // the image — each chain must own its own lineage. The env
        // merge below dirties pages AFTER tracking started, so the
        // diff stays correct.
        if let Some(memory_ref) = memory_ref {
            let _ = self
                .finisher()
                .seed_chain(id, ChainSeed::Fork(memory_ref))
                .await;
        }

        // ADR 0080: let the captured agentd adopt the agentd bundle
        // generation the fresh flavor just patched into its slot —
        // BEFORE any session state (env, harness, policy) binds, so a
        // re-exec loses nothing. Non-fatal by design: a pre-ADR-0080
        // snapshot (agentd baked into the rootfs, no bundle slot), a
        // bundle staging hiccup, or a typed skew error all degrade to
        // the captured agentd — one generation stale beats a failed
        // create.
        match self.inner.refresh_agent(id).await {
            Ok(engram_core::traits::AgentRefresh::UpToDate) => {}
            Ok(engram_core::traits::AgentRefresh::Restarted) => {
                tracing::info!(
                    sandbox_id = %id,
                    "agentd refreshed onto the host's current bundle generation",
                );
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "agentd refresh degraded; session keeps the captured agentd",
                );
            }
        }

        // Inject the per-session env (manifest env + secrets + session
        // id). The base snapshot is shared, so per-session values can't
        // be baked into it — in cold-create they rode vm_spec.env.
        if !session_env.is_empty() {
            self.inner.merge_session_env(id, session_env).await?;
        }

        // ADR 0021 P1.5: option-D substrate swap retired. The harness
        // travels in the rootfs (manifest `[harness] exec`); there's
        // no per-session harness file to materialize or swap.
        Ok(())
    }

    // ADR 0021 P1.5: `swap_harness_drive` wrapper retired with the
    // trait method.

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        self.inner.set_harness_sink(sink);
    }

    fn set_forge_sink(&self, sink: engram_core::traits::ForgeSink) {
        self.inner.set_forge_sink(sink);
    }

    fn set_upload_sink(&self, sink: engram_core::traits::UploadSink) {
        self.inner.set_upload_sink(sink);
    }

    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        let sandbox_id = policy.sandbox_id;
        let session_id = policy.session_id;

        // The sandbox→session binding is recorded whether or not an
        // egress proxy is attached.
        self.session_bindings.insert(sandbox_id, session_id);

        let Some(egress) = self.egress.as_ref() else {
            if !policy.guest_services.is_empty() || !policy.tunnels.is_empty() {
                return Err(SandboxError::InvalidSpec(
                    "guest services and tunnels require a host egress proxy".into(),
                ));
            }
            // No proxy attached: egress is unfiltered. The coordinator
            // still sends policy frames (it does not know whether a host
            // has a proxy); the sandbox→session binding above is what
            // this frame must record.
            tracing::debug!(
                %sandbox_id,
                %session_id,
                "notify_session_policy: no local egress proxy, binding recorded for Phase B publisher only",
            );
            return Ok(());
        };
        // ADR 0121: the registry lives in the node-local daemon; the
        // apply is a UDS round-trip whose error keeps the ack honest
        // (a policy the daemon refused is not enforced).
        egress
            .apply_policy(policy)
            .await
            .map_err(|e| SandboxError::InvalidSpec(format!("apply egress policy: {e}")))?;
        tracing::debug!(
            %sandbox_id,
            %session_id,
            "egress policy registered with the node-local daemon",
        );
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        let mut ids: std::collections::HashSet<_> = self.inner.list().await?.into_iter().collect();
        ids.extend(self.teardowns.keys());
        Ok(ids.into_iter().collect())
    }

    /// ADR 0035 amendment D2: proxy to the wrapped backend — FC and VZ override
    /// the trait default with their live attachment maps; Process
    /// inherits the empty default (it keeps no attachment record).
    async fn aux_bundles_all(&self) -> Vec<engram_core::types::sandbox::SandboxAuxBundles> {
        self.inner.aux_bundles_all().await
    }

    /// ADR 0068: proxy to the wrapped backend, same as every other
    /// capability method here — `FirecrackerBackend` overrides the
    /// trait default with the ground-truth manifest check; VZ/Process
    /// inherit the default (list-membership mirror). The pool itself
    /// tracks no independent liveness signal worth adding here.
    async fn probe_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        self.inner.probe_sandbox(id).await
    }

    async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
        self.inner.guest_endpoints(id).await
    }

    /// ADR 0014 follow-up: forward to inner. Without this, the
    /// SandboxBackend trait's default impl (return Ok(7681)) would
    /// run instead and the FC backend's actual vsock StartShell RPC
    /// to in-VM agentd would never fire — host's proxy_shell would
    /// then dial port 7681 blind and hit Connection refused whenever
    /// the bake's init didn't auto-start ttyd or the warm-restore
    /// stripped the listening socket.
    async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
        self.inner.start_shell(id).await
    }

    /// ADR 0080: forward to inner, exactly as `start_shell` does — without
    /// this the trait default (`Ok(UpToDate)`) would run and the FC
    /// backend's actual vsock RefreshAgent RPC to in-VM agentd would never
    /// fire, silently pinning every session to its capture-time agentd.
    async fn refresh_agent(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::AgentRefresh, SandboxError> {
        self.inner.refresh_agent(id).await
    }

    /// ADR 0065: forward to inner, exactly as `start_shell` does — without
    /// this the trait default (`Ok(5900)`) would run and the FC/VZ backend's
    /// actual vsock StartBrowser RPC to in-VM agentd would never fire, so
    /// the host's `proxy_vnc` would dial a port nothing started.
    async fn start_browser(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        self.inner.start_browser(id).await
    }

    /// ADR 0065: forward to inner (the trait default is a no-op).
    async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.stop_browser(id).await
    }

    /// ADR 0085: forward to inner, exactly as `start_browser` does — without
    /// this the trait default (`Ok(13337)`) would run and the FC/VZ backend's
    /// actual vsock StartIde RPC to in-VM agentd would never fire, so the
    /// orchestrator would relay to a port nothing started.
    async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
        self.inner.start_ide(id).await
    }

    /// ADR 0085: forward to inner (the trait default is a no-op).
    async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.stop_ide(id).await
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use engram_host_core::ChunkIndex;
    use engram_sandbox_process::ProcessBackend;

    /// A lazy restore awaits the staging task before the handler starts,
    /// so the cleanup after the restore sees a task that has completed.
    #[tokio::test]
    async fn staged_copies_are_removed_after_the_restore_awaited_the_task() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let r = engram_core::types::manifest::ManifestRef::new();
        let path = engram_sandbox_firecracker::staged_manifest_path(tmp.path(), id, &r);
        let staged_path = path.clone();
        let mut staging = AbortTaskOnDrop(tokio::spawn(async move {
            fs::write(&staged_path, b"{}").await.unwrap();
        }));
        (&mut staging.0).await.unwrap();
        assert!(path.exists());

        remove_staged_manifests(Some(staging), tmp.path(), id, &[r]).await;
        assert!(!path.exists(), "the staged copy is removed");
    }

    /// 2026-08-03 `chain_poisoned` alert regression: once the SIGTERM
    /// capture quiesce is raised, `capture_phase` must refuse BEFORE the
    /// write-ahead invalidate — the durable chain-head record (the
    /// successor's rehydrate seed) survives the refusal, so the surviving
    /// VM keeps its Diff-capable chain instead of paying a FULL re-chunk.
    #[tokio::test]
    async fn quiesced_capture_refuses_before_invalidating_the_chain_head() {
        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        let pooled = PooledBackend::new(inner).with_checkpoint_dir(tmp.path().join("checkpoints"));
        let id = SandboxId::new();

        // A durable chain-head record, as a survivor's capture leaves it.
        let store = pooled.chain_heads.as_ref().unwrap();
        store
            .persist(crate::checkpoint::ChainHeadRecord {
                sandbox_id: id,
                manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                session_id: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        pooled.quiesce_captures_for_shutdown();
        let err = match pooled.capture_phase(id, CaptureUser::EvictionCapture).await {
            Ok(_) => panic!("a quiesced capture must refuse"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("capture refused"),
            "unexpected error: {err}"
        );
        // The refusal consumed nothing: the record is still the seed, and
        // the capture lock was released.
        assert!(
            crate::checkpoint::ChainHeadRecord::load(store.dir(), id)
                .await
                .is_some(),
            "the chain-head record must survive a quiesced-capture refusal",
        );
        assert!(!pooled.capture_in_flight(id));
    }

    /// The CaptureDrain ladder stage: an in-flight capture (a held
    /// capture lock) past the deadline counts as a straggler; a released
    /// lock drains clean.
    #[tokio::test]
    async fn shutdown_capture_drain_counts_stragglers_and_drains_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        let pooled = PooledBackend::new(inner);
        pooled.quiesce_captures_for_shutdown();
        let id = SandboxId::new();

        // A capture in flight that outlives the deadline is a straggler.
        let guard = pooled
            .capture_gate(id)
            .serialize(id, CaptureUser::PeriodicCheckpoint)
            .await;
        let stragglers = pooled
            .drain_captures_for_shutdown(std::time::Duration::from_millis(50))
            .await;
        assert_eq!(stragglers, 1, "a held capture lock must count");

        // A capture that completes (the lock releases) drains clean, and
        // the drain leaves the lock free behind it.
        drop(guard);
        let stragglers = pooled
            .drain_captures_for_shutdown(std::time::Duration::from_secs(5))
            .await;
        assert_eq!(stragglers, 0);
        assert!(!pooled.capture_in_flight(id));
    }

    #[tokio::test]
    async fn guest_services_and_tunnels_require_the_local_egress_proxy() {
        use engram_core::types::integration::{GuestService, SessionTunnel};
        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        let pooled = PooledBackend::new(inner);

        let cases = [
            (vec![GuestService::new("gcp.gce_metadata")], Vec::new()),
            (
                Vec::new(),
                vec![SessionTunnel {
                    id: "prod-readonly".into(),
                    connector: "gcp.cloud_sql".into(),
                    config_json: "{}".into(),
                    mint_source: None,
                }],
            ),
        ];
        for (guest_services, tunnels) in cases {
            let error = pooled
                .notify_session_policy(SessionEgressPolicy {
                    session_id: SessionId::new(),
                    sandbox_id: SandboxId::new(),
                    guest_ip: std::net::Ipv4Addr::LOCALHOST,
                    network_allow_hosts: Vec::new(),
                    network_allow_host_patterns: Vec::new(),
                    allow_all: false,
                    secrets: Vec::new(),
                    injects: Vec::new(),
                    observes: Vec::new(),
                    guest_services,
                    tunnels,
                    apps: Vec::new(),
                    secret_mode: engram_core::types::image::SecretMode::Broker,
                })
                .await
                .expect_err("guest gateway features must fail closed without an egress proxy");

            assert!(
                error
                    .to_string()
                    .contains("guest services and tunnels require a host egress proxy"),
                "{error}",
            );
        }
    }

    /// ADR 0101 B: the adaptive candidacy glue over the pacing maps —
    /// the pure controller (`next_epoch_after`) is tested in
    /// `checkpoint.rs`; this pins the map-driven wiring around it: a
    /// dirt-heavy sample earns the floor cadence, a trickle keeps the
    /// backstop, a sample-less sandbox takes the short first epoch, and a
    /// fresh capture is never due.
    #[tokio::test]
    async fn adaptive_candidates_honor_pacing_and_backstop() {
        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        let pooled = PooledBackend::new(inner).with_checkpoint_dir(tmp.path().join("checkpoints"));
        let cfg = crate::checkpoint::CheckpointConfig {
            interval: Some(std::time::Duration::from_secs(600)),
            min_interval: std::time::Duration::from_secs(30),
            target_epoch_bytes: 256 * 1024 * 1024,
            first_interval: std::time::Duration::from_secs(120),
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let sample = |dirty: Option<u64>| EpochPacingSample {
            dirty_bytes: dirty,
            epoch: std::time::Duration::from_secs(60),
        };
        let bind = |age_secs: i64, pacing: Option<EpochPacingSample>| {
            let sb = SandboxId::new();
            let session = SessionId::new();
            pooled.session_bindings.insert(sb, session);
            pooled
                .last_snapshot_unix_ms
                .insert(sb, now_ms - age_secs * 1000);
            if let Some(p) = pacing {
                pooled.checkpoint_pacing.insert(sb, p);
            }
            sb
        };

        // Dirt-heavy (2× target in 60s) → floor cadence (30s): due at 40s.
        let busy_due = bind(40, Some(sample(Some(2 * cfg.target_epoch_bytes))));
        // Same rate but captured 10s ago → not due yet.
        let busy_fresh = bind(10, Some(sample(Some(2 * cfg.target_epoch_bytes))));
        // A trickle (1 KiB/epoch) → backstop cadence: not due at 40s.
        let idle_trickle = bind(40, Some(sample(Some(1024))));
        // No pacing sample → the first epoch (120s): not due at 40s, due
        // at 130s.
        let unsampled_fresh = bind(40, None);
        let unsampled_old = bind(130, None);

        let due: std::collections::HashSet<SandboxId> = pooled
            .checkpoint_candidates_adaptive(&cfg)
            .into_iter()
            .map(|(sb, _)| sb)
            .collect();
        assert!(
            due.contains(&busy_due),
            "dirt-heavy at 40s is due (30s floor)"
        );
        assert!(!due.contains(&busy_fresh), "fresh capture is never due");
        assert!(
            !due.contains(&idle_trickle),
            "a trickle keeps the 600s backstop"
        );
        assert!(
            !due.contains(&unsampled_fresh),
            "the first epoch is not over at 40s"
        );
        assert!(
            due.contains(&unsampled_old),
            "the first epoch ends before the backstop"
        );
    }

    /// ADR 0091 probe gate (engrams review, #835 round 2): one probe
    /// slot per sandbox — claim, re-claim refused, release, re-claim ok.
    #[tokio::test]
    async fn dead_probe_gate_is_one_slot_per_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        let pooled = PooledBackend::new(inner);
        let sb = SandboxId::new();
        assert!(pooled.try_begin_dead_probe(sb));
        assert!(!pooled.try_begin_dead_probe(sb), "second claim refused");
        assert!(
            pooled.try_begin_dead_probe(SandboxId::new()),
            "other sandboxes unaffected"
        );
        pooled.end_dead_probe(sb);
        assert!(pooled.try_begin_dead_probe(sb), "released slot reclaimable");
    }

    /// ADR 0019 / telemetry restoration (#526): the stats-file → outcome
    /// mapping `restore()` drives `engram_resume_prefault_total` off.
    /// Covers the three labeled outcomes, the alarm shape (absent file),
    /// and defensive handling of an unparseable file.
    #[test]
    fn classify_prefault_outcome_covers_all_three_labels() {
        // Absent file (read failed / never written) => the alarm.
        assert_eq!(
            classify_prefault_outcome(None),
            PrefaultOutcome::StatsMissing
        );

        // trace_loaded: false => expected "nothing to replay" case.
        let no_trace = br#"{"trace_loaded":false,"chunks_in_trace":0,"installed":0,"skipped":0,"duration_ms":3}"#;
        assert_eq!(
            classify_prefault_outcome(Some(no_trace)),
            PrefaultOutcome::NoTrace
        );

        // trace_loaded: true => replayed, carrying installed/skipped.
        let replayed = br#"{"trace_loaded":true,"chunks_in_trace":10,"installed":8,"skipped":2,"duration_ms":42}"#;
        assert_eq!(
            classify_prefault_outcome(Some(replayed)),
            PrefaultOutcome::Replayed {
                installed: 8,
                skipped: 2
            }
        );

        // Corrupt/unparseable bytes => treated the same as absent (a
        // handler that died mid-write is just as much an alarm as one
        // that never wrote at all).
        assert_eq!(
            classify_prefault_outcome(Some(b"not json")),
            PrefaultOutcome::StatsMissing
        );

        // Extra/superset fields (the prefault-admission-control
        // convergence schema, e.g. a `trace_source` field) must not
        // break parsing — #[serde(default)] + no `deny_unknown_fields`.
        let superset = br#"{"trace_loaded":true,"chunks_in_trace":1,"installed":1,"skipped":0,"duration_ms":1,"trace_source":"canonical"}"#;
        assert_eq!(
            classify_prefault_outcome(Some(superset)),
            PrefaultOutcome::Replayed {
                installed: 1,
                skipped: 0
            }
        );
    }

    /// Review finding 1 regression test: a file that lands mid-poll
    /// (mirroring the uffd-handler's background prefault write racing
    /// resume-return) must be picked up, not misclassified as
    /// `stats_missing` on the first miss.
    #[tokio::test]
    async fn prefault_stats_retry_picks_up_a_late_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefault-stats.json");
        let write_path = path.clone();
        // Simulate the handler's background thread: the file doesn't
        // exist yet when the poll starts, and lands ~30ms later (well
        // inside the poll's bound but after several immediate misses).
        // ATOMIC temp+rename, exactly like the real handler — a plain
        // `write` here let the 5ms poll observe a created-but-empty file
        // under parallel-test load (one observed flake, 2026-07-17).
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let tmp = write_path.with_extension("json.tmp");
            tokio::fs::write(
                &tmp,
                b"{\"trace_loaded\":true,\"installed\":3,\"skipped\":0}",
            )
            .await
            .unwrap();
            tokio::fs::rename(&tmp, &write_path).await.unwrap();
        });
        let bytes = read_prefault_stats_with_retry_bounded(
            &path,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            classify_prefault_outcome(bytes.as_deref()),
            PrefaultOutcome::Replayed {
                installed: 3,
                skipped: 0
            }
        );
    }

    /// A file that never appears within the bound still classifies as
    /// `stats_missing` — the real alarm case (handler died / never
    /// fired) must survive the race-tolerance fix, just no longer fire
    /// on the first instant.
    #[tokio::test]
    async fn prefault_stats_retry_times_out_to_stats_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefault-stats.json");
        let bytes = read_prefault_stats_with_retry_bounded(
            &path,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(30),
        )
        .await;
        assert_eq!(
            classify_prefault_outcome(bytes.as_deref()),
            PrefaultOutcome::StatsMissing
        );
    }

    /// Review finding 1: a leg wrapped by `spawn_leg_keepalive` must keep
    /// resending the SAME event on the keepalive interval until the
    /// guard is dropped, and must stop immediately once it is. This is
    /// the mechanism that keeps the coordinator's fenced `enable_jobs`
    /// write (== the capture-claim lease renewal, since the blind ticker
    /// was deleted) alive during the boot/snapshot legs, which have no
    /// `[warm]`-hook progress traffic of their own to drive it.
    #[tokio::test]
    async fn leg_keepalive_resends_until_dropped() {
        // SAFETY: `cargo nextest run` (the repo's enforced test runner —
        // see `just check`) gives every test its own process, so mutating
        // process env here can't race a sibling test.
        std::env::set_var("ENGRAM_CAPTURE_KEEPALIVE_SECS", "1");

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let event = engram_core::types::CaptureProgress {
            phase: engram_core::types::CapturePhase::Boot,
            sandbox_id: None,
            warm_stage: None,
            detail: None,
            output_tail: "boot-in-progress".into(),
            warm_stages: Vec::new(),
        };
        let guard = spawn_leg_keepalive(tx, event.clone());

        // Two resends within the (test-shrunk) 1s interval.
        for _ in 0..2 {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("keepalive must resend within the timeout")
                .expect("channel must stay open while the guard is alive");
            assert_eq!(got.phase, engram_core::types::CapturePhase::Boot);
            assert_eq!(got.output_tail, "boot-in-progress");
        }

        drop(guard);
        while rx.try_recv().is_ok() {
            // drain anything already in flight before the drop landed
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        assert!(
            rx.try_recv().is_err(),
            "no further resend once the guard is dropped"
        );
    }

    fn live_spec(image: &str) -> SandboxSpec {
        SandboxSpec {
            image: image.into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
            swap_mib: None,
            swap_source: None,
            swap_manifest: None,
        }
    }

    #[tokio::test]
    async fn base_capture_propagates_guest_death() {
        struct DeadGuest(Arc<std::sync::atomic::AtomicBool>);
        #[async_trait]
        impl SandboxBackend for DeadGuest {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn wait_agent_ready(&self, _: SandboxId) -> Result<(), SandboxError> {
                Err(SandboxError::Vm(
                    "guest died before agentd dialed ready port: Kernel panic: capture-tail-marker"
                        .into(),
                ))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                unreachable!("dead guest must not execute a warm hook")
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!("dead guest must not be captured")
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!("dead guest has no snapshot")
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                unreachable!("cold boot")
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                unreachable!("base capture does not start a harness")
            }
        }
        let destroyed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pooled = PooledBackend::new(Arc::new(DeadGuest(destroyed.clone())));
        let (progress, _rx) = tokio::sync::mpsc::channel(16);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("dead-guest"),
                    warm: None,
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress,
            ),
        )
        .await
        .expect("capture must fail promptly")
        .unwrap_err()
        .to_string();
        assert!(error.contains("guest died before agentd dialed"), "{error}");
        assert!(error.contains("capture-tail-marker"), "{error}");
        assert!(destroyed.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Review finding 1: a slow cold boot (agentd-ready takes a while)
    /// must keep emitting `phase=boot` `CaptureProgress` on the keepalive
    /// interval — not just the one frame at the very start — or the
    /// coordinator's fenced write (== the capture-claim lease renewal)
    /// goes stale and a peer re-claims mid-boot. No `[warm]` hook here:
    /// this specifically isolates the BOOT leg's own keepalive from
    /// `run_warm_hook`'s (already covered by other tests).
    #[tokio::test]
    async fn slow_boot_keeps_emitting_boot_phase_progress() {
        // SAFETY: see the ENGRAM_WARM_STALL_SECS precedent elsewhere in
        // this module — nextest gives every test its own process.
        std::env::set_var("ENGRAM_CAPTURE_KEEPALIVE_SECS", "1");

        struct Probe {
            staging: PathBuf,
        }
        struct SlowBootMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for SlowBootMock {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn wait_agent_ready(&self, _: SandboxId) -> Result<(), SandboxError> {
                // Longer than 2 keepalive ticks (1s each): proves the
                // boot leg's own keepalive fires WHILE we're still
                // waiting, not just the single frame sent before this
                // call.
                tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
                Ok(())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                unreachable!("no [warm] hook in this test — exec_stream must not be called")
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(SlowBootMock(probe));
        let pooled = PooledBackend::new(inner).with_chunk_store(cs);
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        pooled
            .build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("slow-boot"),
                    warm: None,
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            )
            .await
            .expect("capture with no warm hook must succeed");

        let boot_events = {
            let mut n = 0;
            while let Ok(ev) = progress_rx.try_recv() {
                if ev.phase == engram_core::types::CapturePhase::Boot {
                    n += 1;
                }
            }
            n
        };
        assert!(
            boot_events >= 2,
            "expected the boot leg's keepalive to resend at least once \
             during a 2.5s wait_agent_ready with a 1s interval, got {boot_events} boot events"
        );
    }

    /// Review finding 3: `Exit(None)` (the child died to a signal) MUST
    /// NOT be unconditionally labeled `WarmGlobalTimeout` — that's only
    /// correct when agentd's `timeout_ms` in-guest backstop actually
    /// fired. A hook killed by something else (guest OOM, a manual
    /// `kill -9`) well before its declared `timeout_secs` budget must
    /// classify as the distinct, unattributed `WarmKilled` kind, or an
    /// operator gets steered to raise `timeout_secs` for a failure that
    /// timeout had nothing to do with.
    #[tokio::test]
    async fn warm_hook_early_signal_kill_is_not_misclassified_as_global_timeout() {
        struct EarlySignalKillMock;
        #[async_trait]
        impl SandboxBackend for EarlySignalKillMock {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                // Exit(None) arrives almost immediately — nowhere near
                // the 30s `timeout_secs` budget below.
                let events =
                    futures::stream::iter(vec![engram_core::types::sandbox::ExecEvent::Exit(None)]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-early-kill".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!("a killed [warm] hook must fail the capture before snapshot runs")
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(EarlySignalKillMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous: proves the classification isn't just "we're near
            // the deadline", it's "we're nowhere near it".
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-early-kill"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must not hang");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmKilled,
            "an Exit(None) far from the timeout budget must not be labeled WarmGlobalTimeout"
        );
    }

    /// Issue #539: a `[warm]` hook that emits one `start` progress line and
    /// then goes silent (no more stdout/stderr, no `Exit`) must be killed
    /// within the (test-shrunk) stall budget — not the old single opaque
    /// global timeout, which this test sets generously (30s) precisely so
    /// a pass proves the STALL path fired, not the global-timeout backstop.
    /// The resulting `SandboxError::CaptureFailed` must name the open
    /// stage and carry the hook's own output in its tail; the same
    /// information must also have reached a live `CaptureProgress` event
    /// on the `progress` channel before the terminal failure.
    #[tokio::test]
    async fn warm_hook_stall_fails_capture_with_stage_and_tail() {
        use futures::StreamExt;

        // SAFETY: `cargo nextest run` (the repo's enforced test runner —
        // see `just check`) gives every test its own process, so mutating
        // process env here can't race a sibling test.
        std::env::set_var("ENGRAM_WARM_STALL_SECS", "1");

        struct SilentAfterStartMock;
        #[async_trait]
        impl SandboxBackend for SilentAfterStartMock {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                let line = "::engram-warm:: event=start stage=deps-up msg=installing deps\n";
                let events =
                    futures::stream::iter(vec![engram_core::types::sandbox::ExecEvent::Stdout(
                        bytes::Bytes::from(line),
                    )])
                    .chain(futures::stream::pending());
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-stall".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!("a stalled [warm] hook must fail the capture before snapshot runs")
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(SilentAfterStartMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous global timeout: the test proves the STALL path (1s,
            // via ENGRAM_WARM_STALL_SECS above), not this backstop.
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-stall"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must fail within the stall budget, not hang past the test timeout");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmStall
        );
        assert_eq!(failure.stage.as_deref(), Some("deps-up"));
        assert!(
            failure.tail.contains("deps-up"),
            "tail must carry the hook's own output: {}",
            failure.tail
        );

        let mut saw_live_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("deps-up") {
                saw_live_stage = true;
            }
        }
        assert!(
            saw_live_stage,
            "expected a live CaptureProgress event naming the stage before the terminal failure"
        );
    }

    /// Issue #539/#563 review: a stage's own `deadline_secs` must be
    /// enforced even when the hook is chatty the whole time — continuous
    /// heartbeats keep the STALL clock reset forever, so a pass here can
    /// only be explained by the stage-deadline arm of the watchdog, not
    /// the stall detector (which this test sets generously precisely to
    /// rule it out). The resulting `SandboxError::CaptureFailed` must
    /// carry the `WarmStageDeadline` kind, the open stage's name, and the
    /// hook's own output in its tail; the same stage must also have
    /// reached a live `CaptureProgress` event before the terminal
    /// failure.
    #[tokio::test]
    async fn warm_hook_stage_deadline_fails_capture_with_stage_and_tail() {
        use futures::StreamExt;

        // SAFETY: see the stall test above — nextest gives every test its
        // own process, so mutating process env here can't race a sibling
        // test. Generous on purpose: this test proves the STAGE-DEADLINE
        // path (1s, via the hook's own `deadline_secs=1`), not the stall
        // backstop.
        std::env::set_var("ENGRAM_WARM_STALL_SECS", "30");

        struct ChattyStuckStageMock;
        #[async_trait]
        impl SandboxBackend for ChattyStuckStageMock {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                let start =
                    "::engram-warm:: event=start stage=wedged-stage deadline_secs=1 msg=starting\n";
                let heartbeat =
                    "::engram-warm:: event=heartbeat stage=wedged-stage msg=still going\n";
                let events =
                    futures::stream::once(
                        async move { ExecEvent::Stdout(bytes::Bytes::from(start)) },
                    )
                    .chain(futures::stream::unfold((), move |()| async move {
                        // Fires far faster than both the 1s stage deadline and
                        // the 30s stall budget above, so the stall clock never
                        // comes close to expiring — only the stage deadline can
                        // explain a failure here.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        Some((ExecEvent::Stdout(bytes::Bytes::from(heartbeat)), ()))
                    }));
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-stage-deadline".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!(
                    "a stage-deadline-killed [warm] hook must fail the capture before snapshot runs"
                )
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(ChattyStuckStageMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous global timeout: the test proves the STAGE DEADLINE
            // path (1s, via the hook's own `deadline_secs=1`), not this
            // backstop.
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-stage-deadline"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must fail within the stage-deadline budget, not hang past the test timeout");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmStageDeadline
        );
        assert_eq!(failure.stage.as_deref(), Some("wedged-stage"));
        assert!(
            failure.tail.contains("wedged-stage"),
            "tail must carry the hook's own output: {}",
            failure.tail
        );

        let mut saw_live_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("wedged-stage") {
                saw_live_stage = true;
            }
        }
        assert!(
            saw_live_stage,
            "expected a live CaptureProgress event naming the stage before the terminal failure"
        );
    }

    /// Review finding 5: a hook that streams newline-free output (gradle
    /// rich-console `\r` redraws, binary noise) must not grow
    /// `pending_stdout` unbounded — and, once the cap drops the
    /// unparseable noise, a real `::engram-warm::` line arriving right
    /// after must still parse cleanly (the cap doesn't wedge future
    /// parsing).
    #[tokio::test]
    async fn warm_hook_newline_free_noise_does_not_wedge_progress_parsing() {
        struct Probe {
            staging: PathBuf,
        }
        struct NoiseThenLineMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for NoiseThenLineMock {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                // Well past OutputTail::DEFAULT_CAP_BYTES (16 KiB), no
                // newline anywhere — the exact shape that grew
                // `pending_stdout` unbounded pre-fix.
                let noise = vec![b'x'; 64 * 1024];
                let events = futures::stream::iter(vec![
                    ExecEvent::Stdout(bytes::Bytes::from(noise)),
                    ExecEvent::Stdout(bytes::Bytes::from(
                        "::engram-warm:: event=start stage=after-noise\n",
                    )),
                    ExecEvent::Stdout(bytes::Bytes::from(
                        "::engram-warm:: event=done stage=after-noise\n",
                    )),
                    ExecEvent::Exit(Some(0)),
                ]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-noise".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(NoiseThenLineMock(probe));
        let pooled = Arc::new(PooledBackend::new(inner).with_chunk_store(cs));
        let warm = WarmConfig {
            command: vec!["true".into()],
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-noise"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must not hang on unbounded newline-free output");

        result.expect("a hook that exits 0 must succeed even after newline-free noise");

        let mut saw_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("after-noise") {
                saw_stage = true;
            }
        }
        assert!(
            saw_stage,
            "the real progress line after the noise must still parse"
        );
    }

    /// Issue #539: a `[warm]` hook that emits two stages and exits 0 must
    /// succeed, and the ordered `CaptureProgress` events observed on the
    /// channel must carry the full (closed) stage history.
    #[tokio::test]
    async fn warm_hook_two_stages_then_exit_zero_succeeds_with_ordered_progress() {
        struct Probe {
            staging: PathBuf,
        }
        struct TwoStageMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for TwoStageMock {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                let lines = [
                    "::engram-warm:: event=start stage=deps-up\n",
                    "::engram-warm:: event=done stage=deps-up\n",
                    "::engram-warm:: event=start stage=migrations\n",
                    "::engram-warm:: event=done stage=migrations\n",
                ]
                .concat();
                let events = futures::stream::iter(vec![
                    ExecEvent::Stdout(bytes::Bytes::from(lines)),
                    ExecEvent::Exit(Some(0)),
                ]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-two-stage".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(TwoStageMock(probe.clone()));
        let pooled = Arc::new(PooledBackend::new(inner).with_chunk_store(cs));

        let warm = WarmConfig {
            command: vec!["true".into()],
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        pooled
            .build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-two-stage"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            )
            .await
            .expect("a two-stage hook that exits 0 must succeed");

        let mut events = Vec::new();
        while let Ok(ev) = progress_rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.is_empty(),
            "expected at least one CaptureProgress event"
        );
        // The last warm-phase event's stage history must show both stages
        // closed with outcome `Done`, in emission order.
        let last_warm = events
            .iter()
            .rev()
            .find(|e| e.phase == engram_core::types::CapturePhase::Warm)
            .expect("at least one phase=warm event");
        assert_eq!(last_warm.warm_stages.len(), 2);
        assert_eq!(last_warm.warm_stages[0].name, "deps-up");
        assert_eq!(
            last_warm.warm_stages[0].outcome,
            engram_core::types::WarmStageOutcome::Done
        );
        assert_eq!(last_warm.warm_stages[1].name, "migrations");
        assert_eq!(
            last_warm.warm_stages[1].outcome,
            engram_core::types::WarmStageOutcome::Done
        );
    }

    /// ADR 0084 §B: `build_base_snapshot`'s cold-base/warm-overlay stage
    /// plan. `ColdBaseMock` tracks `create`/`restore`/`snapshot` call
    /// counts so each test asserts the EXACT stage sequence its
    /// `ColdBasePlan` should drive, without needing to fake FC's real
    /// `memory.diff` sparse-range format (that fidelity belongs to the
    /// FC integration test, not this unit suite) — this mock has no
    /// `checkpoint_dir` wired, so every `snapshot()` call routes through
    /// `inner.snapshot()` (never `inner.snapshot_diff()`); what's under
    /// test here is purely the STAGE SEQUENCE (create-vs-restore,
    /// snapshot call count, the hard-error gate, and the result shape),
    /// not the Full/Diff cost distinction itself.
    mod cold_base_stage_plan {
        use super::*;
        use engram_core::traits::sandbox::BuildBaseSnapshotRequest;
        use engram_core::types::capture_job::{ColdBaseMissReason, ColdBasePlan};
        use engram_core::types::image::WarmConfig;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct ColdBaseMock {
            staging: PathBuf,
            create_calls: Arc<AtomicUsize>,
            restore_calls: Arc<AtomicUsize>,
            snapshot_calls: Arc<AtomicUsize>,
            destroy_calls: Arc<AtomicUsize>,
            supports_diff: bool,
        }

        impl ColdBaseMock {
            fn new(staging: PathBuf, supports_diff: bool) -> Self {
                Self {
                    staging,
                    create_calls: Arc::new(AtomicUsize::new(0)),
                    restore_calls: Arc::new(AtomicUsize::new(0)),
                    snapshot_calls: Arc::new(AtomicUsize::new(0)),
                    destroy_calls: Arc::new(AtomicUsize::new(0)),
                    supports_diff,
                }
            }
        }

        #[async_trait]
        impl SandboxBackend for ColdBaseMock {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                self.create_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                // A trivial always-succeeds hook (mirrors `command =
                // ["true"]` — no `::engram-warm::` progress lines, just a
                // clean exit).
                use engram_core::types::sandbox::ExecEvent;
                let events = futures::stream::iter(vec![ExecEvent::Exit(Some(0))]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-cold-base-mock".into(),
                    events: Box::pin(events),
                })
            }
            fn supports_diff_checkpoints(&self) -> bool {
                self.supports_diff
            }
            async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![9u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging.join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                self.restore_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                self.destroy_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        fn warm_config() -> WarmConfig {
            WarmConfig {
                command: vec!["true".into()],
                timeout_secs: Some(30),
                workdir: None,
                env: Vec::new(),
                network: None,
            }
        }

        fn fake_snapshot() -> SnapshotMetadata {
            SnapshotMetadata {
                id: engram_core::SnapshotId::new(),
                size_bytes: 4096,
                created_at: chrono::Utc::now(),
                image_version: "cold-base:1".into(),
                swap_manifest: None,
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                base_disk_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
                paused_at: None,
                peer_hints: Vec::new(),
            }
        }

        async fn pooled_over(mock: ColdBaseMock) -> Arc<PooledBackend> {
            let tmp = tempfile::tempdir().unwrap();
            let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
            );
            let cs = engram_chunk_store::ChunkStore::new(blob);
            let inner: Arc<dyn SandboxBackend> = Arc::new(mock);
            let p = Arc::new(PooledBackend::new(inner).with_chunk_store(cs));
            std::mem::forget(tmp);
            p
        }

        /// `NotApplicable` (non-FC host / warm-less image): single-stage
        /// path — `create` once, `restore` never, exactly ONE `snapshot`
        /// call, and the result carries no `cold_base`.
        #[tokio::test]
        async fn not_applicable_is_single_stage_with_no_cold_base() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-not-applicable"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::NotApplicable,
                    },
                    progress_tx,
                )
                .await
                .expect("single-stage capture must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(counters.snapshot_calls.load(Ordering::SeqCst), 1);
            assert!(result.cold_base.is_none());
        }

        /// Warm-less image on an FC host with `ColdBasePlan::Miss`: still
        /// single-stage (ONE `snapshot` call — the single Full path IS
        /// the cold base, ADR §B3) — but the result now carries
        /// `cold_base` (so a LATER warm image with the same content can
        /// reuse it), `freshly_captured: true`, with the SAME snapshot
        /// metadata as the artifact.
        #[tokio::test]
        async fn warm_less_miss_is_single_stage_but_reports_cold_base_identity() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-warm-less-miss"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-1".into(),
                            reason: ColdBaseMissReason::NoCandidate,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("warm-less miss must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                1,
                "warm-less has no hook to protect a pre-hook cold-base capture for"
            );
            let cb = result.cold_base.expect("must report cold-base identity");
            assert_eq!(cb.content_key, "ck-1");
            assert!(cb.freshly_captured);
            assert_eq!(cb.miss_reason, Some(ColdBaseMissReason::NoCandidate));
            assert_eq!(
                cb.snapshot.id, result.snapshot.id,
                "the single Full capture IS both the cold base and the artifact"
            );
        }

        /// Warm image on a MISS: two `snapshot` calls (the pre-hook Full
        /// cold-base capture, then the post-hook overlay) and exactly one
        /// `create` — no cold boot avoided (there was nothing to reuse),
        /// but the cold base is minted BEFORE the hook runs.
        #[tokio::test]
        async fn warm_miss_takes_a_pre_hook_snapshot_then_the_overlay() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-warm-miss"),
                        warm: Some(warm_config()),
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-2".into(),
                            reason: ColdBaseMissReason::ChunksMissing,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("warm miss must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                2,
                "a WARM miss mints its own cold base before the hook, then takes the overlay"
            );
            let cb = result.cold_base.expect("must report the minted cold base");
            assert!(cb.freshly_captured);
            assert_eq!(cb.miss_reason, Some(ColdBaseMissReason::ChunksMissing));
            assert_ne!(
                cb.snapshot.id, result.snapshot.id,
                "the pre-hook cold base and the post-hook overlay are DISTINCT captures"
            );
        }

        /// A `Hit`: `restore` instead of `create` — no cold boot at all —
        /// and exactly one `snapshot` call (the overlay). The reported
        /// `cold_base` echoes the candidate's OWN (unchanged) identity,
        /// not the fresh overlay, and `freshly_captured` is `false`.
        #[tokio::test]
        async fn hit_restores_instead_of_creating() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);
            let candidate = fake_snapshot();
            let candidate_id = candidate.id;

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-hit"),
                        warm: Some(warm_config()),
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Hit {
                            content_key: "ck-3".into(),
                            snapshot: Box::new(candidate),
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("hit path must succeed");

            assert_eq!(
                counters.create_calls.load(Ordering::SeqCst),
                0,
                "a Hit must never cold-boot"
            );
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                1,
                "only the post-hook overlay — no pre-hook capture needed, we already have one"
            );
            let cb = result
                .cold_base
                .expect("must report the cold-base identity");
            assert!(!cb.freshly_captured);
            assert_eq!(cb.miss_reason, None);
            assert_eq!(
                cb.snapshot.id, candidate_id,
                "a Hit echoes the EXISTING candidate's identity, not a new capture"
            );
            assert_ne!(
                cb.snapshot.id, result.snapshot.id,
                "the candidate's identity is distinct from the fresh overlay artifact"
            );
        }

        /// ADR decision 11: a `Hit`/`Miss` plan on a backend that can't
        /// actually diff is a placement bug — hard error, no cold boot
        /// ever attempted, never a silent single-stage fallback.
        #[tokio::test]
        async fn capability_mismatch_hard_errors_before_booting_anything() {
            let staging = tempfile::tempdir().unwrap();
            // supports_diff = false: this "FC" host has dirty-page
            // tracking disabled (or IS non-FC) — either way it can't
            // honor a Hit/Miss plan.
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), false);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let err = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-mismatch"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-4".into(),
                            reason: ColdBaseMissReason::NoCandidate,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect_err("a capability mismatch must hard-error");

            match err {
                SandboxError::CaptureFailed(failure) => {
                    assert_eq!(
                        failure.kind,
                        engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch
                    );
                    assert!(!failure.kind.is_retryable());
                }
                other => {
                    panic!("expected CaptureFailed(ColdBaseCapabilityMismatch), got {other:?}")
                }
            }
            assert_eq!(
                counters.create_calls.load(Ordering::SeqCst),
                0,
                "must fail BEFORE ever booting a VM"
            );
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(counters.destroy_calls.load(Ordering::SeqCst), 0);
        }

        /// A `Hit` combined with an actual capability mismatch is the
        /// SAME hard error as `Miss` — the gate checks the plan variant,
        /// not which specific variant it is.
        #[tokio::test]
        async fn hit_with_capability_mismatch_also_hard_errors() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), false);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let err = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-hit-mismatch"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Hit {
                            content_key: "ck-5".into(),
                            snapshot: Box::new(fake_snapshot()),
                        },
                    },
                    progress_tx,
                )
                .await
                .expect_err("a capability mismatch must hard-error even on a Hit");

            assert!(matches!(
                err,
                SandboxError::CaptureFailed(f)
                    if f.kind == engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch
            ));
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
        }
    }

    /// Export identity gates the framed VM-state and disk-seal artifacts.
    #[tokio::test]
    async fn migration_fetch_checks_export_id_and_frames_artifacts() {
        use engram_core::types::snapshot::{DiskSealInfo, MigrationItem};
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner);
        let export_dir = tmp.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("state.bin"), b"vmstate-bytes").unwrap();
        let sandbox_id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), sandbox_id).await;
        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        let guard_src = Arc::new(tokio::sync::Mutex::new(()));
        assert!(pooled.migrations.insert(crate::migration::MigrationExport {
            export_id: export_id.clone(),
            sandbox_id,
            snapshot_dir: export_dir,
            disk_seal: [(DiskRole::Root, seal_root(&volumes, sandbox_id).await)].into(),
            precopy: Default::default(),
            clock: std::sync::Arc::new(engram_core::traits::SystemClock::new()),
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
            capture_guard: guard_src.clone().try_lock_owned().unwrap(),
        }));

        // Bad export id => NotFound.
        let Err(err) = pooled
            .migration_fetch("0000", vec![MigrationItem::StateBin])
            .await
        else {
            panic!("bad export must be refused");
        };
        assert!(matches!(err, SandboxError::NotFound));

        // Valid pull: state.bin, the disk seal descriptor and a sealed
        // block, framed in order.
        let stream = pooled
            .migration_fetch(
                &export_id,
                vec![
                    MigrationItem::StateBin,
                    MigrationItem::DiskSealInfo(DiskRole::Root),
                    MigrationItem::DiskBlockAt(DiskRole::Root, 0),
                ],
            )
            .await
            .expect("valid fetch");
        let frames: Vec<_> = stream.map(|f| f.expect("frame")).collect().await;
        let item = |idx: u32| -> Vec<u8> {
            frames
                .iter()
                .filter(|f| f.item_idx == idx)
                .flat_map(|f| f.data.to_vec())
                .collect()
        };
        assert_eq!(item(0), b"vmstate-bytes");
        assert_eq!(
            serde_json::from_slice::<DiskSealInfo>(&item(1)).unwrap(),
            DiskSealInfo {
                block_indices: vec![0]
            }
        );
        assert_eq!(item(2), root_block());
        assert!(frames.iter().any(|f| f.item_idx == 2 && f.last));

        // A block the seal does not list is refused.
        let frames: Vec<_> = pooled
            .migration_fetch(
                &export_id,
                vec![MigrationItem::DiskBlockAt(DiskRole::Root, 1)],
            )
            .await
            .expect("fetch opens")
            .collect()
            .await;
        assert!(matches!(&frames[0], Err(SandboxError::InvalidSpec(_))));
    }

    /// The first descriptor permits resume; a later update carries the final
    /// order. The handler loader must wait and preserve that order exactly.
    #[test]
    fn malformed_hint_is_separate_from_required_seal_fields() {
        for hot in [
            serde_json::json!("invalid"),
            serde_json::json!([[1, 2]]),
            serde_json::Value::Null,
        ] {
            let bytes = serde_json::to_vec(
                &serde_json::json!({"sealed_chunk_indices": [], "hot_chunks": hot}),
            )
            .unwrap();
            assert_eq!(parse_hot_hint(&bytes), Some(Vec::new()));
        }
        assert_eq!(parse_hot_hint(br#"{"hot_chunks":[]}"#), Some(vec![]));
        assert!(parse_hot_hint(br#"{}"#).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn hot_hint_polling_has_bounded_requests_and_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let requests = AtomicUsize::new(0);
        let started = tokio::time::Instant::now();
        poll_hot_hint(Duration::from_secs(240), || async {
            requests.fetch_add(1, Ordering::SeqCst);
            false // Publication failed: every request returns NotFound.
        })
        .await;
        assert!(requests.load(Ordering::SeqCst) <= 125);
        assert!(requests.load(Ordering::SeqCst) > 5);
        assert_eq!(started.elapsed(), Duration::from_secs(240));

        // An unresponsive fetch cannot extend the remaining budget either.
        let started = tokio::time::Instant::now();
        poll_hot_hint(Duration::from_millis(37), std::future::pending).await;
        assert_eq!(started.elapsed(), Duration::from_millis(37));
    }

    #[tokio::test(start_paused = true)]
    async fn hot_hint_polls_stop_on_local_drain_without_drain_wait() {
        use engram_migrate_proto::HandlerControl;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        // Socket I/O must not let Tokio auto-advance the paused clock. Only
        // the explicit advances below may expire a polling backoff.
        let _keep_clock_paused = AbortTaskOnDrop(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }));
        for terminal in [
            HandlerControl::DrainDone {
                pulled: 1,
                alt_sourced: 0,
                zero_chunks: 0,
                ms: 1,
                faults: 0,
                fault_us: 0,
                fault_max_us: 0,
                fault_around_chunks_installed: 0,
            },
            HandlerControl::PeerLost {
                remaining: 1,
                detail: "peer closed".into(),
            },
        ] {
            let done = matches!(terminal, HandlerControl::DrainDone { .. });
            let tmp = tempfile::tempdir().unwrap();
            let sock = tmp.path().join("control.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (report_tx, report_rx) = tokio::sync::oneshot::channel();
            let mut source = AbortTaskOnDrop(tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                ready_tx.send(()).unwrap();
                report_rx.await.unwrap();
                let mut bytes = Vec::new();
                engram_migrate_proto::write_frame(&mut bytes, &terminal).unwrap();
                stream.write_all(&bytes).await.unwrap();
            }));
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
            let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
            let poller = AbortTaskOnDrop(tokio::spawn(async move {
                start_rx.await.unwrap();
                poll_hot_hint(Duration::from_secs(240), || async {
                    count.fetch_add(1, Ordering::SeqCst);
                    request_tx.send(()).unwrap();
                    false
                })
                .await;
                // Cancellation drops this sender instead of completing normally.
                finished_tx.send(()).unwrap();
            }));
            let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
            let mut observer = observe_post_copy_drain(sock, Some(poller), |outcome| async move {
                outcome_tx.send(outcome).unwrap();
            });
            ready_rx.await.unwrap();
            start_tx.send(()).unwrap();
            request_rx.recv().await.unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 1);
            let intervals = [100, 200, 400];
            for (index, millis) in intervals.into_iter().enumerate() {
                tokio::time::advance(Duration::from_millis(millis)).await;
                request_rx.recv().await.unwrap();
                assert_eq!(requests.load(Ordering::SeqCst), index + 2);
            }
            // No coordinator calls migration_drain_wait. The local report
            // must cancel polling while the hint remains unavailable.
            report_tx.send(()).unwrap();
            (&mut source.0).await.unwrap();
            (&mut observer.0).await.unwrap();
            assert!(finished_rx.await.is_err(), "poller must be cancelled");
            let outcome = outcome_rx.await.unwrap();
            assert_eq!(
                matches!(
                    outcome,
                    engram_core::types::snapshot::DrainOutcome::Done { .. }
                ),
                done
            );
            let expected = 1 + intervals.len();
            assert_eq!(requests.load(Ordering::SeqCst), expected);
            tokio::time::advance(Duration::from_millis(800)).await;
            assert_eq!(requests.load(Ordering::SeqCst), expected);
        }
    }

    #[tokio::test]
    async fn capture_hot_hint_leaves_initial_artifacts_unchanged() {
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(tmp.path()));
        let pooled = PooledBackend::new(inner);
        let export_dir = tmp.path().join("export");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(export_dir.join("state.bin"), b"state").unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let guard = pooled
            .capture_gate(id)
            .serialize(id, CaptureUser::PeriodicCheckpoint)
            .await;
        assert!(pooled.migrations.insert(crate::migration::MigrationExport {
            export_id: "hot-export".into(),
            sandbox_id: id,
            snapshot_dir: export_dir.clone(),
            disk_seal: [(DiskRole::Root, seal_root(&volumes, id).await)].into(),
            precopy: Default::default(),
            clock: pooled.clock.clone(),
            state_served: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: Arc::new(std::sync::Mutex::new(pooled.clock.now_mono())),
            capture_guard: guard,
        }));
        let trace_path = dest.join(engram_sandbox_firecracker::MIGRATION_HOT_TRACE_FILE);
        let mut loader = Box::pin(engram_uffd_handler::working_set::load_migration_trace(
            &trace_path,
        ));
        assert!(futures::poll!(&mut loader).is_pending());
        let initial = || async {
            pooled
                .migration_fetch(
                    "hot-export",
                    vec![
                        MigrationItem::DiskSealInfo(DiskRole::Root),
                        MigrationItem::StateBin,
                    ],
                )
                .await
                .unwrap()
                .map(|f| f.unwrap().data)
                .collect::<Vec<_>>()
                .await
        };
        let before = initial().await;
        // Even a large, already published hint cannot enlarge the initial fetch.
        let hot = vec![[255; 32]; 4096];
        publish_hot_hint(&export_dir, &hot).unwrap();
        let frames = initial().await;
        assert_eq!(frames, before);
        assert_eq!(frames.len(), 2);
        assert_eq!(
            serde_json::from_slice::<engram_core::types::snapshot::DiskSealInfo>(&frames[0])
                .unwrap()
                .block_indices,
            vec![0]
        );
        assert_eq!(frames[1].as_ref(), b"state");
        // Both missing and published hints must leave the lease untouched.
        let last_activity = pooled
            .migrations
            .find_by_export_id("hot-export")
            .unwrap()
            .last_activity
            .clone();
        let anchor = *last_activity.lock().unwrap();
        std::fs::remove_file(export_dir.join("hot-hint.json")).unwrap();
        for _ in 0..3 {
            let frames: Vec<_> = pooled
                .migration_fetch("hot-export", vec![MigrationItem::HotHint])
                .await
                .unwrap()
                .collect()
                .await;
            assert!(matches!(&frames[0], Err(SandboxError::NotFound)));
            assert_eq!(*last_activity.lock().unwrap(), anchor);
        }
        publish_hot_hint(&export_dir, &[[2; 32], [1; 32]]).unwrap();
        let frames: Vec<_> = pooled
            .migration_fetch("hot-export", vec![MigrationItem::HotHint])
            .await
            .unwrap()
            .collect()
            .await;
        let bytes: Vec<_> = frames
            .into_iter()
            .flat_map(|f| f.unwrap().data.to_vec())
            .collect();
        assert_eq!(*last_activity.lock().unwrap(), anchor);
        let info: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let hot: Vec<[u8; 32]> = serde_json::from_value(info["hot_chunks"].clone()).unwrap();
        stage_hot_trace(&trace_path, &hot).unwrap();
        let trace = loader.await.unwrap();
        assert_eq!(
            trace
                .chunks
                .iter()
                .map(|h| *h.as_bytes())
                .collect::<Vec<_>>(),
            vec![[2; 32], [1; 32]]
        );
    }

    #[tokio::test]
    async fn migration_presetup_without_chain_signals_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(tmp.path()));
        let pooled = PooledBackend::new(inner);
        pooled.set_migrate_peer_server(crate::migrate_peer::PeerServer::new(0));
        let err = pooled
            .migration_presetup(SandboxId::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::InvalidSpec(ref detail) if detail.contains("no checkpoint chain"))
        );
        assert!(pooled.pending_presetups.is_empty());
    }

    /// A live capture pauses the source only once the destination is
    /// ready: the destination's first fetch marks it contacted, and its
    /// first `state.bin` request (after its precopy landed) marks it ready.
    #[tokio::test(start_paused = true)]
    async fn a_presetup_learns_when_the_destination_is_ready_for_the_pause() {
        use engram_core::types::snapshot::MigrationItem;
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner);
        let id = SandboxId::new();
        let dest: Arc<DestProgress> = Arc::default();
        pooled.pending_presetups.insert(
            id,
            PendingPresetup {
                export_id: "E1".into(),
                peer_token: "tok".into(),
                chain_ref: engram_core::types::manifest::ManifestRef::new(),
                disk_manifest_ref: None,
                swap_manifest_ref: None,
                precopy: Default::default(),
                sidecar_json: Vec::new(),
                idle_marked: false,
                hot_chunks: Vec::new(),
                dest: dest.clone(),
            },
        );
        assert!(!dest.contacted.wait(DEST_CONTACT_WAIT).await);
        let _ = pooled
            .migration_fetch("E1", vec![MigrationItem::DiskPrecopyInfo(DiskRole::Root)])
            .await;
        assert!(dest.contacted.wait(DEST_CONTACT_WAIT).await);
        assert!(!dest.ready.wait(std::time::Duration::from_millis(10)).await);
        let waiter = tokio::spawn({
            let dest = dest.clone();
            async move { dest.ready.wait(DEST_READY_WAIT).await }
        });
        tokio::task::yield_now().await;
        let _ = pooled
            .migration_fetch("E1", vec![MigrationItem::StateBin])
            .await;
        assert!(waiter.await.unwrap());
    }

    /// Issue #222: `migration_capture_postcopy` must remove the pending
    /// presetup ONLY when the export id matches. Presetups are
    /// last-write-wins (a coordinator retry re-mints a new export under
    /// the same sandbox id), so a stale straggler `capture(id, E1)`
    /// arriving after presetup re-minted E2 must NOT evict the live E2
    /// entry — it must just error, leaving E2 intact for the legitimate
    /// `capture(id, E2)`.
    ///
    /// Regression for the pre-fix `remove(&id).filter(...)`, which evicted
    /// the entry unconditionally and then discarded it on mismatch,
    /// dooming the legitimate capture to "no matching presetup".
    // Paused: the legitimate capture first waits out the bound for a
    // destination that never asks.
    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn capture_postcopy_export_mismatch_leaves_live_presetup_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner);

        let id = SandboxId::new();
        // E2 is the live presetup (the newer attempt's mint).
        let live_export = "E2".to_string();
        pooled.pending_presetups.insert(
            id,
            PendingPresetup {
                export_id: live_export.clone(),
                peer_token: "tok".into(),
                chain_ref: engram_core::types::manifest::ManifestRef::new(),
                disk_manifest_ref: None,
                swap_manifest_ref: None,
                precopy: Default::default(),
                sidecar_json: Vec::new(),
                idle_marked: false,
                hot_chunks: Vec::new(),
                dest: Arc::default(),
            },
        );

        // A straggler capture from the abandoned attempt (export E1).
        let Err(err) = pooled.migration_capture_postcopy(id, "E1").await else {
            panic!("stale-export straggler must be refused");
        };
        assert!(
            matches!(&err, SandboxError::InvalidSpec(m) if m.contains("no matching presetup")),
            "expected the no-matching-presetup error, got: {err:?}"
        );
        // The crux of #222: E2 must SURVIVE the mismatched straggler.
        assert!(
            pooled.pending_presetups.contains_key(&id),
            "the live presetup was evicted by a mismatched straggler (the #222 bug)"
        );
        assert_eq!(
            pooled.pending_presetups.get(&id).unwrap().export_id,
            live_export,
            "the surviving presetup must still be E2"
        );

        // The legitimate capture for E2 gets past the presetup gate and
        // fails at the next check: the checkpoint chain was never
        // registered for this synthetic sandbox.
        let Err(err) = pooled.migration_capture_postcopy(id, &live_export).await else {
            panic!("E2 capture must proceed past the presetup gate");
        };
        assert!(
            matches!(&err, SandboxError::InvalidSpec(m) if m.contains("checkpoint chain vanished")),
            "expected to pass the presetup gate and fail at the chain check, got: {err:?}"
        );
        // A failed capture puts the presetup back for the coordinator's retry.
        assert_eq!(
            pooled
                .pending_presetups
                .get(&id)
                .map(|p| p.export_id.clone()),
            Some(live_export),
            "a capture that fails before the export exists restores the presetup"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Issue #202: the capture unwind guard. A migration/snapshot
    // capture that errors or is cancelled AFTER pause + fence + drain
    // must NOT leave the guest paused, the fence stuck, or the drained
    // disk chunks lost. The guard's `Drop` runs the same recovery
    // `migration_abort` does. A successful capture defuses it (no-op).
    // ──────────────────────────────────────────────────────────────

    /// Inner backend double that records `resume` calls so the test can
    /// assert the unwind guard un-pauses the guest.
    struct ResumeSpy {
        capture_root: Option<PathBuf>,
        resumes: std::sync::Arc<parking_lot::Mutex<Vec<SandboxId>>>,
    }
    #[async_trait]
    impl SandboxBackend for ResumeSpy {
        fn captures_guest_memory(&self) -> bool {
            false
        }
        fn post_copy_source_view(
            &self,
            _: SandboxId,
        ) -> Option<engram_core::traits::sandbox::PostCopySourceView> {
            Some(engram_core::traits::sandbox::PostCopySourceView {
                fc_pid: std::process::id(),
                memory_backing: self.capture_root.as_ref()?.join("ram"),
            })
        }
        fn working_set_trace_path(&self, _: SandboxId) -> Option<PathBuf> {
            Some(self.capture_root.as_ref()?.join("trace.json"))
        }
        async fn pause(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn snapshot_vmstate_only_package(
            &self,
            _: SandboxId,
            _: &[u8],
        ) -> Result<(engram_core::SnapshotId, PathBuf), SandboxError> {
            let root = self
                .capture_root
                .as_ref()
                .expect("capture fixture")
                .join("export");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("state.bin"), b"state").unwrap();
            Ok((engram_core::SnapshotId::new(), root))
        }
        fn compose_live_sidecar(
            &self,
            _: SandboxId,
            _: Option<engram_core::types::manifest::ManifestRef>,
        ) -> Result<Vec<u8>, SandboxError> {
            Ok(b"{}".to_vec())
        }
        async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
            PathBuf::new()
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: SnapshotMetadata,
            _disks: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.resumes.lock().push(id);
            Ok(())
        }
    }

    /// Session volumes over a file-backed disk, with one provisioned
    /// root volume that holds a non-zero chunk.
    async fn volumes_with_a_root(tmp: &std::path::Path, id: SandboxId) -> Arc<SessionVolumes> {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;
        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.join("blob")));
        let disk: Arc<dyn engram_host_core::SessionDisk> = Arc::new(
            engram_host_core::FileDisk::new(tmp.join("volumes"), MIGRATION_DISK_BLOCK_BYTES)
                .unwrap(),
        );
        let volumes = Arc::new(SessionVolumes::new(
            disk,
            engram_fill::FillEngine::new(ChunkStore::new(blob), engram_fill::Config::default()),
        ));
        volumes
            .provision(
                id,
                Source::Empty {
                    size_bytes: 2 * MIGRATION_DISK_BLOCK_BYTES,
                },
                None,
            )
            .await
            .unwrap();
        let live = volumes.live(id, DiskRole::Root).await.unwrap();
        live.write_chunk(ChunkIndex(0), root_block()).await.unwrap();
        volumes
    }

    /// The bytes `volumes_with_a_root` writes into block 0.
    fn root_block() -> Vec<u8> {
        vec![0x42; MIGRATION_DISK_BLOCK_BYTES as usize]
    }

    /// A seal of the root volume's pending snapshot, listing block 0.
    async fn seal_root(volumes: &SessionVolumes, id: SandboxId) -> Arc<DiskSeal> {
        let pending = volumes.take(id, DiskRole::Root).await.unwrap();
        let sealed = volumes
            .changed_blocks(&pending, MIGRATION_DISK_BLOCK_BYTES)
            .await
            .unwrap();
        assert_eq!(sealed, vec![0]);
        Arc::new(DiskSeal {
            pending,
            sealed: sealed.into_iter().collect(),
        })
    }

    async fn settle(resumes: &parking_lot::Mutex<Vec<SandboxId>>) {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if !resumes.lock().is_empty() {
                break;
            }
        }
    }

    /// A guard dropped armed (an error or a cancelled capture after the
    /// pause) discards the checkpoint it holds and resumes the guest;
    /// the live volume and the last published checkpoint stay.
    #[tokio::test]
    async fn capture_unwind_on_drop_discards_the_pending_checkpoint_and_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let resumes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: resumes.clone(),
        }))
        .with_session_volumes(volumes.clone());
        {
            let mut guard = CaptureUnwind::new(&pooled, id);
            guard.arm();
            guard
                .pending
                .push(volumes.take(id, DiskRole::Root).await.unwrap());
        }
        settle(&resumes).await;
        assert_eq!(*resumes.lock(), vec![id], "unwind resumes the guest once");
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let gone = volumes.pending(id, DiskRole::Root).await.unwrap_err();
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(volumes.roles(id).await.unwrap(), vec![DiskRole::Root]);
        let live = volumes.live(id, DiskRole::Root).await.unwrap();
        assert_eq!(live.read_chunk(ChunkIndex(0)).await.unwrap(), root_block());
    }

    /// A defused guard is inert: no resume, and the checkpoint it handed
    /// over is still on disk for its new owner.
    #[tokio::test]
    async fn capture_unwind_defused_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let resumes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: resumes.clone(),
        }))
        .with_session_volumes(volumes.clone());
        let owned_elsewhere;
        {
            let mut guard = CaptureUnwind::new(&pooled, id);
            guard.arm();
            guard
                .pending
                .push(volumes.take(id, DiskRole::Root).await.unwrap());
            owned_elsewhere = guard.pending.pop();
            guard.defuse();
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(resumes.lock().is_empty(), "a defused guard must not resume");
        assert!(owned_elsewhere.is_some());
        volumes.pending(id, DiskRole::Root).await.unwrap();
    }

    /// A destroy retires the pending presetup of a move that never
    /// reached its capture, precopy snapshot included.
    #[tokio::test]
    async fn destroy_retires_a_pending_presetup_and_its_precopy() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: Default::default(),
        }))
        .with_session_volumes(volumes.clone());
        let precopy = pooled.take_precopy(&volumes, id).await.unwrap();
        let seal = std::sync::Arc::downgrade(&precopy[&DiskRole::Root]);
        pooled.pending_presetups.insert(
            id,
            PendingPresetup {
                export_id: "E1".into(),
                peer_token: "tok".into(),
                chain_ref: engram_core::types::manifest::ManifestRef::new(),
                disk_manifest_ref: None,
                swap_manifest_ref: None,
                precopy,
                sidecar_json: Vec::new(),
                idle_marked: false,
                hot_chunks: Vec::new(),
                dest: Arc::default(),
            },
        );

        pooled.destroy(id).await.unwrap();
        assert!(!pooled.pending_presetups.contains_key(&id));
        assert!(
            seal.upgrade().is_none(),
            "nothing holds the precopy snapshot after destroy"
        );
    }

    /// A destroy consumes an open export first, so its seal does not hold
    /// a snapshot open when the session disks are released.
    #[tokio::test]
    async fn destroy_consumes_an_open_export_and_its_seal() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: Default::default(),
        }))
        .with_session_volumes(volumes.clone());
        let dir = tmp.path().join("export");
        let export_id = open_export(&pooled, id, dir.clone()).await;
        let mut export = pooled.migrations.remove_validated(id, &export_id).unwrap();
        let sealed = seal_root(&volumes, id).await;
        let seal = Arc::downgrade(&sealed);
        export.disk_seal.insert(DiskRole::Root, sealed);
        assert!(pooled.migrations.insert(export));

        pooled.destroy(id).await.unwrap();
        assert!(pooled.migrations.export_id_of(id).is_none());
        assert!(
            seal.upgrade().is_none(),
            "nothing holds the seal after destroy"
        );
        assert!(!dir.exists(), "the export dir goes with the destroy");
        assert!(volumes.roles(id).await.unwrap().is_empty());
    }

    /// Aborting an export discards its seal, removes the export dir and
    /// resumes the guest in place.
    #[tokio::test]
    async fn migration_abort_discards_the_seal_and_resumes() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let volumes = volumes_with_a_root(tmp.path(), id).await;
        let resumes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: resumes.clone(),
        }))
        .with_session_volumes(volumes.clone());
        let dir = tmp.path().join("export");
        let export_id = open_export(&pooled, id, dir.clone()).await;
        let mut export = pooled.migrations.remove_validated(id, &export_id).unwrap();
        export
            .disk_seal
            .insert(DiskRole::Root, seal_root(&volumes, id).await);
        pooled.abort_migration_export(export).await.unwrap();
        settle(&resumes).await;
        assert_eq!(*resumes.lock(), vec![id]);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!dir.exists(), "the export dir goes with the abort");
        let gone = volumes.pending(id, DiskRole::Root).await.unwrap_err();
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(volumes.roles(id).await.unwrap(), vec![DiskRole::Root]);
    }

    /// An abort that requires an unserved state resumes the guest only
    /// while no destination has fetched `state.bin`; after a fetch it
    /// leaves the export open and the guest paused.
    #[tokio::test]
    async fn migration_abort_unserved_refuses_after_the_state_is_fetched() {
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let resumes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: resumes.clone(),
        }));

        let served = SandboxId::new();
        let dir = tmp.path().join("served");
        let export_id = open_export(&pooled, served, dir.clone()).await;
        std::fs::write(dir.join("state.bin"), b"vmstate").unwrap();
        let frames: Vec<_> = pooled
            .migration_fetch(&export_id, vec![MigrationItem::StateBin])
            .await
            .unwrap()
            .collect()
            .await;
        assert!(frames.iter().all(Result::is_ok));
        assert!(!pooled
            .migration_abort_unserved(served, &export_id)
            .await
            .unwrap());
        assert_eq!(pooled.migrations.export_id_of(served), Some(export_id));
        assert!(dir.exists());

        let unserved = SandboxId::new();
        let dir = tmp.path().join("unserved");
        let export_id = open_export(&pooled, unserved, dir.clone()).await;
        assert!(matches!(
            pooled.migration_abort_unserved(unserved, "other").await,
            Err(SandboxError::NotFound)
        ));
        assert!(pooled
            .migration_abort_unserved(unserved, &export_id)
            .await
            .unwrap());
        assert_eq!(*resumes.lock(), vec![unserved]);
        assert!(pooled.migrations.export_id_of(unserved).is_none());
        assert!(!dir.exists());
    }

    /// A fetch of `state.bin` after the TTL sweep chose to abort makes the
    /// abort refuse: the source stays paused and its export stays open.
    #[tokio::test]
    async fn export_ttl_abort_refuses_a_state_fetched_after_the_verdict() {
        use crate::migration::{ttl_verdict, TtlVerdict};
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let resumes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let pooled = PooledBackend::new(Arc::new(ResumeSpy {
            capture_root: None,
            resumes: resumes.clone(),
        }));

        let raced = SandboxId::new();
        let dir = tmp.path().join("raced");
        let export_id = open_export(&pooled, raced, dir.clone()).await;
        std::fs::write(dir.join("state.bin"), b"vmstate").unwrap();
        let verdict = ttl_verdict(true, Some(true), pooled.migration_state_served(raced));
        assert_eq!(verdict, TtlVerdict::AbortInPlace);
        let frames: Vec<_> = pooled
            .migration_fetch(&export_id, vec![MigrationItem::StateBin])
            .await
            .unwrap()
            .collect()
            .await;
        assert!(frames.iter().all(Result::is_ok));
        pooled
            .run_export_ttl_verdict(raced, &export_id, verdict)
            .await
            .unwrap();
        assert!(resumes.lock().is_empty(), "a served source never resumes");
        assert_eq!(pooled.migrations.export_id_of(raced), Some(export_id));

        let idle = SandboxId::new();
        let export_id = open_export(&pooled, idle, tmp.path().join("idle")).await;
        let verdict = ttl_verdict(true, Some(true), pooled.migration_state_served(idle));
        pooled
            .run_export_ttl_verdict(idle, &export_id, verdict)
            .await
            .unwrap();
        assert_eq!(*resumes.lock(), vec![idle]);
        assert!(pooled.migrations.export_id_of(idle).is_none());
    }

    /// ADR 0007 / Phase 5: snapshot wrap chunks an FC-style
    /// `memory.bin` into the chunk store and patches the FC sidecar
    /// `manifest.json` with the resulting `memory_manifest` ref.
    /// Validates that:
    ///  - `SnapshotMetadata.memory_manifest` carries the ref
    ///  - `manifest.json` gained a `memory_manifest` field
    ///  - the manifest in the chunk store is byte-equal to memory.bin
    ///    when materialised back
    ///
    /// Uses a fake inner backend (writes the snapshot artifacts to
    /// `dest` like FC does on a real snapshot, but skips the actual
    /// VM machinery) so the test runs cross-platform.
    #[tokio::test]
    async fn snapshot_chunks_fc_memory_bin_and_patches_manifest() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();
        let _dest = tmp.path().join("snap-1");

        // 1. Synthetic memory.bin: 1 MiB of distinct, non-zero
        //    content so chunk_file produces multiple chunks. Two
        //    512 KiB chunks (default memory chunk size).
        let mut bytes = vec![0u8; 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }

        // 2. Inner backend stub: writes memory.bin + state.bin +
        //    manifest.json to its own per-snapshot dir (ADR 0007
        //    Phase 6 contract), returns a bare SnapshotMetadata
        //    with memory_manifest=None (the FC bare-snapshot shape).
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"state-bin-placeholder")
                    .await
                    .unwrap();
                // Mirror FC's manifest.json shape minimally.
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1",
                        "rootfs_source": null,
                        "image_uri": null,
                        "harness_pack_uri": null,
                        "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64},
                        "disk": {"max_gib": 1},
                        "ttl": null,
                        "env": {},
                        "workdir": null,
                        "harness_substrate": null,
                        "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        // 3. Wire the PooledBackend with a chunk store.
        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload: bytes.clone(),
            staging_root: tmp.path().join("fc-snaps"),
        });
        let pooled = PooledBackend::new(inner).with_chunk_store(cs.clone());

        // 4. Take the snapshot. PooledBackend's wrap chunks
        //    memory.bin and patches manifest.json.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let dest = pooled.snapshot_path_for(metadata.id);
        let manifest_ref = metadata
            .memory_manifest
            .expect("PooledBackend must populate memory_manifest");

        // 5. manifest.json now carries the same ref under
        //    `memory_manifest`.
        let mj: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(dest.join("manifest.json")).await.unwrap())
                .unwrap();
        let patched_ref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mj["memory_manifest"].clone())
                .expect("memory_manifest field must round-trip");
        assert_eq!(patched_ref, manifest_ref);

        // 6. The chunk store materializes the manifest back to the
        //    exact bytes.
        let manifest = cs.get_manifest(manifest_ref).await.unwrap();
        assert!(matches!(manifest.kind, ManifestKind::Memory));
        assert_eq!(manifest.total_bytes, bytes.len() as u64);
        let recovered = tmp.path().join("recovered.bin");
        cs.materialize_to_file_from(&manifest, &recovered, &cs)
            .await
            .unwrap();
        let recovered_bytes = tokio::fs::read(&recovered).await.unwrap();
        assert_eq!(recovered_bytes, bytes, "memory bytes round-trip");
    }

    /// ADR 0014: PooledBackend's snapshot wrap uploads state.bin +
    /// sidecar.json to BlobStorage and stamps the metadata with the
    /// portable blob keys. Mirrors the chunked-memory test pattern
    /// above but verifies the new state/sidecar upload step rather
    /// than the existing memory-chunk step.
    #[tokio::test]
    async fn snapshot_uploads_state_and_sidecar_to_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // FakeFcBackend writes the three artifacts (memory.bin,
        // state.bin, manifest.json) into its staging dir, mirroring
        // what real FC `create_snapshot` produces.
        #[derive(Clone)]
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"fake-state-bin-bytes")
                    .await
                    .unwrap();
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload,
            staging_root: tmp.path().join("fc-snaps"),
        });
        let pooled = PooledBackend::new(inner).with_chunk_store(cs);

        let source_sandbox = SandboxId::new();
        let metadata = pooled.snapshot(source_sandbox).await.unwrap();

        // Portable fields populated.
        assert_eq!(
            metadata.source_sandbox_id,
            Some(source_sandbox),
            "PooledBackend must stamp source_sandbox_id",
        );
        let state_key = metadata
            .state_blob_key
            .as_ref()
            .expect("state_blob_key must be set after upload");
        let sidecar_key = metadata
            .sidecar_blob_key
            .as_ref()
            .expect("sidecar_blob_key must be set after upload");
        assert!(state_key.contains(&metadata.id.to_string()));
        assert!(sidecar_key.contains(&metadata.id.to_string()));

        // Blobs are actually in BlobStorage.
        let downloaded_state: bytes::Bytes = blob.get(state_key).await.unwrap();
        assert_eq!(&downloaded_state[..], b"fake-state-bin-bytes");
        let downloaded_sidecar: bytes::Bytes = blob.get(sidecar_key).await.unwrap();
        let sidecar: serde_json::Value = serde_json::from_slice(&downloaded_sidecar).unwrap();
        assert_eq!(sidecar["format"], "fc");
        assert_eq!(sidecar["spec"]["image"], "test:1");
    }

    /// ADR 0014: PooledBackend's restore wrap pulls state.bin +
    /// sidecar from BlobStorage when the local files are missing
    /// (cross-host restore case). Validated by deleting the local
    /// staging files between snapshot and restore — restore must
    /// rebuild them before delegating to inner.
    #[tokio::test]
    async fn restore_materializes_missing_state_and_sidecar_from_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // Inner backend that lets us drive snapshot then asserts on
        // restore that state.bin + manifest.json are present on disk
        // by the time it's invoked.
        struct AssertingInner {
            staging_root: PathBuf,
            payload: Vec<u8>,
            saw_state: parking_lot::Mutex<Option<Vec<u8>>>,
            saw_sidecar: parking_lot::Mutex<Option<Vec<u8>>>,
        }
        impl AssertingInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for AssertingInner {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"upload-me-state")
                    .await
                    .unwrap();
                let sidecar = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "t", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&sidecar).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "t".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(
                &self,
                _: SandboxId,
                meta: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                // Capture what was on disk at the moment inner.restore ran.
                let dest = self.dir_for(meta.id);
                let state = tokio::fs::read(dest.join("state.bin"))
                    .await
                    .expect("state.bin must be materialised before inner.restore");
                let sidecar = tokio::fs::read(dest.join("manifest.json"))
                    .await
                    .expect("manifest.json must be materialised before inner.restore");
                *self.saw_state.lock() = Some(state);
                *self.saw_sidecar.lock() = Some(sidecar);
                Ok(())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner = Arc::new(AssertingInner {
            staging_root: tmp.path().join("fc-snaps"),
            payload: payload.clone(),
            saw_state: parking_lot::Mutex::new(None),
            saw_sidecar: parking_lot::Mutex::new(None),
        });
        let inner_dyn: Arc<dyn SandboxBackend> = inner.clone();
        let pooled = PooledBackend::new(inner_dyn).with_chunk_store(cs);

        // Snapshot — uploads state + sidecar to BlobStorage.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let staging = inner.dir_for(metadata.id);

        // Simulate cross-host: delete the local staging files so
        // restore has nothing to read from disk. ADR 0039: the capture
        // already removed memory.bin after chunking it — assert that
        // (regression guard for the 61G leak fix), then restore
        // re-materializes it from the chunked manifest.
        assert!(
            !staging.join("memory.bin").exists(),
            "ADR 0039: Full capture must remove the local memory.bin after chunking",
        );
        tokio::fs::remove_file(staging.join("state.bin"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("manifest.json"))
            .await
            .unwrap();

        // Restore — wrap must re-download state.bin + sidecar
        // before delegating to inner.
        pooled
            .restore(SandboxId::new(), metadata, None)
            .await
            .unwrap();

        let seen_state = inner.saw_state.lock().clone().expect("inner.restore ran");
        assert_eq!(&seen_state[..], b"upload-me-state");
        let seen_sidecar = inner.saw_sidecar.lock().clone().expect("inner.restore ran");
        let parsed: serde_json::Value = serde_json::from_slice(&seen_sidecar).unwrap();
        assert_eq!(parsed["format"], "fc");
    }

    /// Snapshot wrap is a no-op when no chunk store is wired —
    /// metadata + manifest.json stay exactly as the inner backend
    /// produced them. Guards against accidental coupling between
    /// `with_chunk_store` and the snapshot path.
    #[tokio::test]
    async fn snapshot_no_chunk_store_passes_through_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir));
        let pooled = PooledBackend::new(inner);

        let id = SandboxId::new();
        pooled.create(id, live_spec("warm-test")).await.unwrap();
        // ProcessBackend's snapshot writes the directory structure
        // without a memory.bin (no guest RAM concept). The wrap
        // skips chunking silently and metadata.memory_manifest
        // stays None.
        let metadata = pooled.snapshot(id).await.expect("ProcessBackend snapshot");
        assert!(
            metadata.memory_manifest.is_none(),
            "no chunk_store wired must leave memory_manifest unset",
        );
    }

    /// ADR 0007 Phase 5: when `restore()` is called on a snapshot
    /// dir whose `memory.bin` is missing locally but whose sidecar
    /// JSON carries a `memory_manifest`, PooledBackend materialises
    /// the file from chunks before delegating to the inner backend.
    /// Validates by:
    ///   1. Build a chunk store + put a small memory manifest
    ///   2. Stage a snapshot dir with state.bin + manifest.json
    ///      (memory_manifest set, but NO memory.bin)
    ///   3. Call PooledBackend::restore
    ///   4. Assert memory.bin now exists at byte-equal content
    #[tokio::test]
    async fn restore_materializes_missing_memory_bin_from_chunks() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_core::types::manifest::ManifestRef;

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // Plant a synthetic memory.bin (1 MiB of non-zero pattern)
        // and chunk it into the store. `chunk_file` skips all-zero
        // blocks; we want the test to exercise non-empty chunks.
        let src_path = tmp.path().join("source-memory.bin");
        let bytes: Vec<u8> = (0..(1024 * 1024)).map(|i| ((i % 251) + 1) as u8).collect();
        tokio::fs::write(&src_path, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&src_path, ManifestKind::Memory, None)
            .await
            .unwrap();
        let manifest_ref = ManifestRef::new();
        cs.put_manifest(manifest_ref, &manifest).await.unwrap();

        // Inner backend: records the metadata it was called with
        // so we can assert PooledBackend's wrap fired the
        // materialize step BEFORE inner.restore. Owns a staging
        // root that snapshot_path_for derives from — the wrap
        // looks the path up from there to stage memory.bin.
        struct CapturingInner {
            captured: parking_lot::Mutex<Option<SnapshotMetadata>>,
            staging_root: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for CapturingInner {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                m: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                *self.captured.lock() = Some(m);
                Ok(())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner = Arc::new(CapturingInner {
            captured: parking_lot::Mutex::new(None),
            staging_root: tmp.path().join("inner-snaps"),
        });

        // Allocate the snapshot_id up front so we can stage its
        // dir at the path snapshot_path_for will return.
        let snapshot_id = engram_core::SnapshotId::new();
        let snap_dir = inner.snapshot_path_for(snapshot_id);
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        tokio::fs::write(snap_dir.join("state.bin"), b"state-placeholder")
            .await
            .unwrap();
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": manifest_ref,
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest_json).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            !snap_dir.join("memory.bin").exists(),
            "precondition: memory.bin must be absent before restore"
        );

        let pooled = PooledBackend::new(inner.clone()).with_chunk_store(cs.clone());

        // Restore — should materialise memory.bin then delegate.
        let metadata = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            swap_manifest: None,
            disk_manifest: None,
            memory_manifest: Some(manifest_ref),
            base_memory_manifest: None,
            base_disk_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
        };
        pooled
            .restore(SandboxId::new(), metadata.clone(), None)
            .await
            .unwrap();

        // Inner backend saw the metadata; memory.bin is now
        // present and byte-equal to the original.
        assert_eq!(
            inner.captured.lock().as_ref().map(|m| m.id),
            Some(snapshot_id)
        );
        let recovered = tokio::fs::read(snap_dir.join("memory.bin"))
            .await
            .expect("memory.bin must exist post-restore");
        assert_eq!(recovered, bytes, "materialized bytes round-trip");
    }

    /// Cross-host trace replay: when a snapshot carries a
    /// `trace_host_hint` on its sidecar JSON, the FC restore path
    /// passes that as `--prefault-trace <host>` to the UFFD
    /// handler so the recorded trace replays on a different host.
    /// Locked at the wire-shape level here — we assert the field
    /// round-trips through serde so a refactor can't accidentally
    /// drop it. (End-to-end replay needs a live VM; that's the
    /// follow-up.)
    #[tokio::test]
    async fn snapshot_skips_materialize_when_memory_bin_already_present() {
        // Negative case: memory.bin already exists locally, so
        // the materialize branch must be a no-op. Guards against a
        // refactor that accidentally truncates the file.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);

        let snap_dir = tmp.path().join("snap");
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        // Pre-existing memory.bin with sentinel bytes.
        let original = b"original-memory-bytes";
        tokio::fs::write(snap_dir.join("memory.bin"), original)
            .await
            .unwrap();
        // Manifest.json claims a different memory_manifest. The
        // materialize branch MUST skip because the local file is
        // present.
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": {"manifest_id": uuid::Uuid::new_v4(), "version": 1},
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await
        .unwrap();

        // Phase 6: the test backend now owns where snap_dir lives.
        // Use a CapturingInner that points snapshot_path_for at the
        // dir we staged, so PooledBackend's wrap finds the
        // already-existing memory.bin and the materialize branch
        // short-circuits.
        struct StagedInner {
            snap_dir: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for StagedInner {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                self.snap_dir.clone()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                // Inner restore is irrelevant for this test — we
                // only assert the materialize branch was a no-op.
                Ok(())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
        }
        let inner: Arc<dyn SandboxBackend> = Arc::new(StagedInner {
            snap_dir: snap_dir.clone(),
        });
        let pooled = PooledBackend::new(inner).with_chunk_store(cs);
        let metadata = SnapshotMetadata {
            id: engram_core::SnapshotId::new(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            swap_manifest: None,
            disk_manifest: None,
            memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
            base_memory_manifest: None,
            base_disk_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
        };
        let _ = pooled.restore(SandboxId::new(), metadata, None).await;
        let after = tokio::fs::read(snap_dir.join("memory.bin")).await.unwrap();
        assert_eq!(after, original, "memory.bin must not be overwritten");
    }

    /// `create` with an `image_uri` provisions a root volume from the
    /// image bundle's disk manifest: the inner backend receives the
    /// volume path (not the OCI-pulled `rootfs.ext4`), the volume holds
    /// the image bytes, and the spec carries a new lineage ref.
    #[tokio::test]
    async fn create_with_image_uri_provisions_a_root_volume_for_the_inner_backend() {
        use crate::image_cache::ImageCache;
        use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
        use engram_core::traits::SandboxBackend;
        use engram_core::types::sandbox::{
            AgentSpec, CpuLimit, DiskLimit, ExecRequest, ExecStream, MemoryLimit,
        };
        use engram_core::types::snapshot::SnapshotMetadata;
        use engram_core::SandboxId;
        use parking_lot::Mutex as PlMutex;
        use std::path::PathBuf;

        // 1. Shared BlobStorage + ChunkStore, mirroring `--mode=all`.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // 2. Plant a fake rootfs in the chunk store + produce a
        //    manifest. Smaller than the materialize round-trip test
        //    because we just need the wiring to flow, not multi-
        //    chunk content addressing — that's covered above.
        let source = tmp.path().join("source.ext4");
        let bytes: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&source, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&source, ManifestKind::Disk, None)
            .await
            .unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        // 3. Stand up an ImageCache rooted under tmp, then plant a
        //    fake "OCI-pulled" artifact + a uri-map entry pointing at
        //    it. ensure_image will short-circuit via the cache-hit
        //    path without attempting a real OCI fetch.
        let cache_root = tmp.path().join("oci-cache");
        let oci = engram_oci::OciClient::new(Arc::new(engram_oci::AnonymousResolver));
        let cache = ImageCache::open(cache_root.clone(), oci).await.unwrap();
        let digest = "sha256:deadbeefcafefeedfacefeed";
        let image_dir = cache.image_dir_for_test(digest);
        tokio::fs::create_dir_all(&image_dir).await.unwrap();
        tokio::fs::write(
            image_dir.join("manifest.toml"),
            b"name = \"chunked-test\"\n",
        )
        .await
        .unwrap();
        // Drop a bytes-equal `rootfs.ext4` alongside so the cache-hit
        // existence check passes; the chunked path supersedes it.
        tokio::fs::write(image_dir.join("rootfs.ext4"), &bytes)
            .await
            .unwrap();
        let bundle_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "disk_manifest": mref,
        }))
        .unwrap();
        tokio::fs::write(image_dir.join("bundle.json"), bundle_bytes)
            .await
            .unwrap();
        cache.prime_image_for_test("test:1", digest);

        // 4. Capturing inner backend — records the spec it receives.
        struct Capturing {
            captured: PlMutex<Option<SandboxSpec>>,
        }
        #[async_trait]
        impl SandboxBackend for Capturing {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _id: SandboxId, spec: SandboxSpec) -> Result<(), SandboxError> {
                *self.captured.lock() = Some(spec);
                Ok(())
            }
            async fn exec_stream(
                &self,
                _id: SandboxId,
                _cmd: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                std::env::temp_dir()
                    .join("engram-test-snaps")
                    .join(id.to_string())
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(
                &self,
                _id: SandboxId,
                _agent: AgentSpec,
            ) -> Result<(), SandboxError> {
                Ok(())
            }
        }
        let captured = Arc::new(Capturing {
            captured: PlMutex::new(None),
        });
        let inner: Arc<dyn SandboxBackend> = captured.clone();

        // 5. PooledBackend wires the image cache, the chunk store and
        //    session volumes over a file-backed disk.
        let volumes_root = tmp.path().join("volumes");
        let disk: Arc<dyn engram_host_core::SessionDisk> = Arc::new(
            engram_host_core::FileDisk::new(
                &volumes_root,
                engram_chunk_store::DEFAULT_DISK_CHUNK_SIZE,
            )
            .unwrap(),
        );
        let pooled = PooledBackend::new(inner)
            .with_image_cache(cache)
            .with_chunk_store(cs.clone())
            .with_session_volumes(Arc::new(SessionVolumes::new(
                disk,
                engram_fill::FillEngine::new(cs.clone(), engram_fill::Config::default()),
            )));

        // 6. Drive a session-create with image_uri set.
        let mut spec = SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: Some("test:1".into()),
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
            swap_mib: None,
            swap_source: None,
            swap_manifest: None,
        };
        spec.image_uri = Some("test:1".into());
        pooled.create(SandboxId::new(), spec).await.unwrap();

        // 7. The inner backend received the root volume, not the
        //    OCI-pulled rootfs.ext4, and the spec carries a fresh
        //    lineage forked from the image manifest.
        let received = captured.captured.lock().clone().expect("inner not called");
        let received_rootfs = received.rootfs_source.expect("rootfs_source not set");
        assert!(
            received_rootfs.starts_with(&volumes_root),
            "rootfs_source {received_rootfs:?} is not a session volume",
        );
        let lineage = received.rootfs_manifest.expect("rootfs_manifest not set");
        assert_ne!(
            lineage, mref,
            "a create forks the image into its own lineage"
        );

        // 8. The volume holds the image bytes.
        let volume = tokio::fs::read(&received_rootfs).await.unwrap();
        assert_eq!(volume, bytes, "root volume bytes mismatch");
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0014 follow-up:
    // PooledBackend MUST forward start_shell / guest_endpoints to its
    // inner backend. The SandboxBackend trait has default impls for
    // these (Ok(7681) / None respectively) that exist for backends
    // without that capability (process, VZ-without-netns). When
    // PooledBackend wraps a FirecrackerBackend that DOES implement
    // them, NOT forwarding silently routes through the trait defaults
    // and the real FC capability never fires — observed in prod: zero
    // start_shell logs on the host-agent despite a completed
    // proxy_shell GRPC call, exactly because PooledBackend.start_shell
    // was using the trait default.
    // ──────────────────────────────────────────────────────────────

    mod inner_forwarding_tests {
        use super::*;
        use parking_lot::Mutex;

        /// SandboxBackend mock that RECORDS every call to the methods
        /// PooledBackend is supposed to forward. We assert against
        /// the captured counters/values after invoking PooledBackend's
        /// surface.
        struct SpyInner {
            start_shell_calls: Mutex<Vec<SandboxId>>,
            start_browser_calls: Mutex<Vec<SandboxId>>,
            stop_browser_calls: Mutex<Vec<SandboxId>>,
            start_ide_calls: Mutex<Vec<SandboxId>>,
            stop_ide_calls: Mutex<Vec<SandboxId>>,
            guest_endpoints_calls: Mutex<Vec<SandboxId>>,
            /// Non-default response values so we can verify the
            /// forward returned the inner's value, not the trait
            /// default.
            shell_port: u16,
            browser_port: u16,
            ide_port: u16,
            guest_endpoints_value: Option<GuestEndpoints>,
        }
        impl SpyInner {
            fn new() -> Self {
                Self {
                    start_shell_calls: Mutex::new(Vec::new()),
                    start_browser_calls: Mutex::new(Vec::new()),
                    stop_browser_calls: Mutex::new(Vec::new()),
                    start_ide_calls: Mutex::new(Vec::new()),
                    stop_ide_calls: Mutex::new(Vec::new()),
                    guest_endpoints_calls: Mutex::new(Vec::new()),
                    // Pick non-default values so a "trait default ran
                    // instead of our override" failure shows up as a
                    // value mismatch, not just a counter mismatch.
                    shell_port: 31337,
                    // Non-default browser port (the trait default is 5900);
                    // a fall-through would return 5900 with a zero counter.
                    browser_port: 45900,
                    // Non-default ide port (the trait default is 13337);
                    // a fall-through would return 13337 with a zero counter.
                    ide_port: 43337,
                    guest_endpoints_value: Some(GuestEndpoints {
                        egress_identity: "10.200.0.42".parse().unwrap(),
                        dial_ip: "10.200.0.2".parse().unwrap(),
                        netns: Some("engr-vm-spytest".into()),
                        vsock_uds: None,
                    }),
                }
            }
        }
        #[async_trait]
        impl SandboxBackend for SpyInner {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                PathBuf::new()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
                self.start_shell_calls.lock().push(id);
                Ok(self.shell_port)
            }
            async fn start_browser(
                &self,
                id: SandboxId,
            ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
                self.start_browser_calls.lock().push(id);
                Ok(engram_core::traits::sandbox::BrowserStart {
                    port: self.browser_port,
                    warning: None,
                })
            }
            async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
                self.stop_browser_calls.lock().push(id);
                Ok(())
            }
            async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
                self.start_ide_calls.lock().push(id);
                Ok(self.ide_port)
            }
            async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
                self.stop_ide_calls.lock().push(id);
                Ok(())
            }
            async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
                self.guest_endpoints_calls.lock().push(id);
                self.guest_endpoints_value.clone()
            }
        }

        /// Regression guard: PooledBackend.start_shell MUST forward
        /// to its inner backend's start_shell. Without forwarding,
        /// the trait default returns Ok(7681) without ever touching
        /// the inner — host-agent's proxy_shell then dials port
        /// 7681 blind even when the FC backend would have correctly
        /// minted the connection (warm restore in a netns, lazy
        /// ttyd spawn, etc.).
        #[tokio::test]
        async fn pooled_backend_forwards_start_shell_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let port = pooled.start_shell(id).await.unwrap();

            // Inner.start_shell received the sandbox id.
            let calls = inner.start_shell_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_shell must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (7681). If we'd accidentally fallen through to
            // the default, the value would be 7681 and the inner's
            // counter would still be 0.
            assert_eq!(
                port, inner.shell_port,
                "must return inner's port (proves the forward, not the default)",
            );
        }

        /// ADR 0065 regression guard: PooledBackend.start_browser MUST
        /// forward to its inner backend. The trait default returns
        /// Ok(5900) WITHOUT touching the inner, so a deleted/regressed
        /// delegate would hand `proxy_vnc` a port (5900) that nothing
        /// started — exactly the start_shell failure class this module
        /// memorializes, applied to the VNC path.
        #[tokio::test]
        async fn pooled_backend_forwards_start_browser_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let start = pooled.start_browser(id).await.unwrap();

            // Inner.start_browser received the sandbox id.
            let calls = inner.start_browser_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_browser must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (5900). A fall-through would return 5900 and leave
            // the inner's counter at 0.
            assert_eq!(
                start.port, inner.browser_port,
                "must return inner's port (proves the forward, not the 5900 default)",
            );
        }

        /// ADR 0065 regression guard: PooledBackend.stop_browser MUST
        /// forward to its inner backend. The trait default is a no-op, so
        /// a deleted delegate would silently never tear down the in-guest
        /// browser stack (leaking Xvfb/chromium/x11vnc) while reporting
        /// success — and the inner's counter would stay at 0.
        #[tokio::test]
        async fn pooled_backend_forwards_stop_browser_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            pooled.stop_browser(id).await.unwrap();

            let calls = inner.stop_browser_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.stop_browser must forward to inner; got {} calls",
                calls.len(),
            );
        }

        /// ADR 0085 regression guard: PooledBackend.start_ide MUST forward
        /// to its inner backend. The trait default returns Ok(13337)
        /// WITHOUT touching the inner, so a deleted/regressed delegate
        /// would hand the orchestrator a port nothing started — the same
        /// failure class as start_shell/start_browser above.
        #[tokio::test]
        async fn pooled_backend_forwards_start_ide_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let port = pooled.start_ide(id).await.unwrap();

            let calls = inner.start_ide_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_ide must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (13337). A fall-through would return 13337 and leave
            // the inner's counter at 0.
            assert_eq!(
                port, inner.ide_port,
                "must return inner's port (proves the forward, not the 13337 default)",
            );
        }

        /// ADR 0085 regression guard: PooledBackend.stop_ide MUST forward
        /// to its inner backend. The trait default is a no-op, so a deleted
        /// delegate would silently never tear down the in-guest code-server
        /// while reporting success — and the inner's counter would stay at 0.
        #[tokio::test]
        async fn pooled_backend_forwards_stop_ide_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            pooled.stop_ide(id).await.unwrap();

            let calls = inner.stop_ide_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.stop_ide must forward to inner; got {} calls",
                calls.len(),
            );
        }

        /// Regression guard, consolidated (issue #541): PooledBackend
        /// MUST forward guest_endpoints to its inner backend. This one
        /// method now carries what used to be three separate forwards
        /// (guest_ip / netns_name_for / vm_internal_ip) — asserting
        /// full value equality against a GuestEndpoints whose fields
        /// are all non-default (egress_identity != dial_ip, netns
        /// Some) means a fall-through to the trait's `None` default,
        /// or a forward that drops a field, both fail on value, not
        /// just call count.
        #[tokio::test]
        async fn pooled_backend_forwards_guest_endpoints_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let endpoints = pooled.guest_endpoints(id).await;

            let calls = inner.guest_endpoints_calls.lock().clone();
            assert_eq!(calls, vec![id], "must forward to inner");
            assert_eq!(
                endpoints,
                inner.guest_endpoints_value.clone(),
                "must return inner's value (proves the forward, not the default-None)",
            );
        }
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0045 C2 teleport: egress policy + DNS interception must be
    // re-established on the destination host before a teleported
    // session resumes (issue #240). The destination host-agent learns
    // the per-session policy via `notify_session_policy`; after that
    // call the local filtering proxy MUST resolve the guest IP to the
    // session's allow-list. If it doesn't, a teleported guest runs
    // unfiltered on the destination — exactly the regression #240
    // closes.
    // ──────────────────────────────────────────────────────────────
    mod teleport_egress_policy_tests {
        use super::*;
        use engram_core::types::egress::SessionEgressPolicy;
        use engram_core::types::image::SecretMode;
        use std::net::Ipv4Addr;

        /// Minimal inner backend — teleport policy re-application only
        /// touches PooledBackend's own `notify_session_policy` override
        /// (it registers against the wired egress proxy); the inner is
        /// never called.
        struct NoopInner;
        #[async_trait]
        impl SandboxBackend for NoopInner {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                PathBuf::new()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
        }

        /// Run a REAL egress daemon as an in-process task (ADR 0121)
        /// and return the facade plus its control handle for
        /// assertions. Same data plane the production host-agent
        /// drives over the same UDS; only the process boundary is
        /// elided (the daemon's own crate covers that).
        async fn spawn_test_egress() -> (
            HostEgress,
            Arc<crate::proxyd_client::ProxydHandle>,
            tempfile::TempDir,
        ) {
            let dir = tempfile::tempdir().expect("tempdir");
            let ca = engram_egress_proxy::Ca::load_or_generate(&dir.path().join("egress-ca"))
                .expect("test CA");
            let mut args = engram_egress_proxyd::DaemonArgs::new(
                dir.path().to_path_buf(),
                ca.cert_pem.clone(),
                ca.key_pair.serialize_pem(),
                "http://127.0.0.1:1".into(), // never dialed in these tests
                None,
                engram_core::HostId::new(),
            );
            // The daemon binds every port for TCP on 0.0.0.0, and the DNS
            // port for UDP too: pick ports free for all of those binds.
            [args.proxy_port, args.dns_port, args.gateway_port] =
                engram_testkit::net::free_tcp_udp_ports();
            let daemon = tokio::spawn(engram_egress_proxyd::run(args));
            let handle = Arc::new(crate::proxyd_client::ProxydHandle::new(
                dir.path().join(engram_egress_proto::CONTROL_SOCK_NAME),
            ));
            // The daemon writes its manifest only after the listeners
            // bind; hello succeeding means the control plane is live.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if engram_egress_proto::manifest::read_manifest(dir.path()).is_some()
                    && handle.hello().await.is_ok()
                {
                    break;
                }
                // `run` returns only when the daemon gives up (a failed
                // bind, for example). Say so at once with its exit code,
                // not as a silent wait to the deadline.
                if daemon.is_finished() {
                    panic!(
                        "in-process egress daemon exited before it was ready: {:?}",
                        daemon.await
                    );
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "in-process egress daemon never came up",
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let egress = HostEgress::new(handle.clone(), ca.cert_pem.clone(), None);
            (egress, handle, dir)
        }

        fn policy_for(
            session_id: SessionId,
            sandbox_id: SandboxId,
            guest_ip: Ipv4Addr,
            allow: &[&str],
        ) -> SessionEgressPolicy {
            SessionEgressPolicy {
                session_id,
                sandbox_id,
                guest_ip,
                network_allow_hosts: allow.iter().map(|s| s.to_string()).collect(),
                network_allow_host_patterns: Vec::new(),
                allow_all: false,
                secrets: Vec::new(),
                injects: Vec::new(),
                observes: Vec::new(),
                guest_services: Vec::new(),
                tunnels: Vec::new(),
                apps: Vec::new(),
                secret_mode: SecretMode::Literal,
            }
        }

        /// The destination host-agent, on receiving the teleported
        /// session's policy, MUST register it with the local proxy so
        /// the guest IP resolves to the allow-list. Pre-#240 prod ran
        /// with the proxy globally disabled, so this path was a no-op
        /// ("no local egress proxy, binding recorded for Phase B
        /// publisher only") and a teleported guest had unfiltered
        /// egress. This test pins the now-mandatory behavior.
        #[tokio::test]
        async fn teleport_dest_reestablishes_egress_filter_before_resume() {
            let (egress, handle, _dir) = spawn_test_egress().await;

            let pooled = PooledBackend::new(Arc::new(NoopInner) as Arc<dyn SandboxBackend>)
                .with_egress(Arc::new(egress));

            let session_id = SessionId::new();
            let sandbox_id = SandboxId::new();
            let guest_ip = Ipv4Addr::new(10, 200, 0, 2);

            // Before the dest applies policy, the proxy knows nothing
            // about this guest IP — a connection would be refused.
            assert!(
                handle
                    .lookup_guest(guest_ip)
                    .await
                    .expect("lookup")
                    .is_none(),
                "pre-condition: dest proxy must not know the teleported guest yet",
            );

            // The dest receives the teleported session's egress policy
            // (the same call the coordinator makes on the destination
            // before finishing the resume).
            pooled
                .notify_session_policy(policy_for(
                    session_id,
                    sandbox_id,
                    guest_ip,
                    &["api.anthropic.com"],
                ))
                .await
                .expect("dest must accept and apply the teleported egress policy");

            // The filter is now LIVE on the destination: the guest IP
            // resolves to a session whose allow-list permits ONLY the
            // policy's host and refuses everything else (incl. a DNS-
            // exfil target). DNS interception keys off the same
            // `network_allow` HostList the resolver consults, so this
            // also proves DNS filtering is in force for the guest.
            let guest = handle
                .lookup_guest(guest_ip)
                .await
                .expect("lookup")
                .expect("dest proxy must resolve the teleported guest IP after policy apply");
            assert_eq!(guest.session_id, session_id);
            assert_eq!(
                handle
                    .decide(guest_ip, "api.anthropic.com")
                    .await
                    .expect("decide")
                    .as_deref(),
                Some("bypass"),
                "allow-listed host must be permitted post-teleport",
            );
            assert_eq!(
                handle
                    .decide(guest_ip, "evil.example.com")
                    .await
                    .expect("decide")
                    .as_deref(),
                Some("reject"),
                "non-allow-listed host (exfil target) must be refused post-teleport",
            );
        }

        /// Idempotent re-application: a teleport may re-issue the
        /// policy (e.g. parachute fallback re-homes the session). The
        /// dest must update cleanly, never leaving a stale unfiltered
        /// binding or two conflicting entries for the same IP.
        #[tokio::test]
        async fn teleport_dest_policy_reapply_is_idempotent() {
            let (egress, handle, _dir) = spawn_test_egress().await;
            let pooled = PooledBackend::new(Arc::new(NoopInner) as Arc<dyn SandboxBackend>)
                .with_egress(Arc::new(egress));

            let session_id = SessionId::new();
            let sandbox_id = SandboxId::new();
            let guest_ip = Ipv4Addr::new(10, 200, 0, 2);

            pooled
                .notify_session_policy(policy_for(session_id, sandbox_id, guest_ip, &["a.example"]))
                .await
                .unwrap();
            // Re-home: same IP, tighter allow-list.
            pooled
                .notify_session_policy(policy_for(session_id, sandbox_id, guest_ip, &["b.example"]))
                .await
                .unwrap();

            assert_eq!(
                handle
                    .decide(guest_ip, "b.example")
                    .await
                    .expect("decide")
                    .as_deref(),
                Some("bypass"),
            );
            assert_eq!(
                handle
                    .decide(guest_ip, "a.example")
                    .await
                    .expect("decide")
                    .as_deref(),
                Some("reject"),
                "re-applied policy must replace the prior allow-list, not union it",
            );
        }

        /// An inner backend whose `list()` reports a fixed survivor
        /// set — the post-reattach live set of a restarted host-agent.
        struct FixedListInner(Vec<SandboxId>);
        #[async_trait]
        impl SandboxBackend for FixedListInner {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                PathBuf::new()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(self.0.clone())
            }
        }

        /// ADR 0111: the registry a host-agent restart wiped is
        /// rebuilt from the policies the previous process persisted
        /// beside its binding records — a local read, no coordinator.
        /// A coordinator-side re-push cannot rebuild it, and a healthy
        /// VM stays without egress.
        #[tokio::test]
        async fn restart_rebuilds_egress_registry_from_persisted_policies() {
            let bindings_dir = tempfile::tempdir().expect("tempdir");
            let session_id = SessionId::new();
            let sandbox_id = SandboxId::new();
            let guest_ip = Ipv4Addr::new(10, 200, 0, 7);

            // Generation A: policy applied and persisted (the
            // start_agent path), then the process "dies" — its
            // registry (egress_a) simply goes away.
            {
                let (egress_a, _handle_a, _daemon_a) = spawn_test_egress().await;
                let pooled_a = PooledBackend::new(Arc::new(NoopInner) as Arc<dyn SandboxBackend>)
                    .with_egress(Arc::new(egress_a));
                let policy = policy_for(session_id, sandbox_id, guest_ip, &["api.anthropic.com"]);
                pooled_a
                    .notify_session_policy(policy.clone())
                    .await
                    .expect("gen A applies");
                let store = crate::bindings::BindingStore::open(bindings_dir.path())
                    .expect("open bindings");
                store.store_policy(&policy).expect("persist policy");
            }

            // A second persisted policy whose sandbox did NOT survive
            // the roll — it must not be registered.
            let dead_policy = policy_for(
                SessionId::new(),
                SandboxId::new(),
                Ipv4Addr::new(10, 200, 0, 9),
                &["github.com"],
            );
            crate::bindings::BindingStore::open(bindings_dir.path())
                .expect("open bindings")
                .store_policy(&dead_policy)
                .expect("persist dead policy");

            // Generation B: a fresh daemon (the daemon ALSO died —
            // the double-failure recovery path), a fresh store over
            // the same directory, reattach reported one survivor.
            let (egress_b, handle_b, _daemon_b) = spawn_test_egress().await;
            let pooled_b = PooledBackend::new(
                Arc::new(FixedListInner(vec![sandbox_id])) as Arc<dyn SandboxBackend>
            )
            .with_egress(Arc::new(egress_b));
            assert!(
                handle_b
                    .lookup_guest(guest_ip)
                    .await
                    .expect("lookup")
                    .is_none(),
                "pre-condition: the restarted registry is empty",
            );
            let policies = crate::bindings::BindingStore::open(bindings_dir.path())
                .expect("open bindings")
                .list_policies()
                .expect("list policies");
            pooled_b.rebuild_egress_from_policies(policies).await;

            let guest = handle_b
                .lookup_guest(guest_ip)
                .await
                .expect("lookup")
                .expect("survivor guest resolves after rebuild");
            assert_eq!(guest.session_id, session_id);
            assert_eq!(
                handle_b
                    .decide(guest_ip, "api.anthropic.com")
                    .await
                    .expect("decide")
                    .as_deref(),
                Some("bypass"),
            );
            assert!(
                handle_b
                    .lookup_guest(dead_policy.guest_ip)
                    .await
                    .expect("lookup")
                    .is_none(),
                "a policy whose sandbox did not survive must not register",
            );
        }
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0014 issue #1/#2: commit + abort snapshot lifecycle tests.
    // ──────────────────────────────────────────────────────────────

    mod snapshot_lifecycle_tests {
        use super::*;
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        /// Stripped-down FC-shaped inner that writes the three
        /// canonical files (memory.bin, state.bin, manifest.json) into
        /// a sandbox-id-keyed (NOT snapshot-id-keyed — we want to see
        /// PooledBackend driving the snapshot_id path) staging root.
        struct LifecycleInner {
            staging_root: PathBuf,
        }
        impl LifecycleInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for LifecycleInner {
            fn captures_guest_memory(&self) -> bool {
                true
            }
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![0u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"fake-state-bin")
                    .await
                    .unwrap();
                // Minimal but parseable sidecar (the post-snapshot
                // path validates JSON; we don't care about content).
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    swap_manifest: None,
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    base_disk_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        fn build_pooled(
            tmp: &tempfile::TempDir,
        ) -> (PooledBackend, Arc<dyn engram_core::traits::BlobStorage>) {
            let blob: Arc<dyn engram_core::traits::BlobStorage> =
                Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
            let cs = ChunkStore::new(blob.clone());
            let inner: Arc<dyn SandboxBackend> = Arc::new(LifecycleInner {
                staging_root: tmp.path().join("fc-snaps"),
            });
            let pooled = PooledBackend::new(inner).with_chunk_store(cs);
            (pooled, blob)
        }

        /// Sanity: snapshot() leaves the local dir on disk and the
        /// per-snapshot opaque blobs in BlobStorage. commit_snapshot
        /// clears the in-flight tracking *without* deleting them — they
        /// belong to the PG row now.
        #[tokio::test]
        async fn commit_snapshot_keeps_artifacts() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let metadata = pooled.snapshot(sandbox_id).await.unwrap();
            let dir = pooled.inner.snapshot_path_for(metadata.id);
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
            let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);

            assert!(dir.exists(), "snapshot dir must exist after snapshot()");
            assert!(blob.exists(&state_key).await.unwrap());
            assert!(blob.exists(&sidecar_key).await.unwrap());

            pooled.commit_snapshot(sandbox_id).await.unwrap();

            // After commit: dir + blobs stay (PG-owned now); tracking
            // cleared.
            assert!(dir.exists(), "commit must NOT delete the snapshot dir");
            assert!(blob.exists(&state_key).await.unwrap());
            assert!(blob.exists(&sidecar_key).await.unwrap());
            assert!(!pooled.inflight_snapshots.contains_key(&sandbox_id));
        }

        /// ADR 0028 addendum: abort_snapshot tears down the LOCAL dir
        /// (host-disk hygiene — the 4 GiB-per-retry leak) but NO LONGER
        /// deletes the per-snapshot blobs. Those are governed by the
        /// snapshot-blob GC sweep now, pinned by the `snapshots` row;
        /// deleting them inline here was the engine of the recurring
        /// "recoverable=true but blobs gone" brick (a concurrent producer
        /// aborting another's already-recorded snapshot).
        #[tokio::test]
        async fn abort_snapshot_removes_dir_keeps_blobs() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let metadata = pooled.snapshot(sandbox_id).await.unwrap();
            let dir = pooled.inner.snapshot_path_for(metadata.id);
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
            let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);

            pooled.abort_snapshot(sandbox_id).await.unwrap();

            assert!(!dir.exists(), "abort must rm -rf the local snapshot dir");
            assert!(
                blob.exists(&state_key).await.unwrap(),
                "abort must NOT delete the state.bin blob — it's GC-governed now",
            );
            assert!(
                blob.exists(&sidecar_key).await.unwrap(),
                "abort must NOT delete the sidecar.json blob — it's GC-governed now",
            );
            assert!(!pooled.inflight_snapshots.contains_key(&sandbox_id));
        }

        /// abort_snapshot must be idempotent — double-call after the
        /// in-flight tracking is gone is a no-op, not an error.
        #[tokio::test]
        async fn abort_snapshot_is_idempotent() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, _) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            // No snapshot taken yet → abort is a clean no-op.
            pooled.abort_snapshot(sandbox_id).await.unwrap();

            // Snapshot, abort, then abort again.
            pooled.snapshot(sandbox_id).await.unwrap();
            pooled.abort_snapshot(sandbox_id).await.unwrap();
            pooled
                .abort_snapshot(sandbox_id)
                .await
                .expect("double-abort must be a no-op");
        }

        /// The retry case: a second snapshot() for the same sandbox
        /// tears down the first attempt's LOCAL dir BEFORE producing the
        /// new one (host-disk hygiene — the 4 GiB-per-retry leak). ADR
        /// 0028 addendum: it no longer deletes the prior's durable blobs
        /// inline — those are reaped by the snapshot-blob GC sweep (the
        /// prior is orphaned, no row, so it's swept after grace).
        /// Deleting them inline was the engine of the recurring brick.
        #[tokio::test]
        async fn retry_snapshot_overwrites_prior_attempt() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let first = pooled.snapshot(sandbox_id).await.unwrap();
            let first_dir = pooled.inner.snapshot_path_for(first.id);
            let first_state_key = engram_chunk_store::snapshot_blob::state_blob_key(first.id);
            assert!(first_dir.exists());
            assert!(blob.exists(&first_state_key).await.unwrap());

            // Simulate coord crash between snapshot and commit by NOT
            // calling either commit or abort — then retry.
            let second = pooled.snapshot(sandbox_id).await.unwrap();
            let second_dir = pooled.inner.snapshot_path_for(second.id);
            let second_state_key = engram_chunk_store::snapshot_blob::state_blob_key(second.id);

            // Second snapshot has a fresh id and exists on disk + blob.
            assert_ne!(first.id, second.id);
            assert!(second_dir.exists());
            assert!(blob.exists(&second_state_key).await.unwrap());

            // First attempt's LOCAL dir is GONE — overwrite-in-place.
            assert!(
                !first_dir.exists(),
                "retry must rm the prior attempt's local snapshot dir",
            );
            // ...but its BLOB stays — durable-blob deletion is the GC
            // sweep's job now (the prior is orphaned with no row, so the
            // sweep reaps it after grace). Deleting it inline here was
            // the brick race.
            assert!(
                blob.exists(&first_state_key).await.unwrap(),
                "retry must NOT delete the prior attempt's blob — GC-governed now",
            );
        }
    }

    /// Issue #200 regression: the snapshot/migration paths must not hold
    /// a `DashMap` guard across an `.await`.
    ///
    /// Why this matters: a `DashMap` `Ref`/`RefMut` guard pins the
    /// shard's **synchronous** `RwLock`. A `.get()` read guard held
    /// across a multi-second `.await` (GCS upload ~32 s, paused-VM
    /// drains, device fsyncs) blocks every contending writer
    /// (`destroy`'s `remove`, `create`/`rehydrate`'s `insert`) hashing
    /// to the same shard: each parks its tokio worker thread for the
    /// whole await, and dashmap-6's writer-preference then blocks new
    /// readers on that shard too. With ≥ worker_threads collisions the
    /// runtime deadlocks and the host is declared dead.
    ///
    /// The bug is fundamentally "is a shard lock held at the await
    /// suspension point?" — a boolean property of the code shape, not a
    /// timing race. We probe it deterministically: `try_get_mut`
    /// returns [`TryResult::Locked`] iff a guard is currently
    /// outstanding on that key's shard (a read guard blocks the write
    /// lock attempt). So at the exact point where the production code
    /// `.await`s — i.e. where a concurrent writer would attempt its
    /// `insert`/`remove` — we assert the shard is FREE under the fixed
    /// clone-then-drop pattern and LOCKED under the old held-guard
    /// pattern. The second assertion proves the probe actually
    /// distinguishes the two shapes (no vacuous pass); the first is the
    /// regression guard.
    #[test]
    fn issue_200_no_dashmap_guard_held_across_await_in_snapshot_path() {
        use dashmap::try_result::TryResult;
        use dashmap::DashMap;
        use std::sync::Arc;

        let map: DashMap<SandboxId, Arc<()>> = DashMap::new();
        let id = SandboxId::new();
        map.insert(id, Arc::new(()));

        // Sanity: with no guard outstanding, the shard write lock is
        // free — `try_get_mut` succeeds.
        assert!(
            matches!(map.try_get_mut(&id), TryResult::Present(_)),
            "precondition: shard must be free with no guard held",
        );

        // FIX shape (the pattern this PR applies at every snapshot/
        // migration site): clone the Arc out of the guard, drop the
        // guard, THEN do the long work. `.map(..)` consumes the `Ref`,
        // so the shard lock is released before `backend` is used.
        let backend: Option<Arc<()>> = map.get(&id).map(|e| e.value().clone());
        // <-- production `.await` happens here, on the owned `backend`.
        // A concurrent `destroy`/`create` writer reaching the shard now
        // must NOT be blocked: the shard is free.
        assert!(
            !matches!(map.try_get_mut(&id), TryResult::Locked),
            "issue #200: a concurrent shard writer MUST NOT be blocked at \
             the await point — clone-then-drop must release the guard first",
        );
        // Use the owned Arc so the clone models the real flush/upload.
        assert!(backend.is_some());

        // BUG shape (what the listed sites did before this PR): hold the
        // `Ref` guard across the long work. This is the deadlock seed —
        // a concurrent writer IS blocked while the guard is live.
        {
            let _guard = map.get(&id).expect("entry present");
            // <-- the buggy code `.await`ed HERE while `_guard` is alive.
            // A concurrent writer's `insert`/`remove` would block on the
            // shard's sync write lock, parking its worker thread.
            assert!(
                matches!(map.try_get_mut(&id), TryResult::Locked),
                "bug model check: a held DashMap guard MUST block a \
                 concurrent shard writer — if this fails, `try_get_mut` is \
                 not detecting the held lock and the regression probe is \
                 invalid",
            );
            // guard drops here, releasing the shard.
        }
        // After the guard drops the shard is free again.
        assert!(matches!(map.try_get_mut(&id), TryResult::Present(_)));
    }

    struct CustodyBackend {
        root: PathBuf,
        id: SandboxId,
        entered: tokio::sync::Notify,
        release: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        calls: PlMutex<Vec<&'static str>>,
        fail_destroy: bool,
        fail_role_write: std::sync::atomic::AtomicBool,
    }

    impl CustodyBackend {
        async fn park(&self, operation: &'static str) {
            self.calls.lock().push(operation);
            self.entered.notify_one();
            let release = self.release.lock().await.take();
            if let Some(release) = release {
                release.await.unwrap();
            }
        }
    }

    #[async_trait]
    impl SandboxBackend for CustodyBackend {
        fn captures_guest_memory(&self) -> bool {
            false
        }
        async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::NotFound)
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            self.park("snapshot").await;
            Ok(fake_metadata())
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.root.join(id.to_string())
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: SnapshotMetadata,
            _disks: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            self.park("restore").await;
            Ok(())
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            self.park("destroy").await;
            if self.fail_destroy {
                Err(SandboxError::Vm("destroy failed".into()))
            } else {
                Ok(())
            }
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(vec![])
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            self.calls.lock().push("start_agent");
            Ok(())
        }
        async fn resume(&self, _: SandboxId) -> Result<(), SandboxError> {
            self.calls.lock().push("resume");
            Ok(())
        }
        async fn set_manifest_migration_role(
            &self,
            _: SandboxId,
            _: Option<&str>,
        ) -> Result<(), SandboxError> {
            self.calls.lock().push("role");
            if self
                .fail_role_write
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                Err(SandboxError::Snapshot("manifest write failed".into()))
            } else {
                Ok(())
            }
        }
    }

    fn custody_backend(
        root: &Path,
        fail_destroy: bool,
    ) -> (Arc<CustodyBackend>, tokio::sync::oneshot::Sender<()>) {
        let (release, rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(CustodyBackend {
                root: root.to_path_buf(),
                id: SandboxId::new(),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Mutex::new(Some(rx)),
                calls: PlMutex::new(Vec::new()),
                fail_destroy,
                fail_role_write: std::sync::atomic::AtomicBool::new(false),
            }),
            release,
        )
    }

    #[tokio::test]
    async fn cancelled_destroy_keeps_cleanup_and_joiners() {
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        let pooled = PooledBackend::new(inner.clone());
        let id = inner.id;
        pooled.bind_session(SessionId::new(), id);
        pooled.last_snapshot_unix_ms.insert(id, 1);
        let _gate = pooled.capture_gate(id);
        let mut first = Box::pin(pooled.destroy(id));
        assert!(futures::poll!(&mut first).is_pending());
        inner.entered.notified().await;
        drop(first);
        assert_eq!(pooled.list().await.unwrap(), vec![id]);
        let mut second = Box::pin(pooled.destroy(id));
        assert!(futures::poll!(&mut second).is_pending());
        release.send(()).unwrap();
        second.await.unwrap();
        assert_eq!(*inner.calls.lock(), vec!["destroy"]);
        assert!(pooled.list().await.unwrap().is_empty());
        assert!(!pooled.session_bindings.contains_key(&id));
        assert!(!pooled.last_snapshot_unix_ms.contains_key(&id));
        assert!(!pooled.capture_gates.contains_key(&id));
    }

    #[tokio::test]
    async fn cancelled_restore_records_post_copy_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        let pooled = Arc::new(PooledBackend::new(inner.clone()));
        let metadata = fake_metadata();
        let dir = inner.snapshot_path_for(metadata.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(engram_sandbox_firecracker::MIGRATION_PEER_FILE),
            b"{}",
        )
        .unwrap();
        let caller = pooled.clone();
        let id = inner.id;
        let restore = tokio::spawn(async move {
            caller
                .restore_with(id, metadata, false, false, vec![])
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), inner.entered.notified())
            .await
            .unwrap();
        restore.abort();
        assert!(restore.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !pooled.migration_roles.contains_key(&inner.id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *pooled.migration_roles.get(&inner.id).unwrap(),
            crate::migration::MigrationRole::PostCopyDest
        );
    }

    /// ADR 0123 D4: `start_agent` waits while the capture holds the guest
    /// paused, and runs while the post phase still holds the capture lock.
    #[tokio::test]
    async fn start_agent_waits_for_the_pause_but_not_the_post_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        let pooled = PooledBackend::new(inner.clone());
        let spawn = pooled
            .capture_gate(inner.id)
            .quiesce(inner.id, CaptureUser::StartAgent)
            .await;
        assert!(
            pooled.capture_in_flight(inner.id),
            "a harness spawn defers the periodic checkpoint"
        );
        drop(spawn);
        let mut capture = Box::pin(pooled.capture_phase(inner.id, CaptureUser::PeriodicCheckpoint));
        assert!(futures::poll!(&mut capture).is_pending());
        let agent = AgentSpec {
            argv: vec![],
            env: Default::default(),
            session_env: Default::default(),
            binding_epoch: 1,
            host_ca_pem: None,
        };
        let mut start = Box::pin(pooled.start_agent(inner.id, agent));
        assert!(futures::poll!(&mut start).is_pending());
        assert_eq!(*inner.calls.lock(), vec!["snapshot"]);
        release.send(()).unwrap();
        let (guard, cap) = capture.await.unwrap();
        start.await.unwrap();
        assert_eq!(*inner.calls.lock(), vec!["snapshot", "start_agent"]);
        assert!(
            pooled.capture_in_flight(inner.id),
            "the post phase still serializes captures"
        );
        drop(cap);
        drop(guard);
        assert!(!pooled.capture_in_flight(inner.id));
    }

    /// An open export for `id` whose local directory exists on disk.
    async fn open_export(pooled: &PooledBackend, id: SandboxId, dir: PathBuf) -> String {
        std::fs::create_dir_all(&dir).unwrap();
        let export_id = format!("export-{id}");
        assert!(pooled.migrations.insert(crate::migration::MigrationExport {
            export_id: export_id.clone(),
            sandbox_id: id,
            snapshot_dir: dir,
            disk_seal: Default::default(),
            precopy: Default::default(),
            clock: pooled.clock.clone(),
            state_served: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: Arc::new(std::sync::Mutex::new(std::time::Duration::ZERO)),
            capture_guard: pooled
                .capture_gate(id)
                .serialize(id, CaptureUser::PeriodicCheckpoint)
                .await,
        }));
        export_id
    }

    #[tokio::test]
    async fn migration_commit_propagates_destroy_failure_and_removes_the_export_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), true);
        release.send(()).unwrap();
        let pooled = PooledBackend::new(inner.clone());
        let id = inner.id;
        let dir = tmp.path().join("export");
        let export_id = open_export(&pooled, id, dir.clone()).await;
        let error = pooled.migration_commit(id, &export_id).await.unwrap_err();
        assert!(error.to_string().contains("destroy failed"));
        // The export is consumed, so nothing else can reach the directory.
        assert!(!dir.exists(), "export dir must go even when destroy fails");
        assert!(pooled.migrations.export_id_of(id).is_none());
    }

    /// The abort's unconditional work (requeue, unfence, resume) runs
    /// before the role clear, so a failed manifest write never strands a
    /// paused guest; the error still reaches the caller.
    #[tokio::test]
    async fn migration_abort_resumes_the_guest_before_reporting_the_role_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        release.send(()).unwrap();
        let pooled = PooledBackend::new(inner.clone());
        let id = inner.id;
        pooled.note_migration_role(id, Some(crate::migration::MigrationRole::PostCopySource));
        let dir = tmp.path().join("export");
        let export_id = open_export(&pooled, id, dir.clone()).await;
        inner
            .fail_role_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = pooled.migration_abort(id, &export_id).await.unwrap_err();
        assert!(error.to_string().contains("manifest write failed"));
        assert_eq!(*inner.calls.lock(), vec!["resume", "role"]);
        assert!(!dir.exists());
        assert!(pooled.migrations.export_id_of(id).is_none());
        assert!(!pooled.capture_gates.contains_key(&id) || !pooled.capture_in_flight(id));
        // The in-memory fence is down: this process no longer treats the
        // guest as a frozen source.
        assert_eq!(pooled.migration_role(id), None);
    }

    /// A finished drain is remembered for later calls, and the
    /// destination role is the post-copy finish's to clear: the drain
    /// wait never writes it, and a capture is refused while it is set.
    #[tokio::test]
    async fn migration_drain_wait_reports_a_finished_drain_and_leaves_the_role() {
        use engram_core::types::snapshot::DrainOutcome;
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        release.send(()).unwrap();
        let pooled = PooledBackend::new(inner.clone());
        let id = inner.id;
        pooled.note_migration_role(id, Some(crate::migration::MigrationRole::PostCopyDest));
        let done = DrainOutcome::Done {
            pulled: 7,
            alt_sourced: 1,
            zero_chunks: 2,
            ms: 30,
        };
        pooled.drained_dests.insert(id, done.clone());
        let again = pooled.migration_drain_wait(id).await.unwrap();
        assert!(matches!(
            again,
            DrainOutcome::Done {
                pulled: 7,
                ms: 30,
                ..
            }
        ));
        assert!(inner.calls.lock().is_empty());
        assert_eq!(
            pooled.migration_role(id),
            Some(crate::migration::MigrationRole::PostCopyDest)
        );
        let refused = pooled
            .capture_phase(id, CaptureUser::PeriodicCheckpoint)
            .await
            .err()
            .expect("a post-copy destination refuses captures");
        assert!(refused.to_string().contains("post-copy destination"));
        // Destroy forgets both the role and the drain memo.
        pooled.destroy(id).await.unwrap();
        assert!(!pooled.drained_dests.contains_key(&id));
        assert!(!pooled.migration_roles.contains_key(&id));
    }

    /// The destination's seed is the source chain manifest with the
    /// peer-supplied chunks written over it: the guest's memory at load.
    /// Linux only: APFS can report a fresh sparse file's hole as data.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn post_copy_seed_publishes_the_peer_chunks_over_the_source_manifest() {
        use engram_chunk_store::manifest::{
            ChunkRef, ChunkSize, ManifestKind, MANIFEST_SCHEMA_VERSION,
        };
        use std::os::unix::fs::FileExt as _;
        const CHUNK: u64 = 4096;
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let mut chunks = Vec::new();
        for i in 0..4u8 {
            let hash = cs.put_chunk(&vec![i + 1; CHUNK as usize]).await.unwrap();
            chunks.push(ChunkRef {
                offset: u64::from(i) * CHUNK,
                hash,
            });
        }
        let source_manifest = engram_chunk_store::Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(CHUNK),
            total_bytes: 4 * CHUNK,
            chunks,
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let source = engram_core::types::manifest::ManifestRef::new();
        cs.put_manifest(source, &source_manifest).await.unwrap();
        // The peer supplied chunk 2 with the source's newer bytes.
        let sealed = tmp.path().join("sealed.bin");
        std::fs::File::create(&sealed)
            .unwrap()
            .write_all_at(&[0x5A; CHUNK as usize], 2 * CHUNK)
            .unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        release.send(()).unwrap();
        let id = inner.id;
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs.clone())
            .with_checkpoint_dir(tmp.path().join("checkpoints"));
        let fork = pooled
            .finisher()
            .seed_chain(
                id,
                ChainSeed::PostCopy {
                    source,
                    sealed: &sealed,
                },
            )
            .await
            .unwrap();
        assert_ne!(fork.manifest_id, source.manifest_id, "a fresh lineage");
        assert_eq!(
            pooled.checkpoint_chains.get(&id).unwrap().manifest_ref,
            fork
        );
        assert!(pooled.has_checkpoint_stamp(id));
        let recovered = tmp.path().join("recovered.bin");
        cs.materialize_to_file_from(&cs.get_manifest(fork).await.unwrap(), &recovered, &cs)
            .await
            .unwrap();
        let memory = std::fs::read(&recovered).unwrap();
        for (i, want) in [1u8, 2, 0x5A, 4].into_iter().enumerate() {
            let chunk = &memory[i * CHUNK as usize..(i + 1) * CHUNK as usize];
            assert!(chunk.iter().all(|b| *b == want), "chunk {i}");
        }
    }

    /// After a done drain the finish records the outcome and clears the
    /// role, retrying a failed manifest write. Without a sealed output it
    /// seeds no chain, so the first capture is Full.
    #[tokio::test(start_paused = true)]
    async fn post_copy_finish_clears_the_role_and_retries_a_failed_write() {
        use engram_core::types::snapshot::DrainOutcome;
        let tmp = tempfile::tempdir().unwrap();
        let (inner, release) = custody_backend(tmp.path(), false);
        release.send(()).unwrap();
        let pooled = PooledBackend::new(inner.clone());
        let id = inner.id;
        pooled.note_migration_role(id, Some(crate::migration::MigrationRole::PostCopyDest));
        inner
            .fail_role_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let finish = crate::postcopy_finish::PostCopyFinish {
            id,
            inner: inner.clone(),
            finisher: pooled.finisher(),
            source_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
            migration_roles: pooled.migration_roles.clone(),
            drained_dests: pooled.drained_dests.clone(),
        };
        let sock = tmp.path().join("uffd-control.sock");
        let done = DrainOutcome::Done {
            pulled: 1,
            alt_sourced: 0,
            zero_chunks: 0,
            ms: 1,
        };
        let task = tokio::spawn(async move { finish.complete(done, &sock).await });
        while !inner.calls.lock().contains(&"role") {
            tokio::task::yield_now().await;
        }
        assert!(pooled.drained_dests.contains_key(&id));
        assert_eq!(
            pooled.migration_role(id),
            Some(crate::migration::MigrationRole::PostCopyDest)
        );
        inner
            .fail_role_write
            .store(false, std::sync::atomic::Ordering::SeqCst);
        task.await.unwrap();
        assert_eq!(pooled.migration_role(id), None);
        assert!(!pooled.checkpoint_chains.contains_key(&id));
    }

    /// A `SnapshotMetadata` carrying a recognizable id for assertions.
    fn fake_metadata() -> SnapshotMetadata {
        SnapshotMetadata {
            id: engram_core::types::SnapshotId::new(),
            size_bytes: 4242,
            created_at: chrono::Utc::now(),
            image_version: "test-image:v1".into(),
            swap_manifest: None,
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            base_disk_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: Vec::new(),
            paused_at: None,
            peer_hints: Vec::new(),
        }
    }

    // ---- Issue #529: host-durable eviction finalize ----
    use crate::checkpoint::CheckpointRecord;
    use parking_lot::Mutex as PlMutex;
    use std::path::Path;

    /// A `BlobStorage` whose FIRST `put_streaming` blocks until
    /// released — used to freeze a spawned eviction finalize job
    /// mid-leg so a test can observe it still pending, deterministically
    /// (not racing the job's own completion).
    struct GateFirstPut {
        inner: engram_storage_local::LocalBlobStorage,
        gate: Arc<tokio::sync::Notify>,
        armed: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl engram_core::traits::BlobStorage for GateFirstPut {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            if !self.armed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.gate.notified().await;
            }
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(
            &self,
            prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    /// A minimal FC-shaped capture backend for the eviction-finalize
    /// tests: `supports_diff_checkpoints() -> true` (the gate
    /// `snapshot_begin` checks before ever routing here) and a
    /// `snapshot` that writes the same three artifacts real FC leaves in
    /// its staging dir (memory.bin, state.bin, manifest.json) — the
    /// `SnapshotFinisher`/`eviction_finalize` legs' contract.
    #[derive(Clone)]
    struct FakeCaptureBackend {
        payload: Vec<u8>,
        staging_root: PathBuf,
        destroy_calls: Arc<PlMutex<Vec<SandboxId>>>,
    }
    impl FakeCaptureBackend {
        fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.staging_root.join(id.to_string())
        }
        async fn write_capture(&self, dest: &Path) {
            tokio::fs::create_dir_all(dest).await.unwrap();
            tokio::fs::write(dest.join("memory.bin"), &self.payload)
                .await
                .unwrap();
            tokio::fs::write(dest.join("state.bin"), b"state-bin-placeholder")
                .await
                .unwrap();
            let manifest = serde_json::json!({
                "sandbox_id": uuid::Uuid::new_v4(),
                "created_at": chrono::Utc::now(),
                "spec": {
                    "image": "test:1", "rootfs_source": null, "image_uri": null,
                    "harness_pack_uri": null, "cpu": {"vcpus": 1}, "memory": {"max_mib": 64},
                    "disk": {"max_gib": 1}, "ttl": null, "env": {}, "workdir": null,
                    "harness_substrate": null, "network": {}
                },
                "format": "fc"
            });
            tokio::fs::write(
                dest.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .await
            .unwrap();
        }
    }
    #[async_trait]
    impl SandboxBackend for FakeCaptureBackend {
        fn captures_guest_memory(&self) -> bool {
            true
        }
        async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn supports_diff_checkpoints(&self) -> bool {
            true
        }
        async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            let snapshot_id = engram_core::SnapshotId::new();
            let dest = self.dir_for(snapshot_id);
            self.write_capture(&dest).await;
            Ok(SnapshotMetadata {
                id: snapshot_id,
                size_bytes: self.payload.len() as u64,
                created_at: chrono::Utc::now(),
                image_version: "test:1".into(),
                swap_manifest: None,
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                base_disk_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
                paused_at: None,
                peer_hints: Vec::new(),
            })
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.dir_for(id)
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: SnapshotMetadata,
            _disks: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroy_calls.lock().push(id);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    fn finalize_payload() -> Vec<u8> {
        let mut bytes = vec![0u8; 64 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }
        bytes
    }

    /// `PooledBackend` wired with a chunk store + checkpoint_dir + the
    /// `FakeCaptureBackend`, ready to exercise `snapshot_begin`. The
    /// `gate`'d blob storage lets a test freeze the spawned finalize job
    /// mid-upload; `None` runs it to completion unobstructed.
    async fn finalize_test_backend(
        gate: Option<Arc<tokio::sync::Notify>>,
    ) -> (
        Arc<PooledBackend>,
        ChunkStore,
        PathBuf,
        Arc<PlMutex<Vec<SandboxId>>>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let local = engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob"));
        let blob: Arc<dyn engram_core::traits::BlobStorage> = match gate {
            Some(gate) => Arc::new(GateFirstPut {
                inner: local,
                gate,
                armed: std::sync::atomic::AtomicBool::new(false),
            }),
            None => Arc::new(local),
        };
        let cs = ChunkStore::new(blob);
        let destroy_calls = Arc::new(PlMutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: destroy_calls.clone(),
        });
        let checkpoint_dir = tmp.path().join("checkpoints");
        let p = PooledBackend::new(inner)
            .with_chunk_store(cs.clone())
            .with_checkpoint_dir(checkpoint_dir.clone());
        let arc = Arc::new(p);
        arc.set_self_ref(&arc);
        std::mem::forget(tmp); // keep the staging dirs alive for the test
        (arc, cs, checkpoint_dir, destroy_calls)
    }

    /// Acceptance criterion #7: an eviction-scanner retry storm against
    /// a mid-finalize sandbox must NOT re-capture — `snapshot_begin`
    /// re-observes the pending `snapshot_id`. Gated so the first call's
    /// background job can't race to completion before the second call.
    #[tokio::test]
    async fn snapshot_begin_is_idempotent_under_a_pending_finalize() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (pooled, _cs, _ckpt_dir, _destroy_calls) =
            finalize_test_backend(Some(gate.clone())).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let first = pooled
            .snapshot_begin(sandbox_id)
            .await
            .expect("first begin");
        let second = pooled
            .snapshot_begin(sandbox_id)
            .await
            .expect("second begin (retry storm)");
        assert_eq!(
            first, second,
            "a pending finalize must be re-observed, not re-captured"
        );

        // Let the frozen job run to completion so it doesn't outlive the
        // test's temp dirs.
        gate.notify_one();
    }

    /// `snapshot_begin` persists the `EvictionFinalizeRecord` (durably,
    /// to `<checkpoint_dir>/finalize/`) BEFORE it returns — the core
    /// durability-boundary-moves-earlier claim. Gated so the assertion
    /// runs while the job is still frozen mid-leg, not racing its own
    /// (fast, local-disk) completion.
    #[tokio::test]
    async fn snapshot_begin_persists_the_finalize_record_before_returning() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (pooled, _cs, ckpt_dir, _destroy_calls) =
            finalize_test_backend(Some(gate.clone())).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");
        assert!(
            pooled.pending_finalizes.contains_key(&sandbox_id),
            "pending_finalizes must be set before snapshot_begin returns",
        );
        let record_path = ckpt_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        let bytes = tokio::fs::read(&record_path)
            .await
            .expect("finalize record must be on disk before snapshot_begin returns");
        // R5: records are sealed in a content-hash envelope keyed on their id.
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string())
            .expect("finalize record envelope must open");
        let record: crate::eviction_finalize::EvictionFinalizeRecord =
            serde_json::from_slice(&body).expect("finalize record must parse");
        assert_eq!(record.sandbox_id, sandbox_id);
        assert_eq!(record.session_id, session_id);
        // The gate freezes the job inside the memory leg (the first
        // blob PUT), so by construction the disk leg — synchronous,
        // no blob I/O — is the furthest it can have progressed; not
        // asserting an exact stage here avoids racing the job's own
        // (legitimately fast, local-disk) advancement past `Captured`.
        assert!(
            matches!(
                record.stage,
                crate::eviction_finalize::FinalizeStage::Captured
                    | crate::eviction_finalize::FinalizeStage::DiskUploaded
            ),
            "unexpected stage {:?} while the job is gated in the memory leg",
            record.stage,
        );

        gate.notify_one();
    }

    /// The full happy path: `snapshot_begin` on an FC-shaped capture
    /// eventually (without the coordinator ever touching it again)
    /// produces a durable `CheckpointRecord { kind: EvictionFinal }`,
    /// deletes its `EvictionFinalizeRecord`, clears `pending_finalizes`,
    /// and best-effort destroys the sandbox.
    #[tokio::test]
    async fn snapshot_begin_completes_and_produces_an_eviction_final_checkpoint() {
        let (pooled, cs, ckpt_dir, destroy_calls) = finalize_test_backend(None).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);
        let checkpoint_recorded = pooled.checkpoint_recorded();

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");

        let records_dir = ckpt_dir.join("records");
        let record_path = records_dir.join(format!("{snapshot_id}.json"));
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            checkpoint_recorded.notified(),
        )
        .await
        .expect("the eviction-final record signals the heartbeat");
        assert!(record_path.exists(), "the signal follows the record write");

        let bytes = tokio::fs::read(&record_path).await.unwrap();
        // R5: records are sealed in a content-hash envelope keyed on their id.
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string()).unwrap();
        let record: CheckpointRecord = serde_json::from_slice(&body).unwrap();
        assert_eq!(record.snapshot_id, snapshot_id);
        assert_eq!(record.sandbox_id, sandbox_id);
        assert_eq!(record.session_id, session_id);
        assert_eq!(
            record.kind,
            engram_protocol::heartbeat::CheckpointKind::EvictionFinal
        );
        let memory_ref = record
            .memory_manifest
            .expect("a captured memory.bin must chunk to a manifest");
        let manifest = cs.get_manifest(memory_ref).await.unwrap();
        assert_eq!(manifest.total_bytes, finalize_payload().len() as u64);

        // The checkpoint record becomes VISIBLE at rename time, but the
        // job deletes the finalize record only after `persist` returns —
        // which now includes a parent-dir fsync (an F_FULLFSYNC on macOS)
        // AFTER the rename. Waiting only for the checkpoint file above
        // therefore races the deletion/clear steps by that fsync latency;
        // wait for them like the destroy below, rather than asserting a
        // point-in-time snapshot of a still-running job.
        let finalize_record_path = ckpt_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        wait_for("in-flight finalize record deleted on completion", || {
            !finalize_record_path.exists()
        })
        .await;
        wait_for("pending_finalizes cleared on completion", || {
            !pooled.pending_finalizes.contains_key(&sandbox_id)
        })
        .await;
        wait_for("best-effort destroy", || {
            destroy_calls.lock().contains(&sandbox_id)
        })
        .await;
    }

    /// Issue #529 crash recovery: a record persisted by a PRIOR host-agent
    /// process (the sandbox is gone — never `create()`d in this test)
    /// is picked up by `resume_pending_finalizes` and driven to the same
    /// durable `CheckpointRecord { kind: EvictionFinal }` outcome.
    #[tokio::test]
    async fn resume_pending_finalizes_redrives_a_record_with_the_sandbox_absent() {
        let (pooled, cs, ckpt_dir, _destroy_calls) = finalize_test_backend(None).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let snapshot_id = engram_core::SnapshotId::new();

        // Hand-craft the on-disk state a crashed `snapshot_begin` would
        // have left: the FC staging dir (memory.bin/state.bin/manifest.json)
        // plus the durable EvictionFinalizeRecord pointing at it —
        // entirely independent of any live sandbox or RAM state.
        let dest = tmp_dest_for(&ckpt_dir, snapshot_id);
        let backend = FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: dest.parent().unwrap().to_path_buf(),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        };
        backend.write_capture(&dest).await;

        let record = crate::eviction_finalize::EvictionFinalizeRecord {
            snapshot_id,
            session_id,
            sandbox_id,
            image_version: "test:1".into(),
            size_bytes: finalize_payload().len() as u64,
            paused_at: chrono::Utc::now(),
            captured_at: chrono::Utc::now(),
            dest,
            chain_prev_ref: None,
            disk_pending: Default::default(),
            aux_bundles: vec![],
            stage: crate::eviction_finalize::FinalizeStage::Captured,
            attempts: 0,
            swap_manifest: None,
            disk_manifest: None,
            memory_manifest: None,
            diff_extents: None,
        };
        record
            .persist(&engram_host_core::TokioFs, &ckpt_dir.join("finalize"))
            .await
            .expect("persist finalize record");

        pooled.resume_pending_finalizes().await;

        let record_path = ckpt_dir.join("records").join(format!("{snapshot_id}.json"));
        wait_for("redriven eviction-final checkpoint record", || {
            record_path.exists()
        })
        .await;
        let bytes = tokio::fs::read(&record_path).await.unwrap();
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string()).unwrap();
        let checkpoint: CheckpointRecord = serde_json::from_slice(&body).unwrap();
        assert_eq!(checkpoint.sandbox_id, sandbox_id);
        assert_eq!(
            checkpoint.kind,
            engram_protocol::heartbeat::CheckpointKind::EvictionFinal
        );
        let memory_ref = checkpoint.memory_manifest.expect("memory manifest set");
        let manifest = cs.get_manifest(memory_ref).await.unwrap();
        assert_eq!(manifest.total_bytes, finalize_payload().len() as u64);
    }

    fn tmp_dest_for(checkpoint_dir: &Path, id: engram_core::SnapshotId) -> PathBuf {
        checkpoint_dir
            .parent()
            .unwrap()
            .join("fc-snaps-redrive")
            .join(id.to_string())
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A `BlobStorage` whose PUTs always fail — drives the finalize
    /// quarantine path.
    struct AlwaysFailPut;
    #[async_trait]
    impl engram_core::traits::BlobStorage for AlwaysFailPut {
        async fn put_streaming(
            &self,
            _key: &str,
            _body: engram_core::traits::ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected put failure (test)".into(),
            ))
        }
        async fn get_streaming(
            &self,
            _key: &str,
        ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected get failure (test)".into(),
            ))
        }
        async fn head(
            &self,
            _key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected head failure (test)".into(),
            ))
        }
        async fn delete(&self, _key: &str) -> Result<(), engram_core::error::BlobError> {
            Ok(())
        }
        async fn list_prefix(
            &self,
            _prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            Ok(Vec::new())
        }
    }

    /// Acceptance criterion #5: a finalize that fails terminally is
    /// quarantined with a metric + WARN/ERROR — never silent — after
    /// `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS` attempts. Set to 1 so the
    /// test quarantines on the very first failure (no backoff sleep).
    #[tokio::test]
    async fn finalize_quarantines_after_max_attempts() {
        // SAFETY (test-only): cargo-nextest runs each test in its own
        // process, so this process-global env var doesn't leak across
        // tests.
        std::env::set_var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS", "1");

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(AlwaysFailPut);
        let cs = ChunkStore::new(blob);
        let destroy_calls = Arc::new(PlMutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: destroy_calls.clone(),
        });
        let checkpoint_dir = tmp.path().join("checkpoints");
        let p = PooledBackend::new(inner)
            .with_chunk_store(cs)
            .with_checkpoint_dir(checkpoint_dir.clone());
        let pooled = Arc::new(p);
        pooled.set_self_ref(&pooled);

        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");

        let quarantined_path = checkpoint_dir
            .join("finalize")
            .join("failed")
            .join(format!("{snapshot_id}.json"));
        wait_for("quarantined finalize record", || quarantined_path.exists()).await;

        let in_flight_path = checkpoint_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        // `quarantine()` persists the failed/ copy FIRST, then deletes
        // the in-flight record — observing the former does not imply
        // the latter yet (flaked under full-suite load).
        wait_for("in-flight record cleared after quarantine", || {
            !in_flight_path.exists()
        })
        .await;
        assert!(
            !pooled.pending_finalizes.contains_key(&sandbox_id),
            "pending_finalizes must be cleared on quarantine"
        );
        // No CheckpointRecord — never falsely claim durability on the
        // path that gave up.
        let checkpoint_path = checkpoint_dir
            .join("records")
            .join(format!("{snapshot_id}.json"));
        assert!(
            !checkpoint_path.exists(),
            "a quarantined finalize must never produce a checkpoint record"
        );

        std::env::remove_var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS");
        std::mem::forget(tmp);
    }

    /// Finding 6: `snapshot_begin`'s own `supports_diff_checkpoints()` gate
    /// — the only thing keeping a VZ/Process host on the composed
    /// `snapshot()` pipeline instead of the host-durable eviction finalize
    /// path — must surface `InvalidSpec` (the coordinator's `idle_evictor`
    /// falls through to `snapshot()` on exactly that variant). `ProcessBackend`
    /// doesn't override the trait default (`false`), the same shape VZ is
    /// (module doc: "Gate: VZ/Process ... surface `InvalidSpec`").
    #[tokio::test]
    async fn snapshot_begin_returns_invalidspec_over_a_non_diff_checkpoint_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let inner = Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        assert!(
            !inner.supports_diff_checkpoints(),
            "test premise: ProcessBackend must not opt into diff checkpoints"
        );
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs)
            .with_checkpoint_dir(tmp.path().join("checkpoints"));

        let err = pooled
            .snapshot_begin(SandboxId::new())
            .await
            .expect_err("a non-diff-checkpoint backend must not enter the host-durable path");
        assert!(
            matches!(err, SandboxError::InvalidSpec(_)),
            "expected InvalidSpec (the idle_evictor's composed-path fallback signal), got {err:?}"
        );
    }

    /// Finding 6, second half: the same gate's `checkpoint_dir` half. A
    /// diff-checkpoint-capable backend (FC-shaped) with no `checkpoint_dir`
    /// wired (checkpointing disabled on this host) must also surface
    /// `InvalidSpec`, not attempt to persist a finalize record with nowhere
    /// durable to put it.
    #[tokio::test]
    async fn snapshot_begin_returns_invalidspec_without_a_wired_checkpoint_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        });
        // Deliberately no `.with_checkpoint_dir(..)`.
        let pooled = PooledBackend::new(inner).with_chunk_store(cs);

        let err = pooled
            .snapshot_begin(SandboxId::new())
            .await
            .expect_err("no wired checkpoint_dir must not enter the host-durable path");
        assert!(
            matches!(err, SandboxError::InvalidSpec(_)),
            "expected InvalidSpec (the idle_evictor's composed-path fallback signal), got {err:?}"
        );
    }

    /// [`FakeCaptureBackend`] whose captures lose `lost` from the staging
    /// dir before they return.
    struct LosesFiles {
        inner: FakeCaptureBackend,
        lost: &'static [&'static str],
    }
    #[async_trait]
    impl SandboxBackend for LosesFiles {
        fn captures_guest_memory(&self) -> bool {
            true
        }
        async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: SnapshotMetadata,
            _: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            let metadata = self.inner.snapshot(id).await?;
            for name in self.lost {
                std::fs::remove_file(self.inner.dir_for(metadata.id).join(name)).unwrap();
            }
            Ok(metadata)
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.inner.snapshot_path_for(id)
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            self.inner.list().await
        }
    }

    /// A capture from a backend whose captures hold guest memory fails
    /// when it left no memory, whichever restore file is gone: the
    /// snapshot must not publish the disks alone as a resumable capture.
    #[tokio::test]
    async fn a_memory_capture_that_lost_its_files_fails() {
        for lost in [&["memory.bin"][..], &["state.bin", "memory.bin"][..]] {
            let tmp = tempfile::tempdir().unwrap();
            let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
            );
            let inner: Arc<dyn SandboxBackend> = Arc::new(LosesFiles {
                inner: FakeCaptureBackend {
                    payload: finalize_payload(),
                    staging_root: tmp.path().join("fc-snaps"),
                    destroy_calls: Arc::new(PlMutex::new(Vec::new())),
                },
                lost,
            });
            let pooled = PooledBackend::new(inner).with_chunk_store(ChunkStore::new(blob));
            let err = pooled
                .snapshot(SandboxId::new())
                .await
                .expect_err("a memory capture without its memory must fail");
            assert!(
                matches!(err, SandboxError::Snapshot(_)),
                "lost {lost:?}: {err:?}"
            );
        }
    }

    // ─── durable chain-head record (survivor rehydrate after a pod roll) ───

    fn chain_head_file(checkpoint_dir: &Path, id: SandboxId) -> PathBuf {
        crate::checkpoint::ChainHeadRecord::subdir(checkpoint_dir).join(format!("{id}.json"))
    }

    /// The write-ahead protocol end to end on the composed snapshot
    /// path: (1) a successful Full capture commits a chain-head record
    /// matching the published memory manifest; (2) the next capture
    /// takes the diff path (chain seeded) and — because the write-ahead
    /// invalidate runs BEFORE the FC create — a failed diff create
    /// leaves NO record (the torn-capture property from the 2026-07-13
    /// incident) and drops the chain; (3) the capture after that is a
    /// Full again and re-commits the record.
    #[tokio::test]
    async fn chain_head_record_follows_the_write_ahead_protocol() {
        let (pooled, _cs, ckpt_dir, _destroy) = finalize_test_backend(None).await;
        let id = SandboxId::new();

        // (1) Full capture → chain seeded → record committed.
        let meta = pooled.snapshot(id).await.expect("full capture");
        let mem_ref = meta.memory_manifest.expect("memory manifest chunked");
        let record = crate::checkpoint::ChainHeadRecord::load(
            &crate::checkpoint::ChainHeadRecord::subdir(&ckpt_dir),
            id,
        )
        .await
        .expect("chain-head record committed after the Full");
        assert_eq!(record.manifest_ref, mem_ref);
        assert_eq!(pooled.chain_head_for_test(id), Some(mem_ref));

        // (2) The chain routes the next capture to snapshot_diff, which
        // the fake backend doesn't support — a create failure AFTER the
        // write-ahead invalidate. The record must be gone and the chain
        // poisoned; nothing may have resurrected the record.
        pooled
            .snapshot(id)
            .await
            .expect_err("diff create must fail on the fake backend");
        assert!(
            !chain_head_file(&ckpt_dir, id).exists(),
            "failed diff must leave no chain-head record (write-ahead invalidate)"
        );
        assert_eq!(
            pooled.chain_head_for_test(id),
            None,
            "failed diff create must poison the in-RAM chain"
        );

        // (3) Chain-less again → Full succeeds → record re-committed.
        let meta = pooled.snapshot(id).await.expect("post-poison full capture");
        let mem_ref = meta.memory_manifest.expect("memory manifest chunked");
        let record = crate::checkpoint::ChainHeadRecord::load(
            &crate::checkpoint::ChainHeadRecord::subdir(&ckpt_dir),
            id,
        )
        .await
        .expect("record re-committed by the recovery Full");
        assert_eq!(record.manifest_ref, mem_ref);

        // destroy removes the record with the chain.
        use engram_core::traits::SandboxBackend as _;
        pooled.destroy(id).await.expect("destroy");
        assert!(!chain_head_file(&ckpt_dir, id).exists());
    }

    /// `FakeCaptureBackend` with a configurable `list()` — the survivor
    /// set `rehydrate_chain_heads` seeds from.
    #[derive(Clone)]
    struct SurvivorListBackend {
        inner: FakeCaptureBackend,
        survivors: Vec<SandboxId>,
    }
    #[async_trait]
    impl SandboxBackend for SurvivorListBackend {
        fn captures_guest_memory(&self) -> bool {
            self.inner.captures_guest_memory()
        }
        async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn supports_diff_checkpoints(&self) -> bool {
            true
        }
        async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            self.inner.snapshot(id).await
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.inner.snapshot_path_for(id)
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: SnapshotMetadata,
            _disks: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(self.survivors.clone())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// A chain seed (restore, create) stamps the checkpoint clock, so the
    /// periodic driver does not capture right after the bind; a capture's
    /// own stamp is never overwritten.
    #[tokio::test]
    async fn chain_seed_stamps_the_checkpoint_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let pooled = PooledBackend::new(Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        }) as Arc<dyn SandboxBackend>)
        .with_chunk_store(ChunkStore::new(blob))
        .with_checkpoint_dir(tmp.path().join("checkpoints"));
        let source = SandboxId::new();
        let memory_ref = pooled
            .snapshot(source)
            .await
            .expect("capture")
            .memory_manifest
            .expect("chunked");

        let restored = SandboxId::new();
        pooled.bind_session(SessionId::new(), restored);
        pooled
            .finisher()
            .seed_chain(restored, ChainSeed::Manifest(memory_ref))
            .await
            .expect("seed");
        let cfg = crate::checkpoint::CheckpointConfig {
            interval: Some(std::time::Duration::from_secs(600)),
            min_interval: std::time::Duration::from_secs(30),
            target_epoch_bytes: 256 * 1024 * 1024,
            first_interval: std::time::Duration::from_secs(120),
        };
        assert!(pooled.last_snapshot_unix_ms.contains_key(&restored));
        assert!(
            pooled.checkpoint_candidates_adaptive(&cfg).is_empty(),
            "a freshly seeded sandbox is not due"
        );

        let captured = SandboxId::new();
        pooled.last_snapshot_unix_ms.insert(captured, 7);
        pooled
            .finisher()
            .seed_chain(captured, ChainSeed::Fork(memory_ref))
            .await
            .expect("seed");
        assert_eq!(
            pooled.last_snapshot_unix_ms.get(&captured).map(|v| *v),
            Some(7)
        );
    }

    /// The pod-roll simulation at the unit level: generation A captures
    /// (committing the chain-head record) and dies; generation B — a
    /// fresh `PooledBackend` on the same checkpoint_dir + chunk store,
    /// whose backend reattached the survivor — rehydrates the chain from
    /// the record. Records for sandboxes that did NOT survive are GC'd,
    /// and a torn record seeds nothing.
    #[tokio::test]
    async fn rehydrate_chain_heads_seeds_survivors_and_gcs_strays() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let checkpoint_dir = tmp.path().join("checkpoints");
        let fake = FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        };

        let survivor = SandboxId::new();
        let dead = SandboxId::new();
        let torn = SandboxId::new();

        // Generation A: capture the survivor AND the dead sandbox —
        // both get chain-head records.
        let gen_a = {
            let p = PooledBackend::new(Arc::new(fake.clone()) as Arc<dyn SandboxBackend>)
                .with_chunk_store(cs.clone())
                .with_checkpoint_dir(checkpoint_dir.clone());
            let arc = Arc::new(p);
            arc.set_self_ref(&arc);
            arc
        };
        let survivor_meta = gen_a.snapshot(survivor).await.expect("survivor capture");
        let survivor_ref = survivor_meta.memory_manifest.expect("chunked");
        gen_a.snapshot(dead).await.expect("dead-sandbox capture");
        // A torn (unparseable) record for a third "survivor".
        let chains_dir = crate::checkpoint::ChainHeadRecord::subdir(&checkpoint_dir);
        tokio::fs::write(chains_dir.join(format!("{torn}.json")), b"not json")
            .await
            .unwrap();
        drop(gen_a); // the roll

        // Generation B: only `survivor` (and the torn id) reattached.
        let gen_b = {
            let p = PooledBackend::new(Arc::new(SurvivorListBackend {
                inner: fake,
                survivors: vec![survivor, torn],
            }) as Arc<dyn SandboxBackend>)
            .with_chunk_store(cs)
            .with_checkpoint_dir(checkpoint_dir.clone());
            let arc = Arc::new(p);
            arc.set_self_ref(&arc);
            arc
        };
        assert_eq!(gen_b.chain_head_for_test(survivor), None, "fresh map");
        gen_b.rehydrate_chain_heads().await;

        assert_eq!(
            gen_b.chain_head_for_test(survivor),
            Some(survivor_ref),
            "survivor's chain must be re-seeded from the durable record"
        );
        let record = crate::checkpoint::ChainHeadRecord::load(&chains_dir, survivor)
            .await
            .expect("survivor record");
        assert_eq!(
            gen_b.last_snapshot_unix_ms.get(&survivor).map(|v| *v),
            Some(record.updated_at.timestamp_millis()),
            "a survivor's checkpoint age continues from its record"
        );
        assert!(!gen_b.last_snapshot_unix_ms.contains_key(&torn));
        assert!(
            !chain_head_file(&checkpoint_dir, dead).exists(),
            "record for a non-surviving sandbox must be GC'd"
        );
        assert_eq!(
            gen_b.chain_head_for_test(torn),
            None,
            "a torn record must seed nothing (treated as absent)"
        );
    }

    // ---- balloon_inflate_for_seed matrix (adversarial-review fix) ----
    //
    // The invariant under test: after ANY outcome, either the balloon is
    // provably deflated / absent (Ok(false)), or the caller has been told
    // it owes a confirmed release (Ok(true)), or the capture fails (Err).
    // There is no path that proceeds with the balloon state unknown.

    mod balloon_matrix {
        use super::super::balloon_inflate_for_seed;
        use async_trait::async_trait;
        use engram_core::traits::sandbox::SandboxBackend;
        use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
        use engram_core::types::snapshot::SnapshotMetadata;
        use engram_core::{SandboxError, SandboxId};
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        struct BalloonMock {
            reclaim_result: Mutex<Option<Result<u64, SandboxError>>>,
            release_result: Mutex<Option<Result<(), SandboxError>>>,
            release_calls: AtomicUsize,
        }

        impl BalloonMock {
            fn new(reclaim: Result<u64, SandboxError>, release: Result<(), SandboxError>) -> Self {
                Self {
                    reclaim_result: Mutex::new(Some(reclaim)),
                    release_result: Mutex::new(Some(release)),
                    release_calls: AtomicUsize::new(0),
                }
            }
        }

        #[async_trait]
        impl SandboxBackend for BalloonMock {
            fn captures_guest_memory(&self) -> bool {
                false
            }
            async fn balloon_reclaim(
                &self,
                _: SandboxId,
                _: u64,
                _: std::time::Duration,
            ) -> Result<u64, SandboxError> {
                self.reclaim_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("reclaim called once")
            }
            async fn balloon_release(&self, _: SandboxId) -> Result<(), SandboxError> {
                self.release_calls.fetch_add(1, Ordering::SeqCst);
                self.release_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("release called at most once")
            }

            // ---- unused required surface ----
            async fn create(&self, _: SandboxId, _: SandboxSpec) -> Result<(), SandboxError> {
                unreachable!()
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                unreachable!()
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!()
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(
                &self,
                _: SandboxId,
                _: SnapshotMetadata,
                _disks: Option<engram_core::types::sandbox::SessionDisks>,
            ) -> Result<(), SandboxError> {
                unreachable!()
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                unreachable!()
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                unreachable!()
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                unreachable!()
            }
        }

        fn vm_err(msg: &str) -> SandboxError {
            SandboxError::Vm(msg.to_string().into())
        }

        const DL: std::time::Duration = std::time::Duration::from_secs(1);

        /// A landed inflate — even a 0-MiB grant — obligates a release.
        #[tokio::test]
        async fn landed_inflate_owes_a_release() {
            for granted in [0u64, 512] {
                let mock = BalloonMock::new(Ok(granted), Ok(()));
                let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                    .await
                    .unwrap();
                assert!(
                    inflated,
                    "granted={granted}: caller must be told to release"
                );
                assert_eq!(
                    mock.release_calls.load(Ordering::SeqCst),
                    0,
                    "the helper itself must not release on the happy path \
                     (the caller releases after the dump)"
                );
            }
        }

        /// The TYPED no-device outcome is the only fail-open: dense
        /// seed, no release owed, no release attempted.
        #[tokio::test]
        async fn typed_no_device_is_fail_open() {
            let mock = BalloonMock::new(
                Err(SandboxError::InvalidSpec("no balloon device".into())),
                Ok(()),
            );
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .unwrap();
            assert!(!inflated);
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 0);
        }

        /// An untyped reclaim failure (the inflate PATCH may have
        /// landed) is normalized via release-and-confirm; a confirmed
        /// release means a safe dense seed.
        #[tokio::test]
        async fn untyped_reclaim_failure_normalizes_via_release() {
            let mock = BalloonMock::new(Err(vm_err("stats poll: connection reset")), Ok(()));
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .unwrap();
            assert!(!inflated, "normalized ⇒ dense seed, nothing owed");
            assert_eq!(
                mock.release_calls.load(Ordering::SeqCst),
                1,
                "the normalizing release MUST run on an untyped reclaim failure"
            );
        }

        /// Reclaim failed AND the normalizing release failed: the
        /// balloon state is unknown — the capture must fail, never
        /// proceed to a warm hook.
        #[tokio::test]
        async fn unnormalizable_balloon_state_fails_the_capture() {
            let mock = BalloonMock::new(
                Err(vm_err("stats poll: connection reset")),
                Err(SandboxError::Snapshot("deflate did not complete".into())),
            );
            let err = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .expect_err("unknown balloon state must fail the capture");
            assert!(
                err.to_string().contains("balloon state unknown"),
                "error must say why: {err}"
            );
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 1);
        }

        /// Target 0 (tiny guest ≤ the reserve): no balloon interaction
        /// at all.
        #[tokio::test]
        async fn zero_target_never_touches_the_balloon() {
            let mock = BalloonMock::new(Ok(0), Ok(()));
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 0, DL)
                .await
                .unwrap();
            assert!(!inflated);
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 0);
            assert!(
                mock.reclaim_result.lock().unwrap().is_some(),
                "reclaim must not have been called"
            );
        }
    }
    struct HoldBackend {
        inner: FakeCaptureBackend,
        paused: std::sync::atomic::AtomicBool,
        rearms: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl SandboxBackend for HoldBackend {
        fn captures_guest_memory(&self) -> bool {
            self.inner.captures_guest_memory()
        }
        async fn create(&self, id: SandboxId, spec: SandboxSpec) -> Result<(), SandboxError> {
            self.inner.create(id, spec).await
        }
        async fn exec_stream(
            &self,
            id: SandboxId,
            cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            use std::sync::atomic::Ordering;
            assert!(
                !self.paused.load(Ordering::SeqCst),
                "guest exec while paused"
            );
            let script = cmd.command.join(" ");
            if script.contains("swapon") {
                self.rearms.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ExecStream {
                sandbox_id: id,
                exec_id: "swap".into(),
                events: Box::pin(futures::stream::iter([
                    engram_core::types::sandbox::ExecEvent::Stdout(bytes::Bytes::from_static(
                        b"SwapTotal: 1024 kB\nSwapFree: 1024 kB\nMemAvailable: 65536 kB\n",
                    )),
                    engram_core::types::sandbox::ExecEvent::Exit(Some(0)),
                ])),
            })
        }
        async fn pause(&self, _: SandboxId) -> Result<(), SandboxError> {
            self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn resume(&self, _: SandboxId) -> Result<(), SandboxError> {
            self.paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            panic!("hold must not call the capture that resumes")
        }
        async fn snapshot_hold(
            &self,
            id: SandboxId,
            _: bool,
        ) -> Result<SnapshotMetadata, SandboxError> {
            assert!(self.paused.load(std::sync::atomic::Ordering::SeqCst));
            self.inner.snapshot(id).await
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.inner.snapshot_path_for(id)
        }
        async fn restore(
            &self,
            id: SandboxId,
            meta: SnapshotMetadata,
            disks: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            self.inner.restore(id, meta, disks).await
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            self.inner.list().await
        }
    }
    fn held_fixture() -> (tempfile::TempDir, Arc<PooledBackend>, Arc<HoldBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(HoldBackend {
            inner: FakeCaptureBackend {
                payload: vec![0; 4096],
                staging_root: dir.path().join("snapshots"),
                destroy_calls: Arc::new(PlMutex::new(Vec::new())),
            },
            paused: std::sync::atomic::AtomicBool::new(false),
            rearms: std::sync::atomic::AtomicUsize::new(0),
        });
        let pooled = Arc::new(PooledBackend::new(inner.clone()));
        pooled.set_self_ref(&pooled);
        (dir, pooled, inner)
    }
    #[tokio::test]
    async fn snapshot_hold_stays_paused_without_swap_rearm() {
        let (_dir, pooled, inner) = held_fixture();
        let id = SandboxId::new();
        let first = pooled.snapshot_hold(id, false).await.unwrap();
        let retry = pooled.snapshot_hold(id, false).await.unwrap();
        assert_eq!(
            first.id, retry.id,
            "a lost response reuses the held capture"
        );
        tokio::task::yield_now().await;
        assert!(inner.paused.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(inner.rearms.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(pooled.held_snapshots.contains_key(&id));
        // The hold is a persisted source role: a host-agent restart
        // re-adopts the paused VM as frozen, never as a running guest.
        assert_eq!(
            pooled.migration_role(id),
            Some(crate::migration::MigrationRole::HeldSource)
        );
        assert!(
            pooled.snapshot(id).await.is_err(),
            "periodic capture must not touch a held VM"
        );
    }
    #[tokio::test]
    async fn resume_after_snapshot_hold_keeps_swap_active() {
        let (_dir, pooled, inner) = held_fixture();
        let id = SandboxId::new();
        pooled.snapshot_hold(id, false).await.unwrap();
        pooled.resume(id).await.unwrap();
        pooled.resume(id).await.unwrap();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(!inner.paused.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(inner.rearms.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!pooled.held_snapshots.contains_key(&id));
        assert_eq!(
            pooled.migration_role(id),
            None,
            "resume clears the held role"
        );
    }
    #[tokio::test]
    async fn destroy_after_snapshot_hold_clears_metadata() {
        let (_dir, pooled, inner) = held_fixture();
        let id = SandboxId::new();
        pooled.snapshot_hold(id, false).await.unwrap();
        pooled.destroy(id).await.unwrap();
        tokio::task::yield_now().await;
        assert!(!pooled.held_snapshots.contains_key(&id));
        assert_eq!(inner.rearms.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(inner.inner.destroy_calls.lock().as_slice(), &[id]);
    }
}
