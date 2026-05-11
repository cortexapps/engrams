//! End-to-end against a real `BlobStorage` impl (GCS via
//! fake-gcs-server). Validates that the chunk store's typed API
//! works correctly against a backend with wire-level pagination,
//! streaming, etc. — not just the local-fs unit-test impl.
//!
//! Gated on `STORAGE_EMULATOR_HOST` + `ENGRAM_TEST_GCS_BUCKET`.
//! No-op without them.

use std::sync::Arc;

use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::BlobStorage;
use engram_storage_gcs::GcsBlobStorage;
use tokio::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunk_a_64mib_file_through_gcs_round_trips() {
    let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
        return;
    };
    let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
        return;
    };

    let blob: Arc<dyn BlobStorage> = Arc::new(
        GcsBlobStorage::connect(bucket)
            .await
            .expect("connect to emulator"),
    );
    let store = ChunkStore::new(blob.clone());

    // Unique scratch dir per run.
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src.bin");

    // 64 MiB of deterministic data. With 16 MiB chunks that's 4
    // chunks — enough to exercise the streaming put path, large
    // enough to catch any "we collected the whole body to memory"
    // regression.
    let mut data = vec![0u8; 64 * 1024 * 1024];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    fs::write(&src, &data).await.unwrap();

    // chunk_file → manifest with 4 chunk refs.
    let manifest = store
        .chunk_file(&src, ManifestKind::Disk, Some(16 * 1024 * 1024))
        .await
        .expect("chunk_file");
    assert_eq!(manifest.chunks.len(), 4, "expected 4 × 16 MiB chunks");
    assert_eq!(manifest.total_bytes, data.len() as u64);

    // materialize_to_file should round-trip byte-for-byte.
    let dest = work.path().join("dest.bin");
    store
        .materialize_to_file(&manifest, &dest)
        .await
        .expect("materialize");
    let got = fs::read(&dest).await.unwrap();
    assert_eq!(got, data, "round-tripped bytes must match");

    // Cleanup. Walk the chunk hashes and delete from the bucket so
    // the emulator state doesn't grow without bound across runs.
    for entry in &manifest.chunks {
        let _ = blob.delete(&entry.hash.storage_key()).await;
    }
}

/// Cross-session dedup: two distinct manifests built from
/// identical bytes share the same chunks in storage. Verifies the
/// content-addressing property end-to-end against a real backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_files_share_chunks_in_gcs() {
    let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
        return;
    };
    let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
        return;
    };

    let blob: Arc<dyn BlobStorage> = Arc::new(
        GcsBlobStorage::connect(bucket)
            .await
            .expect("connect to emulator"),
    );
    let store = ChunkStore::new(blob.clone());
    let work = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..96u8).cycle().take(20 * 1024 * 1024).collect();
    let p1 = work.path().join("a.bin");
    let p2 = work.path().join("b.bin");
    fs::write(&p1, &data).await.unwrap();
    fs::write(&p2, &data).await.unwrap();

    let m1 = store
        .chunk_file(&p1, ManifestKind::Disk, Some(16 * 1024 * 1024))
        .await
        .unwrap();
    let m2 = store
        .chunk_file(&p2, ManifestKind::Disk, Some(16 * 1024 * 1024))
        .await
        .unwrap();

    // Same chunk hashes → same storage keys → cross-file dedup.
    assert_eq!(m1.chunks, m2.chunks);
    for entry in &m1.chunks {
        let exists = blob.exists(&entry.hash.storage_key()).await.unwrap();
        assert!(exists, "chunk {} should be present in GCS", entry.hash);
    }

    for entry in &m1.chunks {
        let _ = blob.delete(&entry.hash.storage_key()).await;
    }
}
