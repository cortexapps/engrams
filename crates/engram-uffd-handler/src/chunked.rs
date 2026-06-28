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
//!   — the runtime resolves its hash via
//!   [`ChunkedMemoryBackend::canonical_chunk_hash`] and installs it
//!   (shared via base-shm `UFFDIO_CONTINUE`, else fetch + COPY); or
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
//!   via a [`ChunkCache`] over a [`ChunkStore`].
//! - [`ResolvedPage`] — what the resolver returns. Types-only so the
//!   syscall path stays out of the unit-testable surface.
//!
//! What's NOT in this module:
//!
//! - The UFFD event loop + the actual `UFFDIO_COPY`/`UFFDIO_ZEROPAGE`
//!   (incl. the full-chunk-per-fault install policy). That's in
//!   `runtime.rs` and consumes this backend.
//! - Working-set trace recording. That's in `working_set.rs`.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use engram_chunk_store::{ChunkCache, ChunkHash, ChunkStore, Manifest, ManifestKind};
use engram_core::traits::BlobStorage;

/// What the per-fault resolver returns. Route B has no mmap, so the
/// discrimination is *what backs this guest offset*:
/// - `Chunk` — the session manifest's own hash here (diverged /
///   written-over state); fetch + `UFFDIO_COPY`.
/// - `Canonical` — the session agrees with the base image here; the
///   runtime resolves the canonical hash via
///   [`ChunkedMemoryBackend::canonical_chunk_hash`] and installs it
///   (shared via the base-shm `UFFDIO_CONTINUE`, else fetch + COPY).
///   For a base snapshot (session == canonical) every non-zero page
///   resolves to `Canonical`.
/// - `Zero` — the session omits this offset. The session manifest is
///   a *full*, zero-omitted capture, so omission means the guest
///   zeroed it: install a zero page (`UFFDIO_ZEROPAGE`) regardless of
///   what the base holds there. A base-nonzero chunk the session
///   zeroed must NOT be served as stale base content.
#[derive(Clone, Debug)]
pub enum ResolvedPage {
    /// Session agrees with the base image at this chunk-aligned byte
    /// offset. The runtime looks up the canonical chunk hash here and
    /// installs it (shared base-shm CONTINUE, else fetch + COPY).
    Canonical { canonical_offset: u64 },
    /// Page diverges from canonical. The named chunk is what to
    /// fetch from the store + copy into the guest. The runtime
    /// hands this hash to the [`ChunkCache`] (via
    /// [`ChunkedMemoryBackend::fetch_chunk`]) for the bytes.
    Chunk { hash: ChunkHash },
    /// The session manifest omits this chunk-aligned offset. Because
    /// the session capture is full + zero-omitted (see `file.rs`'s
    /// sparse capture), an omission is an authoritative "all zero
    /// here" — install `UFFDIO_ZEROPAGE`, never base content.
    Zero { offset: u64 },
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
    cache: ChunkCache,
    /// Backend the cache calls into on miss. Held here (rather
    /// than on the cache itself) so the cache stays
    /// backend-agnostic — see the docstring on `ChunkCache`.
    store: ChunkStore,
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

impl ChunkedMemoryBackend {
    /// Build from already-loaded canonical + session manifests
    /// and a chunk cache + the store the cache should miss
    /// through.
    pub fn new(
        canonical: &Manifest,
        session: &Manifest,
        cache: ChunkCache,
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
        Ok(Self {
            canonical,
            session,
            cache,
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
        cache_budget_bytes: u64,
    ) -> Result<Self, ChunkedBackendError> {
        Self::from_blob_with_session_json(
            canonical_ref,
            session_ref,
            None,
            blob,
            cache_root,
            cache_budget_bytes,
        )
        .await
    }

    /// ADR 0045 C1: like [`Self::from_blob`], but when
    /// `session_manifest_json` is `Some(path)` the SESSION manifest is
    /// read from that local file instead of the blob store. A migration
    /// destination restores from a manifest that is deliberately NOT
    /// yet durable (the catch-up upload publishes it later) — its
    /// chunks are already resident in the host NVMe cache, so the
    /// fill path never needs the store for them. The canonical (image
    /// base) manifest is always durable and store-resolved.
    pub async fn from_blob_with_session_json(
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        session_manifest_json: Option<&Path>,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        cache_budget_bytes: u64,
    ) -> Result<Self, ChunkedBackendError> {
        let store = engram_chunk_store::ChunkStore::new(blob);
        // ADR 0045 C1: when the canonical and session refs coincide on a
        // migration restore (no image-base rider), the canonical IS the
        // local file too — store-fetching it would 404 (the v+1 manifest
        // is deliberately unpublished until the catch-up).
        let canonical = match session_manifest_json {
            Some(path) if canonical_ref == session_ref => {
                let bytes = std::fs::read(path).map_err(ChunkedBackendError::Io)?;
                let m: engram_chunk_store::Manifest =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        ChunkedBackendError::InvalidManifest(format!(
                            "parse local canonical manifest {}: {e}",
                            path.display()
                        ))
                    })?;
                m.validate().map_err(ChunkedBackendError::Manifest)?;
                m
            }
            _ => store.get_manifest(canonical_ref).await?,
        };
        let session = match session_manifest_json {
            Some(path) => {
                let bytes = std::fs::read(path).map_err(ChunkedBackendError::Io)?;
                let m: engram_chunk_store::Manifest =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        ChunkedBackendError::InvalidManifest(format!(
                            "parse local session manifest {}: {e}",
                            path.display()
                        ))
                    })?;
                m.validate().map_err(ChunkedBackendError::Manifest)?;
                let _ = session_ref; // named by ref for logging only
                m
            }
            None => store.get_manifest(session_ref).await?,
        };
        let mut cfg = engram_chunk_store::cache::ChunkCacheConfig::new(cache_root.to_path_buf());
        cfg.budget_bytes = cache_budget_bytes;
        let cache = ChunkCache::new(cfg);
        Self::new(&canonical, &session, cache, store)
    }

    /// Bytes per chunk. UFFDIO_COPY copies a full chunk at a time
    /// to amortise the fault cost (subsequent accesses within the
    /// chunk don't fault again).
    pub fn chunk_size(&self) -> u64 {
        self.canonical.chunk_size
    }

    /// Total backing-store size. Should match what FC's UFFD
    /// regions sum to — the runtime's handshake validation does
    /// the cross-check.
    pub fn total_bytes(&self) -> u64 {
        self.canonical.total_bytes
    }

    /// Resolve the chunk-aligned region containing `byte_offset`
    /// into either "matches canonical here" (runtime resolves the
    /// canonical hash / zero-fills) or "fetch this session-divergent
    /// chunk." Pure data-plane; no I/O.
    ///
    /// Returns `None` when the offset is past `total_bytes`. The
    /// runtime treats that as a programming bug — FC's mappings
    /// shouldn't reference past-EOF — and surfaces it loud.
    pub fn resolve(&self, byte_offset: u64) -> Option<ResolvedPage> {
        if byte_offset >= self.canonical.total_bytes {
            return None;
        }
        let chunk_idx = (byte_offset / self.canonical.chunk_size) as usize;
        let chunk_offset = (chunk_idx as u64) * self.canonical.chunk_size;
        let canon = self.canonical.chunks.get(chunk_idx).copied().flatten();
        let session = self.session.chunks.get(chunk_idx).copied().flatten();
        Some(match (canon, session) {
            // Session agrees with the base here → share the canonical
            // page (base-shm CONTINUE / fetch + COPY).
            (Some(c), Some(s)) if c == s => ResolvedPage::Canonical {
                canonical_offset: chunk_offset,
            },
            // Session has its own hash here — diverged / written-over
            // state. Use it regardless of what the base holds.
            (_, Some(s)) => ResolvedPage::Chunk { hash: s },
            // Session omits this offset. The session manifest is a
            // full, zero-omitted capture, so omission == "the guest
            // zeroed this chunk" — install a zero page regardless of
            // the base. Serving canonical content here when the base
            // is non-zero would silently un-zero guest RAM on resume.
            (_, None) => ResolvedPage::Zero {
                offset: chunk_offset,
            },
        })
    }

    /// ADR 0014 M1.14: return the canonical chunk hash at a given
    /// chunk-aligned byte offset. The working-set recorder uses this
    /// to observe canonical-resolved faults too — without it, the
    /// bake-time profile pass produces an empty trace because a
    /// freshly-canonical snapshot has zero divergence chunks.
    pub fn canonical_chunk_hash(&self, byte_offset: u64) -> Option<ChunkHash> {
        if byte_offset >= self.canonical.total_bytes {
            return None;
        }
        let chunk_idx = (byte_offset / self.canonical.chunk_size) as usize;
        self.canonical.chunks.get(chunk_idx).copied().flatten()
    }

    /// Bytes for a session-divergent chunk. Routes through the
    /// `ChunkCache`'s singleflight + local-NVMe layer so multiple
    /// faults on the same chunk in flight share one fetch.
    pub async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes, ChunkedBackendError> {
        self.cache
            .get(hash, || self.store.get_chunk(hash))
            .await
            .map_err(Into::into)
    }

    /// Chunk-start byte offsets where the session manifest holds
    /// `hash`. Used by the working-set replay path: a trace entry
    /// names a chunk hash, the runtime asks "where does the session
    /// place this chunk?" and pre-installs it at each position.
    ///
    /// Linear scan over `chunks` because the chunked manifest is
    /// typically small (≤8192 entries for a 4 GiB / 512 KiB layout)
    /// and the prefault path runs once per restore, before vCPUs
    /// unfreeze — not a hot loop.
    pub fn session_positions_of(&self, hash: ChunkHash) -> Vec<u64> {
        self.session
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| match entry {
                Some(h) if *h == hash => Some((idx as u64) * self.session.chunk_size),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::manifest::{ChunkRef, MANIFEST_SCHEMA_VERSION};
    use engram_chunk_store::{cache::ChunkCacheConfig, ChunkSize, ChunkStore};
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    fn make_cache_and_store() -> (ChunkCache, ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let cs = ChunkStore::new(blob);
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        (ChunkCache::new(cfg), cs, dir)
    }

    /// Synthesize a distinct ChunkHash per byte tag. The actual
    /// hash values don't matter — only equality across canonical
    /// vs session does — but using `ChunkHash::of(&[byte])` gives
    /// us a stable, real sha256 derivation so the test stays
    /// representative of production hash semantics.
    fn h(byte: u8) -> ChunkHash {
        ChunkHash::of(&[byte])
    }

    #[test]
    fn resolves_canonical_match_to_canonical_offset() {
        // Both canonical and session agree on chunk 0. The
        // resolver returns `Canonical { canonical_offset: 0 }`
        // — runtime fetches the canonical chunk hash at that offset.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
        assert!(matches!(
            b.resolve(0),
            Some(ResolvedPage::Canonical {
                canonical_offset: 0
            })
        ));
        assert!(matches!(
            b.resolve(512),
            Some(ResolvedPage::Canonical {
                canonical_offset: 512
            })
        ));
    }

    #[test]
    fn resolves_session_divergence_to_chunk_fetch() {
        // Session's chunk 1 differs from canonical's chunk 1 →
        // runtime fetches via the cache.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(7))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
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
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
        let r = b.resolve(600).unwrap();
        match r {
            ResolvedPage::Canonical { canonical_offset } => assert_eq!(canonical_offset, 512),
            other => panic!("expected Canonical(512), got {other:?}"),
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
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
        assert!(matches!(
            b.resolve(512),
            Some(ResolvedPage::Zero { offset: 512 })
        ));
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
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
        match b.resolve(512) {
            Some(ResolvedPage::Zero { offset }) => assert_eq!(offset, 512),
            other => {
                panic!("expected Zero(512) for a session-zeroed base-nonzero chunk, got {other:?}")
            }
        }
    }

    #[test]
    fn resolves_past_eof_returns_none() {
        let canonical = synth_manifest(1024, 512, vec![]);
        let session = synth_manifest(1024, 512, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, store).unwrap();
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
        match ChunkedMemoryBackend::new(&canonical, &session, cache, store) {
            Ok(_) => panic!("chunk_size mismatch must reject"),
            Err(e) => assert!(format!("{e}").contains("chunk_size")),
        }
    }

    #[test]
    fn rejects_total_bytes_mismatch() {
        let canonical = synth_manifest(2048, 512, vec![]);
        let session = synth_manifest(1024, 512, vec![]);
        let (cache, store, _dir) = make_cache_and_store();
        match ChunkedMemoryBackend::new(&canonical, &session, cache, store) {
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
        match ChunkedMemoryBackend::new(&disk, &session, cache, store) {
            Ok(_) => panic!("non-memory manifest must reject"),
            Err(e) => assert!(format!("{e}").contains("Memory")),
        }
    }
}
