//! Errors for `engram-chunk-store`.
//!
//! Hand-written enum (no `thiserror`) per the workspace convention.

use std::fmt;

use engram_core::error::BlobError;

pub type Result<T> = std::result::Result<T, ChunkStoreError>;

#[derive(Debug)]
pub enum ChunkStoreError {
    /// Underlying object storage failure.
    Blob(BlobError),

    /// JSON (de)serialization of a manifest or trace.
    Serialize(serde_json::Error),

    /// Chunk content didn't hash to its claimed name on read.
    /// This is a data-integrity violation — the blob store corrupted
    /// a chunk, or someone bypassed the chunk-store API to write a
    /// mismatched payload.
    HashMismatch { expected: String, actual: String },

    /// Manifest version conflict on commit — caller raced another
    /// writer. Caller should re-read, re-apply changes, retry.
    VersionConflict {
        manifest_id: uuid::Uuid,
        attempted: u64,
        latest: u64,
    },

    /// Manifest schema_version in the stored payload is newer than
    /// this binary supports. Indicates a downgrade after upgrade.
    UnsupportedSchemaVersion { found: u32, max_supported: u32 },

    /// Manifest claims a chunk at an offset that's not a multiple of
    /// the manifest's chunk_size, or has a total_bytes that doesn't
    /// reconcile with the chunk list. Indicates a malformed manifest.
    MalformedManifest(String),

    /// Local cache I/O failure (writes to NVMe, evictions).
    Cache(std::io::Error),

    /// Origin-tier resolver failure. ADR 0008: when chunks live in
    /// an OCI registry as Nydus-shaped layers, a Range GET against
    /// the registry can fail (network, 404 on missing chunk, 416
    /// on an out-of-bounds range, etc.). The string carries the
    /// resolver-specific error message; the resolver type
    /// (`OciChunkResolver`, future variants) chooses what to surface.
    Origin(String),

    /// Operation hit an internal invariant that shouldn't happen.
    /// Use for "should never reach here" branches that we still want
    /// to surface rather than panic.
    Internal(String),
}

impl fmt::Display for ChunkStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blob(e) => write!(f, "blob storage: {e}"),
            Self::Serialize(e) => write!(f, "manifest serde: {e}"),
            Self::HashMismatch { expected, actual } => write!(
                f,
                "chunk hash mismatch: expected {expected}, got {actual}"
            ),
            Self::VersionConflict {
                manifest_id,
                attempted,
                latest,
            } => write!(
                f,
                "manifest {manifest_id} version conflict: attempted v{attempted}, latest is v{latest}"
            ),
            Self::UnsupportedSchemaVersion {
                found,
                max_supported,
            } => write!(
                f,
                "manifest schema v{found} is newer than this binary supports (max v{max_supported})"
            ),
            Self::MalformedManifest(msg) => write!(f, "malformed manifest: {msg}"),
            Self::Cache(e) => write!(f, "local cache I/O: {e}"),
            Self::Origin(msg) => write!(f, "origin tier: {msg}"),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

impl std::error::Error for ChunkStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Blob(e) => Some(e),
            Self::Serialize(e) => Some(e),
            Self::Cache(e) => Some(e),
            _ => None,
        }
    }
}

impl From<BlobError> for ChunkStoreError {
    fn from(e: BlobError) -> Self {
        Self::Blob(e)
    }
}

impl From<serde_json::Error> for ChunkStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialize(e)
    }
}

impl From<std::io::Error> for ChunkStoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Cache(e)
    }
}
