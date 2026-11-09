//! ADR 0007 / ADR 0020 chunked-memory backend for the UFFD handler.
//!
//! Per page fault the handler asks this backend which bytes back a
//! given guest offset. ADR 0020 **Route B** serves *every* page from
//! chunks — there is no `memory.bin` mmap (the retired model
//! pointer-arithmetic'd into a single canonical mmap). The resolver
//! maps a chunk-aligned offset to one of:
//!
//! - a **session-divergent chunk hash** the session manifest places
//!   there (written-over state) — fetch + `UFFDIO_COPY`; or
//! - the **canonical** chunk at that offset (the common case for a
//!   freshly-resumed session, where the session agrees with the base)
//!   — installed shared via base-shm `UFFDIO_CONTINUE`, else fetch +
//!   COPY; or
//! - a **zero page** (`UFFDIO_ZEROPAGE`) when the session manifest
//!   omits the offset — the capture is full + zero-omitted, so an
//!   omission means the guest zeroed it (never served as base bytes).
//!
//! Why chunk-native (vs the retired `memory.bin` mmap):
//!
//! - **No materialize.** Restore never rebuilds a contiguous 4 GiB
//!   file; the handler faults chunks straight from the (prefetched)
//!   local chunk cache. That rebuild was the dominant restore cost
//!   ADR 0020 removes.
//! - **Cross-host portable.** Move a session's chunks to a fresh host
//!   (or boot a fresh host) and resolution is identical — no
//!   `memory.bin` file to ship.
//!
//! **Dedup (ADR 0045 substrate):** canonical pages now share one host
//! copy. When the runtime has a per-template base-shm attached, a page
//! still identical to the image base installs via `UFFDIO_CONTINUE`
//! over that shared backing — one host page-cache copy serves every
//! fresh and resumed VM of the image — and only session-divergent
//! pages stay private (`UFFDIO_COPY`). See ADR 0045; `runtime.rs` makes
//! the per-fault CONTINUE-vs-COPY call.
//!
//! What's in this module:
//!
//! - [`ChunkedMemoryBackend`] — the data plane: carries the canonical
//!   and session manifests, resolves an offset, and fetches chunks
//!   (local cache file, then the host-agent, then the object store).
//! - [`ResolvedPage`] — what the resolver returns. Types-only so the
//!   syscall path stays out of the unit-testable surface.
//!
//! What's NOT in this module:
//!
//! - The UFFD event loop + the actual `UFFDIO_COPY`/`UFFDIO_ZEROPAGE`
//!   (incl. the full-chunk-per-fault install policy). That's in
//!   `runtime.rs` and consumes this backend.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use engram_chunk_store::reader::ChunkCacheReader;
use engram_chunk_store::{ChunkHash, ChunkStore, Manifest, ManifestKind};
use engram_core::traits::BlobStorage;

/// What the per-fault resolver returns. Route B has no mmap, so the
/// discrimination is *what backs this guest offset*:
/// - `Chunk` — the session manifest's own hash here (diverged /
///   written-over state); fetch + `UFFDIO_COPY`.
/// - `Canonical` — the session agrees with the base image here; the
///   runtime installs the base's chunk (shared via the base-shm
///   `UFFDIO_CONTINUE`, else fetch + COPY).
///   For a base snapshot (session == canonical) every non-zero page
///   resolves to `Canonical`.
/// - `Zero` — the session omits this offset. The session manifest is
///   a *full*, zero-omitted capture, so omission means the guest
///   zeroed it: install a zero page (`UFFDIO_ZEROPAGE`) regardless of
///   what the base holds there. A base-nonzero chunk the session
///   zeroed must NOT be served as stale base content.
#[derive(Clone, Debug)]
pub enum ResolvedPage {
    /// Session agrees with the base image here: `hash` is the chunk
    /// both manifests name.
    Canonical { hash: ChunkHash },
    /// Page diverges from canonical. The named chunk is what to
    /// fetch and copy into the guest, via
    /// [`ChunkedMemoryBackend::fetch_chunk`].
    Chunk { hash: ChunkHash },
    /// The session manifest omits this chunk-aligned offset. Because
    /// the session capture is full + zero-omitted (see `file.rs`'s
    /// sparse capture), an omission is an authoritative "all zero
    /// here" — install `UFFDIO_ZEROPAGE`, never base content.
    Zero,
}

/// In-memory shape of the canonical + session manifests. We pre-
/// parse the manifest's `Vec<ChunkRef>` into a positional array
/// indexed by `chunk_index = offset / chunk_size`. Reads are then
/// O(1) per fault rather than the O(log n) binary search the
/// generic `Manifest::chunk_at(offset)` does.
///
/// `None` entries represent zero-filled chunks (`Manifest::chunks`
/// is sparse — offsets without a `ChunkRef` are implicit zero
/// pages).
#[derive(Clone, Debug)]
struct PositionalManifest {
    chunks: Vec<Option<ChunkHash>>,
    chunk_size: u64,
    total_bytes: u64,
}

impl PositionalManifest {
    fn from_manifest(m: &Manifest) -> Result<Self, ChunkedBackendError> {
        if !matches!(m.kind, ManifestKind::Memory) {
            return Err(ChunkedBackendError::WrongKind(format!(
                "expected ManifestKind::Memory, got {:?}",
                m.kind
            )));
        }
        let chunk_size = m.chunk_size.as_u64();
        if chunk_size == 0 {
            return Err(ChunkedBackendError::InvalidManifest(
                "chunk_size is zero".into(),
            ));
        }
        // chunk_count = ceil(total_bytes / chunk_size); the final
        // chunk may be short, but we still index it.
        let chunk_count = m.total_bytes.div_ceil(chunk_size) as usize;
        let mut chunks = vec![None; chunk_count];
        for entry in &m.chunks {
            if entry.offset % chunk_size != 0 {
                return Err(ChunkedBackendError::InvalidManifest(format!(
                    "chunk offset {} is not a multiple of chunk_size {}",
                    entry.offset, chunk_size,
                )));
            }
            let idx = (entry.offset / chunk_size) as usize;
            if idx >= chunks.len() {
                return Err(ChunkedBackendError::InvalidManifest(format!(
                    "chunk at offset {} (idx {idx}) exceeds total_bytes {}",
                    entry.offset, m.total_bytes,
                )));
            }
            chunks[idx] = Some(entry.hash);
        }
        Ok(Self {
            chunks,
            chunk_size,
            total_bytes: m.total_bytes,
        })
    }
}

/// The data plane the runtime drives. Constructed once at
/// handler startup; cheap to clone (it's all `Arc`-backed).
pub struct ChunkedMemoryBackend {
    canonical: PositionalManifest,
    session: PositionalManifest,
    session_positions: std::collections::HashMap<ChunkHash, Vec<u64>>,
    /// The read-only view of the shared cache dir (ADR 0075): the
    /// handler cannot change the directory.
    reader: ChunkCacheReader,
    /// The channel to the one writer (the host-agent). `None` when no
    /// writer answered at startup: misses go to the object store.
    populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    /// The object store, used only when the writer is unreachable.
    /// Chunks from it stay in memory and never reach the cache dir.
    store: ChunkStore,
}

/// See [`ChunkedMemoryBackend::composition`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Composition {
    pub canonical: usize,
    pub divergent: usize,
    pub divergent_local: usize,
    pub zero: usize,
}

/// Anything that can go wrong in this module. Kept distinct from
/// `HandlerError` so unit tests don't need a UFFD setup.
#[derive(Debug)]
pub enum ChunkedBackendError {
    Io(std::io::Error),
    Manifest(engram_chunk_store::ChunkStoreError),
    /// Caller passed a non-memory manifest.
    WrongKind(String),
    /// Manifest internals are structurally broken (mis-aligned
    /// offset, out-of-range chunk index, mismatched chunk_size).
    InvalidManifest(String),
    /// Canonical and session manifests don't agree on chunk_size
    /// or total_bytes. Both would have to be wrong for this to
    /// happen — bail loud.
    Mismatch(String),
}

impl std::fmt::Display for ChunkedBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Manifest(e) => write!(f, "manifest: {e}"),
            Self::WrongKind(m) => write!(f, "wrong manifest kind: {m}"),
            Self::InvalidManifest(m) => write!(f, "invalid manifest: {m}"),
            Self::Mismatch(m) => write!(f, "canonical/session manifest mismatch: {m}"),
        }
    }
}

impl std::error::Error for ChunkedBackendError {}

impl From<std::io::Error> for ChunkedBackendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<engram_chunk_store::ChunkStoreError> for ChunkedBackendError {
    fn from(e: engram_chunk_store::ChunkStoreError) -> Self {
        Self::Manifest(e)
    }
}

/// Local copies of the manifests, which the backend reads from disk
/// instead of the blob store: a migration's not-yet-durable session
/// manifest, or the copies the host stages beside a snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalManifests<'a> {
    pub canonical: Option<&'a Path>,
    pub session: Option<&'a Path>,
}

/// The manifest `r`, from the local copy at `path` when there is one. A
/// copy that does not read gives a warning, and the store serves the same
/// version; a migration manifest is not in the store yet, so that read fails.
async fn read_manifest(
    store: &engram_chunk_store::ChunkStore,
    path: Option<&Path>,
    r: engram_core::types::manifest::ManifestRef,
    which: &str,
) -> Result<engram_chunk_store::Manifest, ChunkedBackendError> {
    if let Some(path) = path {
        let local = std::fs::read(path)
            .map_err(ChunkedBackendError::Io)
            .and_then(|bytes| {
                serde_json::from_slice::<engram_chunk_store::Manifest>(&bytes).map_err(|e| {
                    ChunkedBackendError::InvalidManifest(format!(
                        "parse local {which} manifest {}: {e}",
                        path.display()
                    ))
                })
            })
            .and_then(|m| {
                m.validate()
                    .map_err(ChunkedBackendError::Manifest)
                    .map(|()| m)
            });
        match local {
            Ok(m) => return Ok(m),
            Err(error) => tracing::warn!(
                %error,
                manifest = %r,
                "local {which} manifest unreadable; reading the store",
            ),
        }
    }
    Ok(store.get_manifest(r).await?)
}

impl ChunkedMemoryBackend {
    /// Build from already-loaded canonical + session manifests, the
    /// read-only cache view, the populate channel to the writer, and
    /// the store for the fallback path.
    pub fn new(
        canonical: &Manifest,
        session: &Manifest,
        reader: ChunkCacheReader,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
        store: ChunkStore,
    ) -> Result<Self, ChunkedBackendError> {
        let canonical = PositionalManifest::from_manifest(canonical)?;
        let session = PositionalManifest::from_manifest(session)?;
        if canonical.chunk_size != session.chunk_size {
            return Err(ChunkedBackendError::Mismatch(format!(
                "chunk_size: canonical={} session={}",
                canonical.chunk_size, session.chunk_size
            )));
        }
        if canonical.total_bytes != session.total_bytes {
            return Err(ChunkedBackendError::Mismatch(format!(
                "total_bytes: canonical={} session={}",
                canonical.total_bytes, session.total_bytes
            )));
        }
        let mut session_positions = std::collections::HashMap::<ChunkHash, Vec<u64>>::new();
        for (idx, hash) in session.chunks.iter().enumerate() {
            if let Some(hash) = hash {
                session_positions
                    .entry(*hash)
                    .or_default()
                    .push(idx as u64 * session.chunk_size);
            }
        }
        Ok(Self {
            canonical,
            session,
            session_positions,
            reader,
            populate,
            store,
        })
    }

    /// Convenience: read both manifests off `BlobStorage` then
    /// build. Used by the binary at startup. The handler binary
    /// constructs its own blob handle from env (`ENGRAM_BLOB_BACKEND`)
    /// upstream of this call.
    pub async fn from_blob(
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    ) -> Result<Self, ChunkedBackendError> {
        Self::from_blob_with_local(
            canonical_ref,
            session_ref,
            LocalManifests::default(),
            blob,
            cache_root,
            populate,
        )
        .await
    }

    /// Like [`Self::from_blob`], but a manifest with a copy in `local` is
    /// read from that file instead of the blob store.
    /// ADR 0045 D4 fail-open (#1066): build with the supplied canonical,
    /// and when the canonical/session manifests disagree on layout
    /// (guest resized since capture, FC snapshot cutover, ...), retry
    /// with `canonical == session` — the documented pre-D4 fallback.
    /// The session's own manifest describes its complete memory image,
    /// so the fallback is a full-fidelity restore; it only forgoes
    /// shared-base density the mismatched session could never use.
    /// Returns `(backend, fell_back)`; the caller MUST drop the
    /// template-keyed base shm when `fell_back` is true — that file is
    /// sized for the canonical layout and is invalid for this session.
    pub async fn build_with_canonical_fallback(
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        local: LocalManifests<'_>,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    ) -> Result<(Self, bool), ChunkedBackendError> {
        match Self::from_blob_with_local(
            canonical_ref,
            session_ref,
            local,
            blob.clone(),
            cache_root,
            populate.clone(),
        )
        .await
        {
            Ok(b) => Ok((b, false)),
            Err(ChunkedBackendError::Mismatch(m)) if canonical_ref != session_ref => {
                tracing::warn!(
                    mismatch = %m,
                    canonical = %canonical_ref,
                    session = %session_ref,
                    "canonical/session manifest mismatch; failing open to \
                     canonical == session (session-keyed restore, no shared base)",
                );
                Self::from_blob_with_local(
                    session_ref,
                    session_ref,
                    LocalManifests {
                        canonical: None,
                        session: local.session,
                    },
                    blob,
                    cache_root,
                    populate,
                )
                .await
                .map(|b| (b, true))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn from_blob_with_local(
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        local: LocalManifests<'_>,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    ) -> Result<Self, ChunkedBackendError> {
        let store = engram_chunk_store::ChunkStore::new(blob);
        // ADR 0045: when the canonical and session refs coincide on a
        // migration restore (no image-base rider), the canonical IS the
        // local file too — store-fetching it would 404 (the v+1 manifest
        // is deliberately unpublished until the catch-up).
        let canonical_path = if canonical_ref == session_ref {
            local.session
        } else {
            local.canonical
        };
        // The two reads are independent, so they run together.
        let (canonical, session) = tokio::try_join!(
            read_manifest(&store, canonical_path, canonical_ref, "canonical"),
            read_manifest(&store, local.session, session_ref, "session"),
        )?;
        let reader = ChunkCacheReader::new(cache_root);
        Self::new(&canonical, &session, reader, populate, store)
    }

    /// Bytes per chunk. UFFDIO_COPY copies a full chunk at a time
    /// to amortise the fault cost (subsequent accesses within the
    /// chunk don't fault again).
    pub fn chunk_size(&self) -> u64 {
        self.canonical.chunk_size
    }

    /// True when the writer (the host-agent) fills the cache: its
    /// prefetch lands the chunks, so a producer can wait for them.
    pub fn has_writer(&self) -> bool {
        self.populate.is_some()
    }

    /// True when the local chunk cache holds `hash`.
    pub fn is_resident(&self, hash: ChunkHash) -> bool {
        self.reader.contains_on_disk(hash)
    }

    /// Total backing-store size. Should match what FC's UFFD
    /// regions sum to — the runtime's handshake validation does
    /// the cross-check.
    pub fn total_bytes(&self) -> u64 {
        self.canonical.total_bytes
    }

    /// Resolve the chunk-aligned region containing `byte_offset`.
    /// Pure data-plane; no I/O.
    ///
    /// Returns `None` when the offset is past `total_bytes`. The
    /// runtime treats that as a programming bug — FC's mappings
    /// shouldn't reference past-EOF — and surfaces it loud.
    pub fn resolve(&self, byte_offset: u64) -> Option<ResolvedPage> {
        if byte_offset >= self.canonical.total_bytes {
            return None;
        }
        let chunk_idx = (byte_offset / self.canonical.chunk_size) as usize;
        let canon = self.canonical.chunks.get(chunk_idx).copied().flatten();
        let session = self.session.chunks.get(chunk_idx).copied().flatten();
        Some(match (canon, session) {
            // Session agrees with the base here → share the canonical
            // page (base-shm CONTINUE / fetch + COPY).
            (Some(c), Some(s)) if c == s => ResolvedPage::Canonical { hash: c },
            // Session has its own hash here — diverged / written-over
            // state. Use it regardless of what the base holds.
            (_, Some(s)) => ResolvedPage::Chunk { hash: s },
            // Session omits this offset. The session manifest is a
            // full, zero-omitted capture, so omission == "the guest
            // zeroed this chunk" — install a zero page regardless of
            // the base. Serving canonical content here when the base
            // is non-zero would silently un-zero guest RAM on resume.
            (_, None) => ResolvedPage::Zero,
        })
    }

    /// The bytes of one chunk, from the first tier that has it:
    ///
    /// 1. The local cache file, mapped (no read into a buffer).
    /// 2. The writer (the host-agent), which makes it available through
    ///    the fill engine; misses from every handler collapse there.
    /// 3. Only when the writer is unreachable (a host-agent roll, or a
    ///    writer past the client's reply timeout): the object store
    ///    directly, hash-verified, kept in memory and never written to
    ///    the cache. Each fault thread waits on at most one such fetch,
    ///    so the fault threads bound the fan-out. It is the only fetch
    ///    outside the fill engine, and it runs only while no engine can
    ///    answer: a handler with no writer at startup (bake and test
    ///    runs) and a writer that is gone mid-run (a guest keeps running
    ///    through the roll).
    ///
    /// Synchronous: the fault and producer threads call it, never a
    /// runtime worker. `rt` runs the object-store fetch of step 3.
    pub fn fetch_chunk(
        &self,
        rt: &tokio::runtime::Handle,
        hash: ChunkHash,
    ) -> Result<Bytes, ChunkedBackendError> {
        match std::fs::File::open(self.reader.path_for(hash)) {
            Ok(file) => {
                crate::fault_stats::set_fetch_source(crate::fault_stats::Source::Cache);
                return Ok(crate::mapped::chunk_file_bytes(&file)?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(client) = &self.populate {
            match client.request(hash) {
                Ok(bytes) => {
                    crate::fault_stats::set_fetch_source(crate::fault_stats::Source::Populate);
                    return Ok(bytes);
                }
                Err(crate::populate_client::PopulateError::WriterUnreachable(msg)) => {
                    // The handler exports no metrics; this WARN is the
                    // signal, and the writer's populate counter going
                    // quiet confirms it.
                    tracing::warn!(
                        %hash,
                        %msg,
                        "substrate writer unreachable; direct object-store fetch (uncached)",
                    );
                }
                Err(e) => {
                    return Err(ChunkedBackendError::Io(std::io::Error::other(format!(
                        "populate: {e}"
                    ))));
                }
            }
        }
        crate::fault_stats::set_fetch_source(crate::fault_stats::Source::Fallback);
        // A guest waits on this fault.
        rt.block_on(engram_chunk_store::ChunkSource::chunk(
            &self.store,
            hash,
            engram_chunk_store::Class::Demand,
        ))
        .map_err(Into::into)
    }

    /// Chunk counts by how a fault would resolve them: canonical,
    /// divergent (and how many of those the local cache holds), zero.
    pub fn composition(&self) -> Composition {
        let mut c = Composition::default();
        for idx in 0..self.session.chunks.len() {
            match self.resolve(idx as u64 * self.session.chunk_size) {
                Some(ResolvedPage::Canonical { .. }) => c.canonical += 1,
                Some(ResolvedPage::Chunk { hash }) => {
                    c.divergent += 1;
                    if self.reader.contains_on_disk(hash) {
                        c.divergent_local += 1;
                    }
                }
                Some(ResolvedPage::Zero) => c.zero += 1,
                None => {}
            }
        }
        c
    }

    /// Chunk-start byte offsets where the session manifest holds
    /// `hash`. The drain orders a hot-set hint (chunk hashes) by these
    /// positions.
    ///
    /// Built once at load, so ordering a large hot set is linear in the
    /// number of returned positions instead of hint length times RAM size.
    pub fn session_positions_of(&self, hash: ChunkHash) -> Vec<u64> {
        self.session_positions
            .get(&hash)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use engram_chunk_store::manifest::{ChunkRef, ManifestRef, MANIFEST_SCHEMA_VERSION};
    use engram_chunk_store::{ChunkSize, ChunkStore};
    use engram_storage_local::LocalBlobStorage;

    fn mem_manifest(total_bytes: u64, chunk_size: u64, tag: u8) -> Manifest {
        let n = total_bytes / chunk_size;
        let chunks = (0..n)
            .map(|i| ChunkRef {
                offset: i * chunk_size,
                hash: ChunkHash::of(&[tag, i as u8]),
            })
            .collect();
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes,
            chunks,
            parent: None,
            annotations: serde_json::Value::Null,
        }
    }

    async fn put(store: &ChunkStore, m: &Manifest) -> ManifestRef {
        let r = ManifestRef::new();
        store.put_manifest(r, m).await.unwrap();
        r
    }

    #[tokio::test]
    async fn hash_index_keeps_all_positions_in_offset_order() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        let mut manifest = mem_manifest(4 * 4096, 4096, 3);
        let repeated = manifest.chunks[0].hash;
        manifest.chunks[2].hash = repeated;
        manifest.chunks.remove(1); // sparse zero chunk has no hash
        let r = put(&store, &manifest).await;
        let (backend, _) = ChunkedMemoryBackend::build_with_canonical_fallback(
            r,
            r,
            LocalManifests::default(),
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(backend.session_positions_of(repeated), vec![0, 8192]);
        assert!(backend
            .session_positions_of(ChunkHash::of(b"absent"))
            .is_empty());
    }

    #[tokio::test]
    async fn a_local_copy_that_does_not_parse_reads_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        let manifest = mem_manifest(4 * 4096, 4096, 7);
        let r = put(&store, &manifest).await;
        let torn = dir.path().join("torn.json");
        std::fs::write(&torn, b"").unwrap();
        let (backend, _) = ChunkedMemoryBackend::build_with_canonical_fallback(
            r,
            r,
            LocalManifests {
                canonical: Some(&torn),
                session: Some(&torn),
            },
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            backend.session_positions_of(manifest.chunks[1].hash),
            vec![4096]
        );
    }

    /// #1066: a session whose layout predates a guest resize must fail
    /// open to canonical == session instead of exiting — the loop that
    /// stranded parked sessions for hours.
    #[tokio::test]
    async fn mismatched_canonical_falls_back_to_session() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        // Canonical: 8 MiB "template"; session: 4 MiB "old era".
        let canonical = put(&store, &mem_manifest(8 << 20, 1 << 20, 0xca)).await;
        let session = put(&store, &mem_manifest(4 << 20, 1 << 20, 0x5e)).await;

        let (backend, fell_back) = ChunkedMemoryBackend::build_with_canonical_fallback(
            canonical,
            session,
            LocalManifests::default(),
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .expect("fallback must succeed");
        assert!(
            fell_back,
            "mismatch must trigger the session-canonical fallback"
        );
        assert_eq!(backend.total_bytes(), 4 << 20, "layout is the SESSION's");
    }

    /// Matching layouts keep the shared-base pairing (no fallback).
    #[tokio::test]
    async fn matching_canonical_does_not_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        let canonical = put(&store, &mem_manifest(4 << 20, 1 << 20, 0xca)).await;
        let session = put(&store, &mem_manifest(4 << 20, 1 << 20, 0x5e)).await;

        let (_backend, fell_back) = ChunkedMemoryBackend::build_with_canonical_fallback(
            canonical,
            session,
            LocalManifests::default(),
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .expect("matching layouts build");
        assert!(!fell_back);
    }

    /// A mismatch with canonical == session has no fallback to take —
    /// it must still bail loud (structurally impossible pairing).
    #[tokio::test]
    async fn identical_refs_never_loop_on_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = ChunkStore::new(blob.clone());
        let session = put(&store, &mem_manifest(4 << 20, 1 << 20, 0x5e)).await;
        let (_b, fell_back) = ChunkedMemoryBackend::build_with_canonical_fallback(
            session,
            session,
            LocalManifests::default(),
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .expect("self-pairing builds");
        assert!(!fell_back);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::manifest::{ChunkRef, MANIFEST_SCHEMA_VERSION};
    use engram_chunk_store::{ChunkSize, ChunkStore};
    use engram_storage_local::LocalBlobStorage;

    fn synth_manifest(
        total_bytes: u64,
        chunk_size: u64,
        chunks: Vec<(u64, ChunkHash)>,
    ) -> Manifest {
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes,
            chunks: chunks
                .into_iter()
                .map(|(offset, hash)| ChunkRef { offset, hash })
                .collect(),
            parent: None,
            annotations: serde_json::Value::Null,
        }
    }

    fn make_cache_and_store() -> (ChunkCacheReader, ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let cs = ChunkStore::new(blob);
        (ChunkCacheReader::new(dir.path().join("cache")), cs, dir)
    }

    /// Synthesize a distinct ChunkHash per byte tag. The actual
    /// hash values don't matter — only equality across canonical
    /// vs session does — but using `ChunkHash::of(&[byte])` gives
    /// us a stable, real sha256 derivation so the test stays
    /// representative of production hash semantics.
    fn h(byte: u8) -> ChunkHash {
        ChunkHash::of(&[byte])
    }

    /// ADR 0070: every cache this handler builds from a blob store must
    /// have eviction disabled — the host-agent (holding the pin set) is
    /// the one process per host that evicts. This is the single-evictor
    /// invariant `--cache-budget-bytes` used to violate.
    #[tokio::test]
    async fn from_blob_built_cache_has_eviction_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let cs = ChunkStore::new(blob.clone());
        let canonical = synth_manifest(512, 512, vec![(0, h(1))]);
        let canonical_ref = engram_core::types::manifest::ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: 1,
        };
        cs.put_manifest(canonical_ref, &canonical).await.unwrap();

        let backend = ChunkedMemoryBackend::from_blob(
            canonical_ref,
            canonical_ref,
            blob,
            &dir.path().join("cache"),
            None,
        )
        .await
        .unwrap();
        // ADR 0075: the handler's view is read-only BY TYPE — there is
        // no cache field to mis-configure anymore; the eviction_enabled
        // assertion this replaced is unrepresentable.
        let _ = &backend;
    }

    /// ADR 0075 roll-survival: with the populate socket dead (a
    /// host-agent mid-roll), a fault still resolves via the direct-blob
    /// fallback within the retry budget, serves correct bytes, and
    /// writes NOTHING to the cache dir (the single-writer invariant
    /// holds even in degraded mode).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fallback_serves_from_blob_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let cs = ChunkStore::new(blob);
        let bytes = bytes::Bytes::from(vec![9u8; 2048]);
        let hash = cs.put_chunk(&bytes).await.unwrap();

        let cache_root = dir.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();
        let canonical = synth_manifest(2048, 2048, vec![(0, hash)]);
        let dead_sock = dir.path().join("nonexistent.sock");
        let b = ChunkedMemoryBackend::new(
            &canonical,
            &canonical,
            ChunkCacheReader::new(cache_root.clone()),
            Some(std::sync::Arc::new(
                crate::populate_client::PopulateClient::new(dead_sock),
            )),
            cs,
        )
        .unwrap();

        let rt = tokio::runtime::Handle::current();
        let got = tokio::task::spawn_blocking(move || b.fetch_chunk(&rt, hash))
            .await
            .unwrap()
            .expect("fallback fetch");
        assert_eq!(got, bytes, "fallback must serve correct bytes");

        // The single-writer invariant in degraded mode: nothing landed
        // in the cache dir.
        let entries: Vec<_> = walk(&cache_root);
        assert!(
            entries.is_empty(),
            "fallback must not write to the cache dir, found {entries:?}",
        );
    }

    fn walk(root: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn resolves_canonical_match_to_the_canonical_hash() {
        // Both canonical and session agree on chunks 0 and 1.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        assert!(matches!(
            b.resolve(0),
            Some(ResolvedPage::Canonical { hash }) if hash == h(1)
        ));
        assert!(matches!(
            b.resolve(512),
            Some(ResolvedPage::Canonical { hash }) if hash == h(2)
        ));
    }

    #[test]
    fn resolves_session_divergence_to_chunk_fetch() {
        // Session's chunk 1 differs from canonical's chunk 1 →
        // runtime fetches via the cache.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(7))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        match b.resolve(512) {
            Some(ResolvedPage::Chunk { hash }) => assert_eq!(hash, h(7)),
            other => panic!("expected Chunk(h(7)), got {other:?}"),
        }
    }

    #[test]
    fn resolves_within_a_chunk_to_chunk_start_offset() {
        // A fault that lands at byte 600 (within chunk 1 at
        // offset 512) resolves the same as a fault at byte 512.
        // Subsequent pages within the chunk get the rest of the
        // chunk's bytes via the runtime's full-chunk COPY policy.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        let r = b.resolve(600).unwrap();
        match r {
            ResolvedPage::Canonical { hash } => assert_eq!(hash, h(2)),
            other => panic!("expected Canonical(h(2)), got {other:?}"),
        }
    }

    #[test]
    fn resolves_session_omitted_chunk_to_zero_when_base_also_zero() {
        // Both manifests omit chunk 1 (base zero, session zero). An
        // omission in the full session capture means "zero here", so
        // resolve returns Zero — the runtime installs UFFDIO_ZEROPAGE.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        assert!(matches!(b.resolve(512), Some(ResolvedPage::Zero)));
    }

    #[test]
    fn resolves_session_zeroed_base_nonzero_chunk_to_zero_not_canonical() {
        // Regression: the base image HAS content at chunk 1 (h(2)) but
        // the session zeroed it, so the full session manifest OMITS it.
        // resolve MUST return Zero — serving the base's h(2) here would
        // silently un-zero guest RAM on resume (the `(Some(c), None)`
        // bug where the zero arm collapsed into Canonical).
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        match b.resolve(512) {
            Some(ResolvedPage::Zero) => {}
            other => {
                panic!("expected Zero for a session-zeroed base-nonzero chunk, got {other:?}")
            }
        }
    }

    #[test]
    fn resolves_past_eof_returns_none() {
        let canonical = synth_manifest(1024, 512, vec![]);
        let session = synth_manifest(1024, 512, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
        assert!(b.resolve(1024).is_none(), "exactly at total_bytes");
        assert!(b.resolve(99999).is_none(), "past total_bytes");
    }

    #[test]
    fn rejects_chunk_size_mismatch() {
        let canonical = synth_manifest(1024, 512, vec![]);
        let session = synth_manifest(1024, 256, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        // `ChunkedMemoryBackend` doesn't impl Debug (its ChunkStore /
        // ChunkCache don't), so we can't use `unwrap_err`. Match by
        // hand.
        match ChunkedMemoryBackend::new(&canonical, &session, cache, None, store) {
            Ok(_) => panic!("chunk_size mismatch must reject"),
            Err(e) => assert!(format!("{e}").contains("chunk_size")),
        }
    }

    #[test]
    fn rejects_total_bytes_mismatch() {
        let canonical = synth_manifest(2048, 512, vec![]);
        let session = synth_manifest(1024, 512, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        match ChunkedMemoryBackend::new(&canonical, &session, cache, None, store) {
            Ok(_) => panic!("total_bytes mismatch must reject"),
            Err(e) => assert!(format!("{e}").contains("total_bytes")),
        }
    }

    #[test]
    fn rejects_non_memory_manifest_kind() {
        let mut disk = synth_manifest(1024, 512, vec![]);
        disk.kind = ManifestKind::Disk;
        let session = synth_manifest(1024, 512, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        match ChunkedMemoryBackend::new(&disk, &session, cache, None, store) {
            Ok(_) => panic!("non-memory manifest must reject"),
            Err(e) => assert!(format!("{e}").contains("Memory")),
        }
    }
}
