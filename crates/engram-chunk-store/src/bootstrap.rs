//! Bootstrap layer format for Nydus-shaped OCI artifacts.
//!
//! ADR 0008 Phase 3: a bake produces two layers per kind (disk,
//! memory):
//!
//! - **Bootstrap** (small JSON): the per-image index from
//!   `ChunkHash` → `(blob_offset, length)`, plus file-offset
//!   metadata for materialization.
//! - **Chunk blob** (large bytes): concatenation of all chunks in
//!   bootstrap-entry order. Pulled lazily via Range GET at
//!   fault time.
//!
//! This module is the bake-time *producer* (build a bootstrap +
//! blob from an existing `Manifest` + `ChunkStore`) and the
//! runtime *consumer* (parse a bootstrap into an
//! `OciChunkIndex`-shaped lookup table — the actual `OciChunkResolver`
//! lives in `engram-oci` to keep the chunk-store crate registry-
//! agnostic).
//!
//! The format is content-addressed at two layers:
//!
//! - Each `ChunkRef.sha256` is the hash of the chunk's bytes.
//!   Used at fault time to verify a Range GET returned correct
//!   bytes.
//! - The OCI layer digest of the chunk-blob layer is `sha256` over
//!   the entire concatenated blob. Computed at push time by the
//!   OCI client; recorded in the chunked image artifact's OCI
//!   manifest. Different from the per-chunk hashes.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::manifest::{ChunkHash, ChunkSize, Manifest, ManifestKind};
use crate::store::ChunkStore;

/// On-wire schema version. Bumped only on incompatible changes.
pub const BOOTSTRAP_SCHEMA_VERSION: u32 = 1;

/// One entry in the bootstrap — a single chunk's metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BootstrapEntry {
    /// Byte offset in the *source file* (ext4 for disk,
    /// `memory.bin` for memory). Matches `Manifest::chunks[i].offset`.
    /// Used by `materialize_*` paths to place chunks correctly when
    /// reconstructing the source file from chunks.
    pub file_offset: u64,
    /// Byte offset in the chunk blob layer. `entries[0].blob_offset
    /// == 0`; subsequent entries follow contiguously.
    pub blob_offset: u64,
    /// Length of this chunk's bytes. Typically `chunk_size` (16 MiB
    /// for disk, 512 KB for memory); the final chunk may be shorter
    /// when `total_bytes % chunk_size != 0`.
    pub length: u32,
    /// sha256 of the chunk's bytes. Identical to the
    /// `chunks/sha256/<hex>` key the chunk lives at in BlobStorage,
    /// and to the chunk's identity in any `ChunkResolver`.
    pub sha256: ChunkHash,
}

/// On-wire bootstrap document. Pushed as an OCI layer with
/// mediaType `application/vnd.engram.bootstrap.{disk,memory}.v1+json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub schema_version: u32,
    pub kind: ManifestKind,
    /// Length of the source file. With zero blocks elided, the sum
    /// of `entries[].length` is ≤ `total_bytes`; materialize fills
    /// the gaps with zeros.
    pub total_bytes: u64,
    /// Nominal chunk size (16 MiB for disk, 512 KB for memory). The
    /// actual per-chunk length lives in `entries[].length` —
    /// `chunk_size` is informational, useful for cache sizing and
    /// fault-coalescing heuristics.
    pub chunk_size: ChunkSize,
    pub entries: Vec<BootstrapEntry>,
}

impl Bootstrap {
    /// Build a bootstrap + concatenated chunk-blob bytes from an
    /// existing manifest. Reads each chunk from the store and
    /// concatenates them in manifest order; the bootstrap records
    /// the per-chunk byte offset into the resulting blob.
    ///
    /// The returned `Vec<u8>` is the bytes to push as the chunks
    /// OCI layer; the registry computes its sha256 digest at push
    /// time.
    ///
    /// Memory cost: O(manifest.total_bytes). For a 4 GiB ext4
    /// that's a 4 GiB allocation. v1 accepts this; a streaming
    /// shape (write the blob to a temp file, then push from the
    /// file) is the obvious optimization when manifests get
    /// larger.
    pub async fn build_from_manifest(
        store: &ChunkStore,
        manifest: &Manifest,
    ) -> Result<(Bootstrap, Vec<u8>)> {
        let mut entries = Vec::with_capacity(manifest.chunks.len());
        let mut blob: Vec<u8> = Vec::new();
        let mut blob_offset: u64 = 0;
        for c in &manifest.chunks {
            let bytes: Bytes = store.get_chunk(c.hash).await?;
            let length = bytes.len() as u32;
            entries.push(BootstrapEntry {
                file_offset: c.offset,
                blob_offset,
                length,
                sha256: c.hash,
            });
            blob.extend_from_slice(&bytes);
            blob_offset += length as u64;
        }
        Ok((
            Bootstrap {
                schema_version: BOOTSTRAP_SCHEMA_VERSION,
                kind: manifest.kind,
                total_bytes: manifest.total_bytes,
                chunk_size: manifest.chunk_size,
                entries,
            },
            blob,
        ))
    }

    /// Sum of `entries[].length`. Equals the OCI chunk-blob layer
    /// size and is useful as a sanity check at push time
    /// (`blob.len() == bootstrap.chunk_blob_len()`).
    pub fn chunk_blob_len(&self) -> u64 {
        self.entries.iter().map(|e| e.length as u64).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkRef, ChunkSize, ManifestKind};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    async fn store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    #[tokio::test]
    async fn build_from_manifest_concatenates_chunks_in_order() {
        let (s, _d) = store().await;
        // Three chunks of distinct content; put them into the store.
        let b0 = b"AAAA"; // 4 bytes
        let b1 = b"BBBBBBBB"; // 8 bytes
        let b2 = b"CCC"; // 3 bytes
        let h0 = s.put_chunk(b0).await.unwrap();
        let h1 = s.put_chunk(b1).await.unwrap();
        let h2 = s.put_chunk(b2).await.unwrap();

        let manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: 100,
            chunk_size: ChunkSize::bytes(4),
            chunks: vec![
                ChunkRef {
                    offset: 0,
                    hash: h0,
                },
                ChunkRef {
                    offset: 4,
                    hash: h1,
                },
                ChunkRef {
                    offset: 12,
                    hash: h2,
                },
            ],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };

        let (bootstrap, blob) = Bootstrap::build_from_manifest(&s, &manifest).await.unwrap();

        // Bootstrap shape.
        assert_eq!(bootstrap.schema_version, BOOTSTRAP_SCHEMA_VERSION);
        assert_eq!(bootstrap.kind, ManifestKind::Disk);
        assert_eq!(bootstrap.total_bytes, 100);
        assert_eq!(bootstrap.chunk_size, ChunkSize::bytes(4));
        assert_eq!(bootstrap.entries.len(), 3);

        // Blob offsets are cumulative.
        assert_eq!(bootstrap.entries[0].blob_offset, 0);
        assert_eq!(bootstrap.entries[0].length, 4);
        assert_eq!(bootstrap.entries[1].blob_offset, 4);
        assert_eq!(bootstrap.entries[1].length, 8);
        assert_eq!(bootstrap.entries[2].blob_offset, 12);
        assert_eq!(bootstrap.entries[2].length, 3);

        // File offsets preserved.
        assert_eq!(bootstrap.entries[0].file_offset, 0);
        assert_eq!(bootstrap.entries[1].file_offset, 4);
        assert_eq!(bootstrap.entries[2].file_offset, 12);

        // Hashes match.
        assert_eq!(bootstrap.entries[0].sha256, h0);
        assert_eq!(bootstrap.entries[1].sha256, h1);
        assert_eq!(bootstrap.entries[2].sha256, h2);

        // Blob bytes are the concatenation.
        assert_eq!(&blob[..], b"AAAABBBBBBBBCCC");
        assert_eq!(bootstrap.chunk_blob_len(), 15);
        assert_eq!(blob.len() as u64, bootstrap.chunk_blob_len());
    }

    #[tokio::test]
    async fn build_from_empty_manifest_produces_empty_blob() {
        let (s, _d) = store().await;
        let manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: 0,
            chunk_size: ChunkSize::bytes(16 * 1024 * 1024),
            chunks: vec![],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let (bootstrap, blob) = Bootstrap::build_from_manifest(&s, &manifest).await.unwrap();
        assert!(bootstrap.entries.is_empty());
        assert!(blob.is_empty());
        assert_eq!(bootstrap.chunk_blob_len(), 0);
    }

    #[test]
    fn bootstrap_round_trips_through_json() {
        let bootstrap = Bootstrap {
            schema_version: BOOTSTRAP_SCHEMA_VERSION,
            kind: ManifestKind::Memory,
            total_bytes: 1024,
            chunk_size: ChunkSize::bytes(512),
            entries: vec![
                BootstrapEntry {
                    file_offset: 0,
                    blob_offset: 0,
                    length: 512,
                    sha256: ChunkHash::of(b"a"),
                },
                BootstrapEntry {
                    file_offset: 512,
                    blob_offset: 512,
                    length: 512,
                    sha256: ChunkHash::of(b"b"),
                },
            ],
        };
        let json = serde_json::to_vec(&bootstrap).unwrap();
        let back: Bootstrap = serde_json::from_slice(&json).unwrap();
        assert_eq!(bootstrap, back);
    }

    /// Hash verification: a bootstrap claims a chunk's sha256, and
    /// the same chunk content (read from the store, found at the
    /// claimed blob_offset/length) must hash to that value. This is
    /// the contract that lets a Range GET at runtime be verified.
    #[tokio::test]
    async fn blob_at_entry_offset_hashes_to_entry_sha256() {
        let (s, _d) = store().await;
        let body = b"verification test chunk";
        let h = s.put_chunk(body).await.unwrap();
        let manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: body.len() as u64,
            chunk_size: ChunkSize::bytes(4096),
            chunks: vec![ChunkRef { offset: 0, hash: h }],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let (bootstrap, blob) = Bootstrap::build_from_manifest(&s, &manifest).await.unwrap();
        let entry = &bootstrap.entries[0];
        let start = entry.blob_offset as usize;
        let end = start + entry.length as usize;
        let chunk_bytes = &blob[start..end];
        assert_eq!(ChunkHash::of(chunk_bytes), entry.sha256);
    }

    /// Builds against a never-stored ChunkHash should surface as a
    /// `Blob` error from the underlying store (not a panic).
    #[tokio::test]
    async fn build_propagates_missing_chunk_error() {
        let (s, _d) = store().await;
        // Reference a hash that was never PUT.
        let unknown = ChunkHash::of(b"never-stored");
        let manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: 16,
            chunk_size: ChunkSize::bytes(16),
            chunks: vec![ChunkRef {
                offset: 0,
                hash: unknown,
            }],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let result = Bootstrap::build_from_manifest(&s, &manifest).await;
        assert!(result.is_err(), "expected missing-chunk error");
    }
}
