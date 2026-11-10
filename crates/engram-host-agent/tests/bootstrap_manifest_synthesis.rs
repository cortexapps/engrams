//! A chunked image whose disk manifest is in the bake's object-store
//! namespace, not the runtime host's: the host synthesizes the manifest
//! from the bootstrap sidecar that came with the image pull.

use std::sync::Arc;

use engram_chunk_store::ChunkStore;
use engram_core::traits::BlobStorage;
use engram_storage_local::LocalBlobStorage;

/// A chunked rootfs is materialized from the manifest that
/// `chunk_store.get_manifest` reads from the object store. When the
/// runtime host's object store does not hold the manifest,
/// `ensure_chunked_manifest_in_blob_storage` reads the bootstrap sidecar
/// and `put_manifest`s a synthesized manifest first, so the read succeeds.
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
            .map(|c| BootstrapEntry {
                file_offset: c.offset,
                blob_digest: Some(format!("sha256:{}", c.hash.to_hex())),
                blob_offset: 0,
                length: 8,
                sha256: c.hash,
            })
            .collect(),
    };

    let cache_root = tempfile::tempdir().unwrap();
    let bs_path = cache_root.path().join("bootstrap.disk.json");
    std::fs::write(&bs_path, serde_json::to_vec(&bootstrap).unwrap()).unwrap();

    // The runtime host's BlobStorage is empty.
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
            bootstrap_disk_available: true,
        }),
        disk_bootstrap_path: Some(bs_path),
        digest: "sha256:synthesis".into(),
    };

    // Synthesize and persist.
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
