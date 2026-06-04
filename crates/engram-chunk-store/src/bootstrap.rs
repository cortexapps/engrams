//! Bootstrap layer format for chunked OCI artifacts.
//!
//! ADR 0008 Phase 3 introduced the bootstrap: a small JSON index
//! describing every chunk of the image. ADR 0036 changed what it
//! points at — each chunk is now its **own OCI blob** whose digest
//! is `sha256:<chunk-hash>`, instead of a byte range inside one
//! monolithic concatenated chunk blob. Push moves only blobs the
//! registry doesn't already have; pull moves only chunks missing
//! from BlobStorage. The monolithic blob (one fragile ~10 GB upload
//! session, GHCR's 10 GB layer ceiling, zero cross-bake dedup) is
//! retired.
//!
//! This module is the bake-time *producer* (build a bootstrap from
//! an existing `Manifest` — a pure function, no chunk I/O) and the
//! runtime *consumer* (parse a bootstrap into an
//! `OciChunkIndex`-shaped lookup table — the actual `OciChunkResolver`
//! lives in `engram-oci` to keep the chunk-store crate registry-
//! agnostic).
//!
//! Content addressing is single-layered and uniform: a chunk's
//! sha256 is simultaneously its BlobStorage key, its `ChunkResolver`
//! identity, and its OCI blob digest.

use serde::{Deserialize, Serialize};

use crate::manifest::{ChunkHash, ChunkSize, Manifest, ManifestKind};

/// On-wire schema version. Bumped only on incompatible changes.
/// v2 = ADR 0036 per-chunk blobs (`blob_digest` always `Some`,
/// `blob_offset` always 0). v1 (monolithic chunk blob) readers and
/// writers were retired in the same change — consumers reject v1
/// with a "re-bake this image" error.
pub const BOOTSTRAP_SCHEMA_VERSION: u32 = 2;

/// One entry in the bootstrap — a single chunk's metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BootstrapEntry {
    /// Byte offset in the *source file* (ext4 for disk,
    /// `memory.bin` for memory). Matches `Manifest::chunks[i].offset`.
    /// Used by `materialize_*` paths to place chunks correctly when
    /// reconstructing the source file from chunks.
    pub file_offset: u64,
    /// OCI blob digest (`sha256:<hex>`) of this chunk's blob —
    /// ADR 0036: always `Some("sha256:<chunk-hash>")` in v2
    /// bootstraps. Kept `Option` on the wire so v1 documents (where
    /// `None` meant "this image's monolithic primary blob") parse
    /// far enough to be rejected with a useful error instead of a
    /// serde failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_digest: Option<String>,
    /// Byte offset within the chunk's blob. Always 0 in v2 (each
    /// chunk is a whole blob); nonzero only in retired v1 documents.
    pub blob_offset: u64,
    /// Length of this chunk's bytes. Typically `chunk_size` (16 MiB
    /// for disk, 512 KB for memory); the final chunk may be shorter
    /// when `total_bytes % chunk_size != 0`.
    pub length: u32,
    /// sha256 of the chunk's bytes. Identical to the
    /// `chunks/sha256/<hex>` key the chunk lives at in BlobStorage,
    /// to the chunk's identity in any `ChunkResolver`, and (modulo
    /// the `sha256:` prefix) to the chunk's OCI blob digest.
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
    /// Build a v2 (per-chunk-blob, ADR 0036) bootstrap from an
    /// existing manifest. Pure function — no chunk reads, no blob
    /// assembly: every entry's blob digest *is* its chunk hash, and
    /// every length is derivable from the manifest's geometry
    /// (`chunk_file` emits fixed-offset chunks of `chunk_size`
    /// bytes, final chunk truncated to `total_bytes`; zero chunks
    /// are elided from the manifest entirely).
    pub fn build_per_chunk(manifest: &Manifest) -> Bootstrap {
        let chunk_size = manifest.chunk_size.as_u64();
        let entries = manifest
            .chunks
            .iter()
            .map(|c| {
                let length = chunk_size.min(manifest.total_bytes - c.offset) as u32;
                BootstrapEntry {
                    file_offset: c.offset,
                    blob_digest: Some(format!("sha256:{}", c.hash.to_hex())),
                    blob_offset: 0,
                    length,
                    sha256: c.hash,
                }
            })
            .collect();
        Bootstrap {
            schema_version: BOOTSTRAP_SCHEMA_VERSION,
            kind: manifest.kind,
            total_bytes: manifest.total_bytes,
            chunk_size: manifest.chunk_size,
            entries,
        }
    }

    /// True iff this document is the ADR 0036 per-chunk shape every
    /// current consumer requires. v1 documents (monolithic chunk
    /// blob) should be rejected with a "re-bake this image" error.
    pub fn is_per_chunk(&self) -> bool {
        self.schema_version >= BOOTSTRAP_SCHEMA_VERSION
            && self.entries.iter().all(|e| e.blob_digest.is_some())
    }

    /// Synthesize a `Manifest` covering the same chunks.
    ///
    /// ADR 0008 Phase 5 final piece: a chunked-OCI image's
    /// `Manifest` lives in the bake's BlobStorage namespace, not
    /// the runtime host's. The bootstrap layer (small, travels
    /// with the OCI artifact) carries enough metadata to
    /// reconstruct an equivalent `Manifest` on the runtime side:
    /// chunk hashes + file offsets + size + kind. Dropped fields
    /// (`blob_digest`, `blob_offset` on `BootstrapEntry`) are
    /// OCI-specific — at runtime, chunks are fetched via the
    /// tiered `ChunkResolver`, which knows how to find them in
    /// BlobStorage or OCI.
    ///
    /// Caller is responsible for `put_manifest` under the
    /// `ManifestRef` from `bundle.json::disk_manifest` (or
    /// `canonical_memory_manifest` for memory bootstraps). After
    /// that, every consumer of `chunk_store.get_manifest` —
    /// materialize, NBD daemon, UFFD handler — finds it in the
    /// local BlobStorage and works unchanged.
    pub fn to_manifest(&self) -> Manifest {
        Manifest {
            schema_version: crate::manifest::MANIFEST_SCHEMA_VERSION,
            kind: self.kind,
            chunk_size: self.chunk_size,
            total_bytes: self.total_bytes,
            chunks: self
                .entries
                .iter()
                .map(|e| crate::manifest::ChunkRef {
                    offset: e.file_offset,
                    hash: e.sha256,
                })
                .collect(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkRef, ChunkSize, Manifest, ManifestKind};

    fn manifest_of(chunk_size: u64, total_bytes: u64, chunks: Vec<(u64, ChunkHash)>) -> Manifest {
        Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes,
            chunk_size: ChunkSize::bytes(chunk_size),
            chunks: chunks
                .into_iter()
                .map(|(offset, hash)| ChunkRef { offset, hash })
                .collect(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    #[test]
    fn build_per_chunk_addresses_each_chunk_by_its_own_digest() {
        let h0 = ChunkHash::of(b"chunk zero");
        let h1 = ChunkHash::of(b"chunk one");
        let m = manifest_of(4, 8, vec![(0, h0), (4, h1)]);

        let bs = Bootstrap::build_per_chunk(&m);
        assert_eq!(bs.schema_version, BOOTSTRAP_SCHEMA_VERSION);
        assert_eq!(bs.kind, ManifestKind::Disk);
        assert_eq!(bs.total_bytes, 8);
        assert_eq!(bs.entries.len(), 2);
        assert!(bs.is_per_chunk());

        for (entry, hash) in bs.entries.iter().zip([h0, h1]) {
            assert_eq!(entry.sha256, hash);
            assert_eq!(
                entry.blob_digest.as_deref(),
                Some(format!("sha256:{}", hash.to_hex()).as_str()),
                "OCI blob digest must be the chunk hash"
            );
            assert_eq!(entry.blob_offset, 0, "each chunk is a whole blob");
            assert_eq!(entry.length, 4);
        }
        assert_eq!(bs.entries[0].file_offset, 0);
        assert_eq!(bs.entries[1].file_offset, 4);
    }

    #[test]
    fn build_per_chunk_truncates_final_chunk_to_total_bytes() {
        let h0 = ChunkHash::of(b"full");
        let h1 = ChunkHash::of(b"short tail");
        // total 7 with chunk_size 4 → final chunk is 3 bytes.
        let m = manifest_of(4, 7, vec![(0, h0), (4, h1)]);
        let bs = Bootstrap::build_per_chunk(&m);
        assert_eq!(bs.entries[0].length, 4);
        assert_eq!(bs.entries[1].length, 3);
    }

    #[test]
    fn build_per_chunk_preserves_zero_chunk_elision_gaps() {
        // `chunk_file` elides all-zero chunks: the manifest simply
        // has no ChunkRef at that offset. The bootstrap mirrors the
        // gap; materialize fills it with zeros.
        let h0 = ChunkHash::of(b"head");
        let h2 = ChunkHash::of(b"tail after a hole");
        let m = manifest_of(4, 12, vec![(0, h0), (8, h2)]);
        let bs = Bootstrap::build_per_chunk(&m);
        assert_eq!(bs.entries.len(), 2);
        assert_eq!(bs.entries[0].file_offset, 0);
        assert_eq!(bs.entries[1].file_offset, 8);
        // Both chunks are full-size; the hole contributes no entry.
        assert!(bs.entries.iter().all(|e| e.length == 4));
    }

    #[test]
    fn bootstrap_round_trips_through_json() {
        let h = ChunkHash::of(b"a");
        let m = manifest_of(512, 512, vec![(0, h)]);
        let bootstrap = Bootstrap::build_per_chunk(&m);
        let json = serde_json::to_vec(&bootstrap).unwrap();
        let back: Bootstrap = serde_json::from_slice(&json).unwrap();
        assert_eq!(bootstrap, back);
        assert!(back.is_per_chunk());
    }

    /// v1 documents (monolithic chunk blob: schema_version 1,
    /// `blob_digest` absent) still *parse* — serde default — but
    /// must be detectable so consumers can reject them with a
    /// "re-bake this image" error instead of mis-reading offsets.
    #[test]
    fn v1_monolithic_bootstrap_parses_but_is_not_per_chunk() {
        let v1 = serde_json::json!({
            "schema_version": 1,
            "kind": serde_json::to_value(ManifestKind::Disk).unwrap(),
            "total_bytes": 8,
            "chunk_size": 4,
            "entries": [
                {"file_offset": 0, "blob_offset": 0, "length": 4,
                 "sha256": ChunkHash::of(b"x")},
                {"file_offset": 4, "blob_offset": 4, "length": 4,
                 "sha256": ChunkHash::of(b"y")},
            ],
        });
        let bs: Bootstrap = serde_json::from_value(v1).unwrap();
        assert!(!bs.is_per_chunk());
    }

    /// `to_manifest` round-trip: build a Bootstrap from a Manifest,
    /// then convert back. Chunks, offsets, sizes, kinds all
    /// preserved.
    #[test]
    fn to_manifest_round_trips_chunk_layout() {
        let hashes = [
            ChunkHash::of(b"AAA"),
            ChunkHash::of(b"BBBB"),
            ChunkHash::of(b"CC"),
        ];
        let original = manifest_of(
            16,
            64,
            vec![(0, hashes[0]), (16, hashes[1]), (32, hashes[2])],
        );
        let bootstrap = Bootstrap::build_per_chunk(&original);
        let round_tripped = bootstrap.to_manifest();
        assert_eq!(round_tripped.chunks.len(), original.chunks.len());
        for (a, b) in round_tripped.chunks.iter().zip(original.chunks.iter()) {
            assert_eq!(a.offset, b.offset);
            assert_eq!(a.hash, b.hash);
        }
        assert_eq!(round_tripped.kind, original.kind);
        assert_eq!(round_tripped.chunk_size, original.chunk_size);
        assert_eq!(round_tripped.total_bytes, original.total_bytes);
    }
}
