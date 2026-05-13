//! ADR 0008 Phase 5 integration test: chunked-OCI fault path.
//!
//! End-to-end exercise of the tiered chunk-resolution path:
//! `local NVMe → BlobStorage → OCI registry`. The test spins up a
//! small loopback HTTP server that speaks just enough of the OCI
//! Distribution Spec (`GET /v2/`, `GET /v2/<repo>/blobs/<digest>`
//! with `Range` support) for `OciChunkResolver` to fault chunks
//! against, then:
//!
//! 1. **Cold path** — `materialize_to_file` on a manifest whose
//!    chunks are *not* in BlobStorage. The tiered resolver falls
//!    through to the fake registry, Range-GETs each chunk's bytes,
//!    verifies the per-chunk sha256, and tees back into
//!    BlobStorage (CDN-fill).
//! 2. **Warm path** — the same materialize re-issued after the cold
//!    path completes. BlobStorage now holds the chunks; the cache
//!    tier hits, the origin tier is not consulted.
//! 3. **`PooledBackend::with_oci_client` dispatch** — exercises the
//!    upgrade path that constructs the tiered resolver when a
//!    `CachedImage::is_disk_chunked_oci()` artifact is presented.
//!
//! Runs in CI with zero external dependencies — the fake registry
//! is a 60-line `axum` router bound to `127.0.0.1:0`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use engram_chunk_store::{
    BlobStorageResolver, Bootstrap, BootstrapEntry, ChunkHash, ChunkResolver, ChunkSize,
    ChunkStore, Manifest, ManifestKind, ManifestRef, TieredChunkResolver, BOOTSTRAP_SCHEMA_VERSION,
};
use engram_core::traits::BlobStorage;
use engram_oci::{AnonymousResolver, OciBlobLocator, OciChunkIndex, OciChunkResolver, OciClient};
use engram_storage_local::LocalBlobStorage;
use tokio::sync::oneshot;

// ---------------------------------------------------------------
// Fake OCI registry. Implements the smallest subset of the OCI
// Distribution Spec we need to exercise `pull_blob_stream_partial`:
//
//   GET /v2/                              -> 200 OK    (anonymous probe)
//   GET /v2/<repo>/blobs/<digest>         -> 200 / 206 (with Range support)
//
// Everything else 404s. Blobs are seeded in memory keyed by digest
// at startup. Range requests are honored; full-body GETs are
// supported as a fallback.
// ---------------------------------------------------------------

#[derive(Clone)]
struct FakeRegistry {
    /// digest (`sha256:<hex>`) → bytes. Cheap-clone via Arc<Bytes>.
    blobs: Arc<HashMap<String, Bytes>>,
    /// Per-blob fetch counter — lets the test assert that the
    /// origin tier was (or wasn't) consulted.
    fetches: Arc<AtomicUsize>,
}

async fn handle_v2_root() -> StatusCode {
    // The well-known endpoint that signals OCI v2 + anonymous-OK.
    // `oci-client` probes this during auth setup.
    StatusCode::OK
}

async fn handle_blob(
    State(state): State<FakeRegistry>,
    Path((_repo, digest)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    state.fetches.fetch_add(1, Ordering::Relaxed);
    let Some(body) = state.blobs.get(&digest) else {
        return (StatusCode::NOT_FOUND, format!("blob not found: {digest}")).into_response();
    };
    let total = body.len() as u64;

    // Range parsing — `bytes=<start>-<end>` inclusive.
    if let Some(range) = headers.get(http::header::RANGE) {
        let range = range.to_str().unwrap_or("");
        if let Some(spec) = range.strip_prefix("bytes=") {
            if let Some((start_s, end_s)) = spec.split_once('-') {
                let start: u64 = start_s.parse().unwrap_or(0);
                let end: u64 = if end_s.is_empty() {
                    total.saturating_sub(1)
                } else {
                    end_s.parse().unwrap_or(total.saturating_sub(1))
                };
                if start > end || start >= total {
                    return (StatusCode::RANGE_NOT_SATISFIABLE, "bad range").into_response();
                }
                let end = end.min(total - 1);
                let slice = body.slice(start as usize..=end as usize);
                let mut resp = Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(http::header::CONTENT_LENGTH, slice.len())
                    .header(
                        http::header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    )
                    .header(http::header::CONTENT_TYPE, "application/octet-stream")
                    .body(axum::body::Body::from(slice))
                    .unwrap();
                resp.headers_mut().insert(
                    http::header::ACCEPT_RANGES,
                    http::HeaderValue::from_static("bytes"),
                );
                return resp;
            }
        }
    }

    // No Range → full body.
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_LENGTH, body.len())
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from(body.clone()))
        .unwrap()
}

/// Spawn the fake registry on a loopback ephemeral port. Returns
/// the bound address (so the test can construct a URI pointing at
/// it) and a shutdown handle.
async fn spawn_fake_registry(
    blobs: HashMap<String, Bytes>,
) -> (SocketAddr, FakeRegistry, oneshot::Sender<()>) {
    let state = FakeRegistry {
        blobs: Arc::new(blobs),
        fetches: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/v2/", get(handle_v2_root))
        .route("/v2/:repo/blobs/:digest", get(handle_blob))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (addr, state, shutdown_tx)
}

// ---------------------------------------------------------------
// Test helpers — chunk layout, manifest fixture, ChunkStore setup.
// ---------------------------------------------------------------

/// Three chunks with deterministic content. Returned along with the
/// raw chunk-blob bytes (concatenation) and the parsed bootstrap.
fn fixture_chunks() -> (Vec<&'static [u8]>, Vec<u8>, Bootstrap) {
    let chunks: Vec<&'static [u8]> = vec![
        b"AAAAAAAA" as &[u8], // 8 bytes
        b"BBBBBBBBBBBBBBBB",  // 16 bytes
        b"CCCC",              // 4 bytes
    ];
    let mut concat = Vec::new();
    let mut entries = Vec::with_capacity(chunks.len());
    let mut blob_offset = 0u64;
    let mut file_offset = 0u64;
    let chunk_size = 16u64;
    for c in &chunks {
        let h = ChunkHash::of(c);
        entries.push(BootstrapEntry {
            file_offset,
            blob_digest: None,
            blob_offset,
            length: c.len() as u32,
            sha256: h,
        });
        concat.extend_from_slice(c);
        blob_offset += c.len() as u64;
        file_offset += chunk_size;
    }
    let total_bytes = file_offset;
    let bootstrap = Bootstrap {
        schema_version: BOOTSTRAP_SCHEMA_VERSION,
        kind: ManifestKind::Disk,
        total_bytes,
        chunk_size: ChunkSize::bytes(chunk_size),
        entries,
    };
    (chunks, concat, bootstrap)
}

/// Build an empty BlobStorage in a tempdir.
fn fresh_blob_storage() -> (Arc<dyn BlobStorage>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
    (blob, dir)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

/// Cold + warm round-trip on a fresh BlobStorage. The first
/// `materialize_to_file` faults all chunks from the fake registry
/// and tees them back into BlobStorage; the second hits the cache
/// tier exclusively (no additional origin fetches).
#[tokio::test]
async fn tiered_resolver_faults_chunks_from_oci_then_serves_from_blob_cache() {
    let (raw_chunks, concat, bootstrap) = fixture_chunks();
    let blob_digest = format!(
        "sha256:{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&concat)
    );

    let mut registry_blobs = HashMap::new();
    registry_blobs.insert(blob_digest.clone(), Bytes::from(concat.clone()));

    let (addr, registry, _shutdown) = spawn_fake_registry(registry_blobs).await;
    let image_uri = format!("127.0.0.1:{}/chunked:v1", addr.port());

    let (blob, _blob_dir) = fresh_blob_storage();

    // Build the tiered resolver: BlobStorageResolver (cache) →
    // OciChunkResolver (origin) → write-back to BlobStorage.
    let mut index = OciChunkIndex::new();
    for entry in &bootstrap.entries {
        index.insert(
            entry.sha256,
            OciBlobLocator {
                blob_digest: blob_digest.clone(),
                offset: entry.blob_offset,
                length: entry.length as u64,
            },
        );
    }
    let oci_client = OciClient::new(Arc::new(AnonymousResolver));
    let oci_resolver: Arc<dyn ChunkResolver> = Arc::new(OciChunkResolver::new(
        oci_client.clone(),
        image_uri.clone(),
        index,
    ));
    let cache_resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
    let tiered = Arc::new(TieredChunkResolver::new(
        vec![cache_resolver, oci_resolver],
        Some(blob.clone()),
    ));

    // Construct a ChunkStore wired to BlobStorage and the tiered
    // resolver. Pre-stash the Manifest so chunk_store.materialize
    // can find it. Chunks themselves are *not* in BlobStorage at
    // this point — that's the cold-path invariant.
    let store = ChunkStore::new(blob.clone()).with_resolver(tiered);
    let manifest = Manifest {
        schema_version: 1,
        kind: ManifestKind::Disk,
        total_bytes: bootstrap.total_bytes,
        chunk_size: bootstrap.chunk_size,
        chunks: bootstrap
            .entries
            .iter()
            .map(|e| engram_chunk_store::ChunkRef {
                offset: e.file_offset,
                hash: e.sha256,
            })
            .collect(),
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    let manifest_ref = ManifestRef::new();
    store.put_manifest(manifest_ref, &manifest).await.unwrap();

    // None of the chunks are in BlobStorage yet.
    for entry in &bootstrap.entries {
        assert!(
            !blob.exists(&entry.sha256.storage_key()).await.unwrap(),
            "pre: chunk {} must not be in BlobStorage",
            entry.sha256
        );
    }

    // Cold materialize. Drives the OCI fault path + write-back.
    let dest1 = tempfile::NamedTempFile::new().unwrap();
    store
        .materialize_to_file(&manifest, dest1.path())
        .await
        .expect("cold materialize via OCI fault path");
    let cold_bytes = std::fs::read(dest1.path()).unwrap();

    // The materialized file reproduces the source file (chunks
    // placed at their file_offset, with zero padding in between
    // for the gaps between chunks).
    let mut expected = vec![0u8; bootstrap.total_bytes as usize];
    for (i, c) in raw_chunks.iter().enumerate() {
        let start = bootstrap.entries[i].file_offset as usize;
        expected[start..start + c.len()].copy_from_slice(c);
    }
    assert_eq!(cold_bytes, expected, "cold materialize bytes mismatch");

    // Registry was consulted for each chunk (3 chunks → 3 Range GETs).
    // Note: oci-client also probes `/v2/` for auth before each
    // pull_blob_stream_partial; that's a separate handler so the
    // blob-fetch count is independent.
    let cold_fetches = registry.fetches.load(Ordering::Relaxed);
    assert_eq!(
        cold_fetches,
        raw_chunks.len(),
        "cold path: registry should have served exactly one blob fetch per chunk"
    );

    // After cold-materialize, all chunks are present in BlobStorage
    // — CDN-fill landed. Each chunk's storage key equals its
    // content's sha256 hex.
    for entry in &bootstrap.entries {
        assert!(
            blob.exists(&entry.sha256.storage_key()).await.unwrap(),
            "post-cold: chunk {} must be tee-filled into BlobStorage",
            entry.sha256
        );
        let cached_bytes = blob.get(&entry.sha256.storage_key()).await.unwrap();
        assert_eq!(
            ChunkHash::of(&cached_bytes),
            entry.sha256,
            "tee-fill must store bytes that hash to the requested key"
        );
    }

    // Warm materialize. Cache tier must serve every chunk; origin
    // tier must not be consulted.
    let dest2 = tempfile::NamedTempFile::new().unwrap();
    store
        .materialize_to_file(&manifest, dest2.path())
        .await
        .expect("warm materialize via BlobStorage cache");
    let warm_bytes = std::fs::read(dest2.path()).unwrap();
    assert_eq!(warm_bytes, cold_bytes, "warm path bytes must match cold");

    let warm_fetches = registry.fetches.load(Ordering::Relaxed);
    assert_eq!(
        warm_fetches, cold_fetches,
        "warm path must not consult the OCI origin tier — cache must serve every chunk"
    );
}

/// Negative: if a chunk is absent from BOTH tiers, materialize
/// surfaces a real error (not a silent zero-fill). Verifies the
/// `last_err` propagation in `TieredChunkResolver::fetch_chunk`.
#[tokio::test]
async fn tiered_resolver_fails_loudly_when_chunk_missing_from_all_tiers() {
    let (_addr, _registry, _shutdown) = spawn_fake_registry(HashMap::new()).await;
    let (blob, _blob_dir) = fresh_blob_storage();

    // Resolver chain with an empty registry → every OCI fetch 404s.
    let unknown_hash = ChunkHash::of(b"never-seen");
    let mut index = OciChunkIndex::new();
    // No entry for unknown_hash → OciChunkResolver returns Origin
    // error immediately, before any network call. Forces the
    // "missing-from-all-tiers" branch in TieredChunkResolver.
    index.insert(
        ChunkHash::of(b"placeholder"),
        OciBlobLocator {
            blob_digest: "sha256:none".into(),
            offset: 0,
            length: 0,
        },
    );

    let oci_client = OciClient::new(Arc::new(AnonymousResolver));
    let oci_resolver: Arc<dyn ChunkResolver> = Arc::new(OciChunkResolver::new(
        oci_client,
        "127.0.0.1:0/x:y".into(),
        index,
    ));
    let cache_resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
    let tiered = TieredChunkResolver::new(vec![cache_resolver, oci_resolver], Some(blob.clone()));

    let result = tiered.fetch_chunk(unknown_hash).await;
    assert!(
        result.is_err(),
        "missing chunk must fail materialize, not return zeros"
    );
}

/// Reproduces the *second* layer of the cross-namespace bug:
/// `ChunkCache::get` routes misses through its **own** internal
/// ChunkStore reference (the one pinned at cache construction
/// time), not through whatever store the caller passes to
/// `materialize_to_file_cached`. When the global cache was wired
/// at startup with a non-tiered store, the upgraded-tiered store
/// passed to materialize gets silently bypassed and chunks
/// resolve via BlobStorage only → "blob not found" on empty
/// BlobStorage namespaces. `ChunkCache::with_store` is the fix:
/// rebind the cache to the tiered store before materialize.
#[tokio::test]
async fn chunk_cache_rebound_to_tiered_store_faults_through_oci() {
    use engram_chunk_store::{
        cache::ChunkCacheConfig, ChunkCache, ChunkRef as CsChunkRef, ChunkSize, Manifest,
        ManifestKind, ManifestRef,
    };

    // Real registry serving a chunk blob; empty BlobStorage.
    let bodies: [&[u8]; 2] = [b"DELTA___", b"EPSILON_"];
    let mut concat = Vec::new();
    let mut hashes = Vec::new();
    for b in &bodies {
        concat.extend_from_slice(b);
        hashes.push(ChunkHash::of(b));
    }
    let blob_digest = format!(
        "sha256:{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&concat)
    );
    let mut blobs = HashMap::new();
    blobs.insert(blob_digest.clone(), Bytes::from(concat));
    let (addr, registry, _shutdown) = spawn_fake_registry(blobs).await;
    let image_uri = format!("127.0.0.1:{}/cached:v1", addr.port());

    let (blob, _blob_dir) = fresh_blob_storage();
    let base_store = ChunkStore::new(blob.clone());

    // Pre-stash the manifest in the runtime blob (simulates what
    // ensure_chunked_manifest_in_blob_storage does upstream).
    let manifest_ref = ManifestRef::new();
    let manifest = Manifest {
        schema_version: 1,
        kind: ManifestKind::Disk,
        total_bytes: 16,
        chunk_size: ChunkSize::bytes(8),
        chunks: vec![
            CsChunkRef {
                offset: 0,
                hash: hashes[0],
            },
            CsChunkRef {
                offset: 8,
                hash: hashes[1],
            },
        ],
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    base_store
        .put_manifest(manifest_ref, &manifest)
        .await
        .unwrap();

    // Build the tiered store (what upgrade_chunk_store_for_chunked_oci
    // produces in production).
    let mut index = OciChunkIndex::new();
    for (i, h) in hashes.iter().enumerate() {
        index.insert(
            *h,
            OciBlobLocator {
                blob_digest: blob_digest.clone(),
                offset: (i * 8) as u64,
                length: 8,
            },
        );
    }
    let oci_client = OciClient::new(Arc::new(AnonymousResolver));
    let oci_resolver: Arc<dyn ChunkResolver> =
        Arc::new(OciChunkResolver::new(oci_client, image_uri.clone(), index));
    let cache_resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
    let tiered: Arc<dyn ChunkResolver> = Arc::new(TieredChunkResolver::new(
        vec![cache_resolver, oci_resolver],
        Some(blob.clone()),
    ));
    let tiered_store = base_store.with_resolver(tiered);

    // The bug shape: a `ChunkCache` constructed at startup against
    // the un-upgraded `base_store`. Without `with_store`, its
    // miss-path would 404.
    let cache_dir = tempfile::tempdir().unwrap();
    let stale_cache = ChunkCache::new(
        ChunkCacheConfig {
            root: cache_dir.path().to_path_buf(),
            budget_bytes: 100 * 1024 * 1024,
        },
        ChunkStore::new(blob.clone()), // un-upgraded
    );

    // The fix: rebind the cache to the tiered store.
    let rebound_cache = stale_cache.with_store(tiered_store.clone());

    // materialize_to_file_cached now routes chunk misses through
    // tiered_store → BlobStorage (miss) → OCI (hit) → tee-fill.
    let dest = tempfile::NamedTempFile::new().unwrap();
    tiered_store
        .materialize_to_file_cached(&manifest, dest.path(), &rebound_cache)
        .await
        .expect("materialize through rebound cache + tiered store");

    // Bytes are correct (rebuilt from the bootstrap entries).
    let got = std::fs::read(dest.path()).unwrap();
    let mut expected = vec![0u8; 16];
    expected[0..8].copy_from_slice(b"DELTA___");
    expected[8..16].copy_from_slice(b"EPSILON_");
    assert_eq!(got, expected);

    // OCI was consulted (2 chunks, 2 fetches).
    assert_eq!(
        registry.fetches.load(Ordering::Relaxed),
        2,
        "rebound cache must drive misses through tiered → OCI"
    );

    // BlobStorage was tee-filled by the tiered resolver.
    for h in &hashes {
        assert!(
            blob.exists(&h.storage_key()).await.unwrap(),
            "chunk should be tee-filled into BlobStorage on tiered hit"
        );
    }
}

/// Reproduces the cross-namespace bricked-image bug:
/// `materialize_chunked_rootfs` calls `chunk_store.get_manifest`
/// which reads straight from BlobStorage, not through the
/// resolver. If the `Manifest` was written to the bake's
/// BlobStorage but the runtime host's BlobStorage is empty (the
/// scenario ADR 0008 was meant to fix), get_manifest 404s and
/// session create fails silently.
///
/// The Phase 5 final synthesis-on-pull fix:
/// `ensure_chunked_manifest_in_blob_storage` reads the bootstrap
/// sidecar and `put_manifest`s a synthesized Manifest into
/// BlobStorage before any downstream consumer needs it. After
/// that, materialize works against an empty BlobStorage.
#[tokio::test]
async fn manifest_synthesis_from_bootstrap_unblocks_materialize_when_blob_empty() {
    use engram_chunk_store::{
        Bootstrap, BootstrapEntry, ChunkRef as CsChunkRef, ChunkSize, Manifest, ManifestKind,
        ManifestRef, BOOTSTRAP_SCHEMA_VERSION,
    };

    // Stage: a chunked-OCI image whose Manifest was committed to
    // BlobStorage namespace A (the bake's), but the runtime host
    // points at empty BlobStorage namespace B.
    let bake_dir = tempfile::tempdir().unwrap();
    let bake_blob: Arc<dyn BlobStorage> =
        Arc::new(LocalBlobStorage::new(bake_dir.path().to_path_buf()));
    let bake_store = ChunkStore::new(bake_blob.clone());

    // Bake writes three chunks + a manifest to its blob.
    let bodies: [&[u8]; 3] = [b"alpha___", b"beta____", b"gamma___"];
    let mut chunks = Vec::with_capacity(3);
    for (i, b) in bodies.iter().enumerate() {
        let h = bake_store.put_chunk(b).await.unwrap();
        chunks.push(CsChunkRef {
            offset: (i * 8) as u64,
            hash: h,
        });
    }
    let manifest_ref = ManifestRef::new();
    let bake_manifest = Manifest {
        schema_version: 1,
        kind: ManifestKind::Disk,
        total_bytes: 24,
        chunk_size: ChunkSize::bytes(8),
        chunks: chunks.clone(),
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    bake_store
        .put_manifest(manifest_ref, &bake_manifest)
        .await
        .unwrap();

    // Bake also writes a bootstrap sidecar (what `pull_image`
    // would land on the runtime host).
    let bootstrap = Bootstrap {
        schema_version: BOOTSTRAP_SCHEMA_VERSION,
        kind: ManifestKind::Disk,
        total_bytes: 24,
        chunk_size: ChunkSize::bytes(8),
        entries: chunks
            .iter()
            .enumerate()
            .map(|(i, c)| BootstrapEntry {
                file_offset: c.offset,
                blob_digest: None,
                blob_offset: (i * 8) as u64,
                length: 8,
                sha256: c.hash,
            })
            .collect(),
    };

    let cache_root = tempfile::tempdir().unwrap();
    let bs_path = cache_root.path().join("bootstrap.disk.json");
    std::fs::write(&bs_path, serde_json::to_vec(&bootstrap).unwrap()).unwrap();

    // Runtime host's BlobStorage is empty — the bug scenario.
    let runtime_dir = tempfile::tempdir().unwrap();
    let runtime_blob: Arc<dyn BlobStorage> =
        Arc::new(LocalBlobStorage::new(runtime_dir.path().to_path_buf()));
    let runtime_store = ChunkStore::new(runtime_blob.clone());

    // Pre-condition: manifest is NOT in the runtime blob.
    assert!(matches!(
        runtime_store.get_manifest(manifest_ref).await,
        Err(engram_chunk_store::ChunkStoreError::Blob(
            engram_core::error::BlobError::NotFound
        ))
    ));

    // Build a `CachedImage` skeleton that points at the bootstrap
    // sidecar we just wrote. This mirrors what
    // `image_cache::ensure_image` produces after a chunked-OCI pull.
    let cached = engram_host_agent::image_cache::CachedImage {
        manifest_path: cache_root.path().join("manifest.toml"),
        rootfs_path: None,
        bundle: Some(engram_host_agent::image_cache::ImageBundle {
            schema_version: 2,
            disk_manifest: manifest_ref,
            canonical_memory_manifest: None,
            bootstrap_disk_available: true,
            bootstrap_memory_available: false,
        }),
        disk_bootstrap_path: Some(bs_path),
        disk_chunks_blob_digest: Some("sha256:notused_in_this_test".into()),
        memory_bootstrap_path: None,
        memory_chunks_blob_digest: None,
        digest: "sha256:bug_repro".into(),
    };

    // Apply the Phase 5 final fix: synthesize + persist.
    engram_host_agent::pooled_backend::ensure_chunked_manifest_in_blob_storage(
        &runtime_store,
        &cached,
    )
    .await
    .expect("synthesis should succeed");

    // Post-condition: manifest IS now in the runtime blob, and its
    // chunks list matches the bake's exactly (deterministic
    // synthesis from the bootstrap).
    let restored = runtime_store
        .get_manifest(manifest_ref)
        .await
        .expect("manifest should be persisted after synthesis");
    assert_eq!(restored.kind, bake_manifest.kind);
    assert_eq!(restored.chunk_size, bake_manifest.chunk_size);
    assert_eq!(restored.total_bytes, bake_manifest.total_bytes);
    assert_eq!(restored.chunks.len(), bake_manifest.chunks.len());
    for (a, b) in restored.chunks.iter().zip(bake_manifest.chunks.iter()) {
        assert_eq!(a.offset, b.offset);
        assert_eq!(a.hash, b.hash);
    }

    // Idempotent re-run is a no-op success (no VersionConflict
    // panic / propagation).
    engram_host_agent::pooled_backend::ensure_chunked_manifest_in_blob_storage(
        &runtime_store,
        &cached,
    )
    .await
    .expect("second call should be idempotent");
}

/// `PooledBackend`-level dispatch: when an image is chunked-OCI
/// shaped AND an OciClient is configured, the upgrade path swaps
/// in the tiered resolver. Exercises the `is_disk_chunked_oci` +
/// `build_oci_chunk_index` + tiered-resolver chain via the public
/// `CachedImage` surface; the actual upgrade method
/// (`PooledBackend::upgrade_chunk_store_for_chunked_oci`) is
/// crate-private but the same code path runs on every call to
/// `resolve_rootfs`.
#[tokio::test]
async fn cached_image_chunked_oci_drives_tiered_materialize_end_to_end() {
    // Same setup as the first test, but instead of constructing
    // the resolver chain by hand, we go through CachedImage's
    // Phase 5 helpers. This proves the discovery path
    // (bootstrap.disk.json + chunks.disk.blob.digest sidecars →
    // OciChunkIndex) is consistent with what hand-built setups
    // exercise.
    use engram_host_agent::image_cache::CachedImage;

    let (raw_chunks, concat, bootstrap) = fixture_chunks();
    let blob_digest = format!(
        "sha256:{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&concat)
    );

    let mut registry_blobs = HashMap::new();
    registry_blobs.insert(blob_digest.clone(), Bytes::from(concat));
    let (addr, registry, _shutdown) = spawn_fake_registry(registry_blobs).await;
    let image_uri = format!("127.0.0.1:{}/chunked:v1", addr.port());

    // Write the bootstrap file out to disk — CachedImage's
    // build_oci_chunk_index reads it from the cache directory.
    let cache_root = tempfile::tempdir().unwrap();
    let bs_path = cache_root.path().join("bootstrap.disk.json");
    std::fs::write(&bs_path, serde_json::to_vec(&bootstrap).unwrap()).unwrap();

    let cached = CachedImage {
        manifest_path: cache_root.path().join("manifest.toml"),
        rootfs_path: None,
        bundle: None,
        disk_bootstrap_path: Some(bs_path),
        disk_chunks_blob_digest: Some(blob_digest),
        memory_bootstrap_path: None,
        memory_chunks_blob_digest: None,
        digest: "sha256:test_cached".into(),
    };
    assert!(cached.is_disk_chunked_oci());

    let index = cached
        .build_oci_chunk_index()
        .await
        .unwrap()
        .expect("chunked image → index");
    assert_eq!(index.len(), raw_chunks.len());

    // Build the same tiered chain the upgrade path constructs.
    let (blob, _blob_dir) = fresh_blob_storage();
    let oci_client = OciClient::new(Arc::new(AnonymousResolver));
    let oci_resolver: Arc<dyn ChunkResolver> =
        Arc::new(OciChunkResolver::new(oci_client, image_uri.clone(), index));
    let cache_resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
    let tiered = Arc::new(TieredChunkResolver::new(
        vec![cache_resolver, oci_resolver],
        Some(blob.clone()),
    ));

    let store = ChunkStore::new(blob.clone()).with_resolver(tiered);
    let manifest = Manifest {
        schema_version: 1,
        kind: ManifestKind::Disk,
        total_bytes: bootstrap.total_bytes,
        chunk_size: bootstrap.chunk_size,
        chunks: bootstrap
            .entries
            .iter()
            .map(|e| engram_chunk_store::ChunkRef {
                offset: e.file_offset,
                hash: e.sha256,
            })
            .collect(),
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    let manifest_ref = ManifestRef::new();
    store.put_manifest(manifest_ref, &manifest).await.unwrap();

    let dest = tempfile::NamedTempFile::new().unwrap();
    store
        .materialize_to_file(&manifest, dest.path())
        .await
        .expect("materialize via tiered store");

    // The OCI registry was consulted; CDN-fill ran; chunks are now
    // cached. Re-materialize and confirm zero origin fetches.
    let after_cold = registry.fetches.load(Ordering::Relaxed);
    assert_eq!(after_cold, raw_chunks.len());

    let dest2 = tempfile::NamedTempFile::new().unwrap();
    store
        .materialize_to_file(&manifest, dest2.path())
        .await
        .expect("warm re-materialize");
    let after_warm = registry.fetches.load(Ordering::Relaxed);
    assert_eq!(
        after_warm, after_cold,
        "warm re-materialize must not consult OCI"
    );
}
