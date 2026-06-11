//! Manifests, chunk references, and content-hash addressing.
//!
//! A **manifest** is the versioned, immutable view of a virtual
//! disk or memory image. It lists the chunks that make up the
//! image, where each chunk lives at a specific byte offset and is
//! identified by the sha256 of its contents.
//!
//! Manifests are append-only: a "new version" produces a *new*
//! JSON object at `manifests/<manifest_id>/v<version>.json`. Old
//! versions stay forever (until GC sweeps them) so consumers can
//! pin to a specific version.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ChunkStoreError, Result};
use crate::working_set::TraceRef;

/// Current schema version. Bumped only on incompatible manifest
/// format changes; backwards-compatible field additions don't bump.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Default chunk size for disk manifests. 16 MiB — Replit's published
/// size; sequential-friendly; one GCS PUT per chunk is the right scale
/// for cold-tier flush. Smaller wastes object-storage roundtrips;
/// larger means every byte of dirty data costs a big PUT.
pub const DEFAULT_DISK_CHUNK_SIZE: u64 = 16 * 1024 * 1024;

/// Default chunk size for memory manifests. 512 KiB — AWS Lambda's
/// published size; tight enough that a few-KiB dirty page doesn't
/// force a multi-MiB upload, loose enough that the manifest stays
/// short for a 16 GiB VM (~32k entries vs 4M for 4 KiB).
pub const DEFAULT_MEMORY_CHUNK_SIZE: u64 = 512 * 1024;

/// Whether a manifest describes disk content or memory content.
/// Carried separately from `chunk_size` (although they correlate)
/// so consumers can dispatch on intent rather than guess from size.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManifestKind {
    /// A virtual block device. Consumed by NBD daemon (Linux+FC) or
    /// materialize-to-file (macOS+VZ).
    Disk,
    /// A virtual memory image. Consumed by the UFFD handler
    /// (Linux+FC). Combined with a canonical-base manifest for the
    /// cross-VM page-cache sharing trick.
    Memory,
}

impl ManifestKind {
    /// Default chunk size for this kind.
    pub fn default_chunk_size(self) -> u64 {
        match self {
            Self::Disk => DEFAULT_DISK_CHUNK_SIZE,
            Self::Memory => DEFAULT_MEMORY_CHUNK_SIZE,
        }
    }
}

/// Chunk size in bytes. Newtype around u64 so the API signatures
/// can't accidentally take a count-of-chunks where bytes were meant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkSize(pub u64);

impl ChunkSize {
    pub const fn bytes(b: u64) -> Self {
        Self(b)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ChunkSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// sha256 of a chunk's raw bytes. The chunk's storage key is
/// derived from this; the same bytes always map to the same key
/// regardless of which manifest or session produced them.
///
/// Stored as 32 raw bytes; serialized as lowercase hex for JSON
/// readability and consistency with object-storage paths.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ChunkHash([u8; 32]);

impl ChunkHash {
    /// Compute the hash of a chunk's bytes.
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&digest);
        Self(buf)
    }

    /// Raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Wrap a raw 32-byte digest (e.g. candidate-table bytes read back
    /// from PG) without recomputing or hex round-tripping.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Lowercase hex (64 chars).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Parse from a 64-char lowercase hex string.
    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != 64 {
            return Err(ChunkStoreError::MalformedManifest(format!(
                "chunk hash hex must be 64 chars, got {}",
                s.len()
            )));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let hex = &s[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(hex, 16).map_err(|e| {
                ChunkStoreError::MalformedManifest(format!("chunk hash hex byte {i}: {e}"))
            })?;
        }
        Ok(Self(out))
    }

    /// Object-storage key for this chunk under the
    /// `chunks/sha256/` prefix. Splits the first byte off into a
    /// subdirectory so a chunk store with many millions of chunks
    /// doesn't put them all in one flat directory (matters for
    /// filesystem backends; harmless for object stores).
    pub fn storage_key(&self) -> String {
        let hex = self.to_hex();
        format!("chunks/sha256/{}/{}", &hex[..2], &hex[2..])
    }

    /// Reverse of [`Self::storage_key`]. Returns `None` for keys
    /// that don't match the `chunks/sha256/<2hex>/<62hex>` shape —
    /// e.g. stray non-chunk keys returned by a wildcard prefix
    /// listing. Phase C's GC sweep uses this to translate
    /// `BlobStorage::list_prefix("chunks/sha256/")` entries back
    /// into `ChunkHash` for pin-set lookup.
    pub fn from_storage_key(key: &str) -> Option<Self> {
        let rest = key.strip_prefix("chunks/sha256/")?;
        let (head, tail) = rest.split_once('/')?;
        if head.len() != 2 || tail.len() != 62 {
            return None;
        }
        let mut hex = String::with_capacity(64);
        hex.push_str(head);
        hex.push_str(tail);
        Self::from_hex(&hex).ok()
    }
}

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkHash({})", self.to_hex())
    }
}

impl fmt::Display for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

// Serde via hex string — JSON manifests stay human-inspectable.
impl Serialize for ChunkHash {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ChunkHash {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// One entry in a manifest's chunk list. Chunks are placed at
/// fixed offsets that are multiples of the manifest's `chunk_size`,
/// so `offset / chunk_size` is the chunk index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub offset: u64,
    pub hash: ChunkHash,
}

// `ManifestRef` lives in `engram-core::types::manifest` so the trait-
// adjacent types (`SnapshotMetadata`, `SandboxSpec`) can mention it
// without a dep cycle through this crate. Re-exported here so callers
// continue to see one canonical name.
pub use engram_core::types::manifest::ManifestRef;

/// A manifest is the durable, versioned view of a virtual disk or
/// memory image: which chunks are at which offsets, plus enough
/// metadata for consumers to reconstruct or share state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version. Bumped only on incompatible changes;
    /// readers check `<=` MAX_SUPPORTED before consuming.
    pub schema_version: u32,

    /// Disk vs memory; consumers dispatch on this.
    pub kind: ManifestKind,

    /// Bytes-per-chunk. Last chunk may be shorter if
    /// `total_bytes` isn't a multiple.
    pub chunk_size: ChunkSize,

    /// Total size of the virtual image in bytes. Consumers use
    /// this for the block-device size, the mmap length, etc.
    pub total_bytes: u64,

    /// Ordered list of `(offset, hash)` pairs. Offsets must be
    /// multiples of `chunk_size`. Gaps mean "zero-filled" — a
    /// sparse manifest needn't list a chunk for every offset.
    pub chunks: Vec<ChunkRef>,

    /// For a forked manifest, the manifest+version we forked from.
    /// Consumers can use this for delta-tracking diagnostics; it's
    /// **not** load-bearing for read correctness (each manifest
    /// version is self-contained — all its chunks are listed in
    /// `chunks`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ManifestRef>,

    /// For memory manifests: the working-set trace the UFFD
    /// handler should prefault before letting vCPUs run. Stored
    /// separately under `traces/` because traces are host-local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_set_trace: Option<TraceRef>,

    /// Free-form metadata. The chunk store doesn't interpret this;
    /// callers stash hints (image_tag, session_id, captured_at) for
    /// observability + tooling. Bounded by JSON sanity (a few KB).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub annotations: serde_json::Value,
}

impl Manifest {
    /// ADR 0036 P4: a `ManifestRef` derived from this manifest's
    /// *content* — a pure function of (kind, chunk_size, total_bytes,
    /// ordered chunk offsets + hashes). Two bakes that produce
    /// byte-identical images get the **same** manifest identity, which
    /// is what makes "this re-bake is content-identical to an already
    /// enabled image" recognizable downstream: the enable pipeline can
    /// reuse the existing base snapshot (skipping the capture VM) by
    /// comparing `enabled_images.disk_manifest_*` columns instead of
    /// OCI tags/digests.
    ///
    /// Contrast `ManifestRef::new()` (random UUID): right for
    /// *snapshot* manifests, whose content is nondeterministic per
    /// capture; wrong for bake outputs, where the random id polluted
    /// `bundle.json` and made even deterministic re-bakes look like
    /// new content at every layer above.
    ///
    /// The UUID is the first 16 bytes of sha256 over the canonical
    /// fields, with RFC 4122 version (8 = custom) and variant bits set
    /// so it round-trips as a well-formed UUID everywhere a random one
    /// would. Version is always 1 — content-derived ids never tick.
    pub fn content_ref(&self) -> ManifestRef {
        let mut h = Sha256::new();
        h.update(b"engram-manifest-content-v1");
        h.update([match self.kind {
            ManifestKind::Disk => 0u8,
            ManifestKind::Memory => 1u8,
        }]);
        h.update(self.chunk_size.as_u64().to_le_bytes());
        h.update(self.total_bytes.to_le_bytes());
        for c in &self.chunks {
            h.update(c.offset.to_le_bytes());
            h.update(c.hash.as_bytes());
        }
        let digest = h.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0F) | 0x80; // version 8 (custom)
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
        ManifestRef {
            manifest_id: uuid::Uuid::from_bytes(bytes),
            version: 1,
        }
    }

    /// Construct an empty manifest for a fresh disk or memory
    /// image. `chunk_size` defaults to the per-kind default.
    pub fn empty(kind: ManifestKind, total_bytes: u64) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind,
            chunk_size: ChunkSize::bytes(kind.default_chunk_size()),
            total_bytes,
            chunks: Vec::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    /// Number of chunks needed to cover `total_bytes` at the
    /// manifest's `chunk_size`. The final chunk may be shorter
    /// than `chunk_size`.
    pub fn expected_chunk_count(&self) -> u64 {
        let sz = self.chunk_size.as_u64();
        if sz == 0 {
            return 0;
        }
        self.total_bytes.div_ceil(sz)
    }

    /// Validate internal consistency: offsets multiples of
    /// chunk_size, monotonically increasing, within total_bytes,
    /// schema_version supported.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(ChunkStoreError::UnsupportedSchemaVersion {
                found: self.schema_version,
                max_supported: MANIFEST_SCHEMA_VERSION,
            });
        }
        let sz = self.chunk_size.as_u64();
        if sz == 0 {
            return Err(ChunkStoreError::MalformedManifest(
                "chunk_size must be > 0".into(),
            ));
        }
        let mut last_offset: Option<u64> = None;
        for entry in &self.chunks {
            if entry.offset % sz != 0 {
                return Err(ChunkStoreError::MalformedManifest(format!(
                    "chunk offset {} is not a multiple of chunk_size {sz}",
                    entry.offset
                )));
            }
            if entry.offset >= self.total_bytes {
                return Err(ChunkStoreError::MalformedManifest(format!(
                    "chunk offset {} exceeds total_bytes {}",
                    entry.offset, self.total_bytes
                )));
            }
            if let Some(prev) = last_offset {
                if entry.offset <= prev {
                    return Err(ChunkStoreError::MalformedManifest(format!(
                        "chunk offsets must be strictly increasing: {} after {}",
                        entry.offset, prev
                    )));
                }
            }
            last_offset = Some(entry.offset);
        }
        Ok(())
    }

    /// Find the chunk at a given offset (must be exactly aligned
    /// to `chunk_size`). Returns `None` if no chunk is listed for
    /// that offset — treat as zero-filled.
    pub fn chunk_at(&self, offset: u64) -> Option<&ChunkRef> {
        // Linear scan is fine for typical manifest sizes (<10k
        // entries). Switch to a binary search if profiling shows
        // it matters.
        self.chunks.iter().find(|c| c.offset == offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn chunk_hash_round_trips_through_hex() {
        let bytes = b"hello, chunked world";
        let h = ChunkHash::of(bytes);
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        let back = ChunkHash::from_hex(&hex).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn chunk_hash_storage_key_splits_first_byte() {
        let h = ChunkHash::of(b"x");
        let key = h.storage_key();
        let hex = h.to_hex();
        let want = format!("chunks/sha256/{}/{}", &hex[..2], &hex[2..]);
        assert_eq!(key, want);
    }

    #[test]
    fn chunk_hash_from_hex_rejects_wrong_length_and_non_hex() {
        assert!(ChunkHash::from_hex("abcd").is_err());
        assert!(ChunkHash::from_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn chunk_hash_is_stable_across_calls() {
        // The same bytes must always hash the same — content
        // addressing depends on this.
        let bytes = b"deadbeef";
        let a = ChunkHash::of(bytes);
        let b = ChunkHash::of(bytes);
        assert_eq!(a, b);
    }

    #[test]
    fn manifest_ref_next_version_keeps_id() {
        let r = ManifestRef::new();
        let next = r.next_version();
        assert_eq!(r.manifest_id, next.manifest_id);
        assert_eq!(next.version, r.version + 1);
    }

    #[test]
    fn manifest_ref_storage_key_includes_version() {
        let r = ManifestRef {
            manifest_id: Uuid::nil(),
            version: 42,
        };
        assert_eq!(
            r.storage_key(),
            "manifests/00000000-0000-0000-0000-000000000000/v42.json"
        );
    }

    #[test]
    fn empty_manifest_uses_kind_default_chunk_size() {
        let m = Manifest::empty(ManifestKind::Disk, 100 * 1024 * 1024);
        assert_eq!(m.chunk_size.as_u64(), DEFAULT_DISK_CHUNK_SIZE);
        let m = Manifest::empty(ManifestKind::Memory, 4 * 1024 * 1024 * 1024);
        assert_eq!(m.chunk_size.as_u64(), DEFAULT_MEMORY_CHUNK_SIZE);
    }

    #[test]
    fn expected_chunk_count_rounds_up() {
        let mut m = Manifest::empty(ManifestKind::Disk, 0);
        m.chunk_size = ChunkSize::bytes(1024);
        m.total_bytes = 0;
        assert_eq!(m.expected_chunk_count(), 0);
        m.total_bytes = 1024;
        assert_eq!(m.expected_chunk_count(), 1);
        m.total_bytes = 1025;
        assert_eq!(m.expected_chunk_count(), 2);
        m.total_bytes = 4096;
        assert_eq!(m.expected_chunk_count(), 4);
    }

    #[test]
    fn validate_rejects_misaligned_offset() {
        let mut m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024 * 2);
        m.chunks.push(ChunkRef {
            offset: 1024,
            hash: ChunkHash::of(b"x"),
        });
        match m.validate() {
            Err(ChunkStoreError::MalformedManifest(msg)) => {
                assert!(msg.contains("multiple"));
            }
            other => panic!("expected MalformedManifest, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_offset_past_total_bytes() {
        let mut m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 32 * 1024 * 1024, // past end
            hash: ChunkHash::of(b"x"),
        });
        match m.validate() {
            Err(ChunkStoreError::MalformedManifest(msg)) => assert!(msg.contains("exceeds")),
            other => panic!("expected MalformedManifest, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_non_monotonic_offsets() {
        let mut m = Manifest::empty(ManifestKind::Disk, 64 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 16 * 1024 * 1024,
            hash: ChunkHash::of(b"a"),
        });
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"b"),
        });
        match m.validate() {
            Err(ChunkStoreError::MalformedManifest(msg)) => assert!(msg.contains("increasing")),
            other => panic!("expected MalformedManifest, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_well_formed_manifest() {
        let mut m = Manifest::empty(ManifestKind::Disk, 48 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"a"),
        });
        m.chunks.push(ChunkRef {
            offset: 16 * 1024 * 1024,
            hash: ChunkHash::of(b"b"),
        });
        m.chunks.push(ChunkRef {
            offset: 32 * 1024 * 1024,
            hash: ChunkHash::of(b"c"),
        });
        assert!(m.validate().is_ok());
    }

    #[test]
    fn validate_rejects_future_schema_version() {
        let mut m = Manifest::empty(ManifestKind::Disk, 0);
        m.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        assert!(matches!(
            m.validate(),
            Err(ChunkStoreError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let mut m = Manifest::empty(ManifestKind::Memory, 4 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"hello"),
        });
        m.parent = Some(ManifestRef::new());
        m.annotations = serde_json::json!({ "image_tag": "warm-1" });
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn chunk_at_returns_none_for_zero_filled_gap() {
        let mut m = Manifest::empty(ManifestKind::Disk, 64 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"a"),
        });
        m.chunks.push(ChunkRef {
            offset: 32 * 1024 * 1024,
            hash: ChunkHash::of(b"c"),
        });
        // Gap at offset 16 MiB — unmaterialized → consumer treats as zero.
        assert!(m.chunk_at(16 * 1024 * 1024).is_none());
        assert!(m.chunk_at(0).is_some());
        assert!(m.chunk_at(32 * 1024 * 1024).is_some());
    }

    /// ADR 0036 P4: the content ref is a pure function of the
    /// manifest's chunk layout — identical content gets an identical
    /// (well-formed) UUID; any chunk perturbation changes it.
    #[test]
    fn content_ref_is_deterministic_and_content_sensitive() {
        let mut m = Manifest::empty(ManifestKind::Disk, 32 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"chunk-a"),
        });
        m.chunks.push(ChunkRef {
            offset: 16 * 1024 * 1024,
            hash: ChunkHash::of(b"chunk-b"),
        });

        let r1 = m.content_ref();
        let r2 = m.clone().content_ref();
        assert_eq!(r1, r2, "same content must derive the same ref");
        assert_eq!(r1.version, 1);
        // Well-formed RFC 4122 UUID (version + variant bits set).
        assert_eq!(r1.manifest_id.get_version_num(), 8);

        // Different chunk bytes → different ref.
        let mut other = m.clone();
        other.chunks[1].hash = ChunkHash::of(b"chunk-b-changed");
        assert_ne!(m.content_ref(), other.content_ref());

        // Same chunks at a different offset → different ref.
        let mut moved = m.clone();
        moved.chunks[1].offset = 0;
        assert_ne!(m.content_ref(), moved.content_ref());

        // Disk vs memory kinds never collide.
        let mut mem = m.clone();
        mem.kind = ManifestKind::Memory;
        assert_ne!(m.content_ref(), mem.content_ref());
    }
}
