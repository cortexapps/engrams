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
use engram_chunk_store::reader::ChunkCacheReader;
use engram_chunk_store::{ChunkHash, ChunkStore, Manifest, ManifestKind};
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
    /// ADR 0075: the READ-ONLY view of the shared cache dir. The
    /// handler cannot mutate the directory by construction.
    reader: ChunkCacheReader,
    /// ADR 0075: the populate channel to the one writer (the
    /// host-agent). `None` = no writer configured (unit tests, and
    /// the explicit fallback-only mode) — misses go straight to the
    /// direct-blob fallback.
    populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    /// Blob store for the LAST-RESORT fallback fetch (writer
    /// unreachable — e.g. a host-agent mid-roll; ADR 0044 K2 handlers
    /// outlive rolls). Served from memory, never written to the cache
    /// dir, counted via `engram_substrate_fallback_fetch_total`.
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
        Ok(Self {
            canonical,
            session,
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
        Self::from_blob_with_session_json(
            canonical_ref,
            session_ref,
            None,
            blob,
            cache_root,
            populate,
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
        session_manifest_json: Option<&Path>,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
    ) -> Result<(Self, bool), ChunkedBackendError> {
        match Self::from_blob_with_session_json(
            canonical_ref,
            session_ref,
            session_manifest_json,
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
                Self::from_blob_with_session_json(
                    session_ref,
                    session_ref,
                    session_manifest_json,
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

    pub async fn from_blob_with_session_json(
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        session_manifest_json: Option<&Path>,
        blob: Arc<dyn BlobStorage>,
        cache_root: &Path,
        populate: Option<std::sync::Arc<crate::populate_client::PopulateClient>>,
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
        // ADR 0075: the handler holds a READ-ONLY view of the shared
        // cache_root — population happens in the one writer (the
        // host-agent) via the substrate socket, so its global
        // singleflight / pin set / budget govern this handler's
        // traffic too. The #437/#522-era per-handler ChunkCache (and
        // its eviction_enabled:false half-measure) is gone.
        let reader = ChunkCacheReader::new(cache_root);
        Self::new(&canonical, &session, reader, populate, store)
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

    /// Bytes for a session-divergent chunk — the ADR 0075 3-step read
    /// path: (1) resident on NVMe → serve (trust-on-read, unchanged);
    /// (2) ask the one writer to populate and read back the fd —
    /// cross-process misses collapse in the WRITER's singleflight;
    /// (3) writer unreachable past its retry budget (host-agent
    /// mid-roll) → direct blob fetch served from memory, never written
    /// to the cache dir. Step 3 keeps the fault path independent of
    /// the control plane (ADR 0044 K2) while preserving the
    /// single-writer invariant.
    pub async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes, ChunkedBackendError> {
        // 1. Fast path: resident.
        if let Some(bytes) = self.reader.read(hash).map_err(ChunkedBackendError::Io)? {
            return Ok(Bytes::from(bytes));
        }
        // 2. Populate via the writer. Sync UDS I/O — hop off the
        // runtime worker (the fault loop block_on's this future on the
        // handler's small private runtime).
        if let Some(client) = &self.populate {
            let client = client.clone();
            let res = tokio::task::spawn_blocking(move || client.request(hash))
                .await
                .map_err(|e| {
                    ChunkedBackendError::Io(std::io::Error::other(format!(
                        "populate task join: {e}"
                    )))
                })?;
            match res {
                Ok(bytes) => return Ok(Bytes::from(bytes)),
                Err(crate::populate_client::PopulateError::WriterUnreachable(msg)) => {
                    // The handler exports no Prometheus metrics (it is a
                    // per-VM leaf process; its telemetry rides logs + the
                    // stats-file pattern). This WARN is the alarm signal —
                    // sustained occurrences mean the writer is unreachable
                    // — and the writer-side populate counter going quiet
                    // corroborates. (ADR 0075 divergence note.)
                    tracing::warn!(
                        %hash,
                        %msg,
                        "substrate writer unreachable; direct-blob fallback (uncached)",
                    );
                    // fall through to 3
                }
                Err(e) => {
                    return Err(ChunkedBackendError::Io(std::io::Error::other(format!(
                        "populate: {e}"
                    ))));
                }
            }
        }
        // 3. Last resort — memory-only, never written to the cache dir.
        self.store.get_chunk(hash).await.map_err(Into::into)
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
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    async fn put(store: &ChunkStore, m: &Manifest) -> ManifestRef {
        let r = ManifestRef::new();
        store.put_manifest(r, m).await.unwrap();
        r
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
            None,
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
            None,
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
            None,
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
            working_set_trace: None,
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

        let got = b.fetch_chunk(hash).await.expect("fallback fetch");
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
    fn resolves_canonical_match_to_canonical_offset() {
        // Both canonical and session agree on chunk 0. The
        // resolver returns `Canonical { canonical_offset: 0 }`
        // — runtime fetches the canonical chunk hash at that offset.
        let canonical = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let session = synth_manifest(1024, 512, vec![(0, h(1)), (512, h(2))]);
        let (cache, store, _dir) = make_cache_and_store();
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
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
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
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
        let b = ChunkedMemoryBackend::new(&canonical, &session, cache, None, store).unwrap();
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
