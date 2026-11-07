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

    /// A chunk fetch failed for a reason other than a `BlobStorage`
    /// error (for example, a fetch task that panicked). The string
    /// carries the reason.
    Origin(String),

    /// Operation hit an internal invariant that shouldn't happen.
    /// Use for "should never reach here" branches that we still want
    /// to surface rather than panic.
    Internal(String),

    /// A ranged read asked for bytes past the end of the chunk.
    RangeOutOfBounds {
        hash: String,
        offset: u64,
        len: u64,
        size: u64,
    },
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
            Self::RangeOutOfBounds {
                hash,
                offset,
                len,
                size,
            } => write!(
                f,
                "range [{offset}, +{len}) is past the end of chunk {hash} ({size} bytes)"
            ),
        }
    }
}

impl ChunkStoreError {
    /// A copy of this error for another caller that waited on the same
    /// operation. The variant stays the same, so a waiter can tell a
    /// missing chunk from a failed fetch; an error with a source that
    /// cannot be copied keeps its message.
    pub fn duplicate(&self) -> Self {
        fn io(e: &std::io::Error) -> std::io::Error {
            std::io::Error::new(e.kind(), e.to_string())
        }
        match self {
            Self::Blob(b) => Self::Blob(match b {
                BlobError::NotFound => BlobError::NotFound,
                BlobError::Sdk(e) => BlobError::Sdk(e.to_string().into()),
                BlobError::Config(m) => BlobError::Config(m.clone()),
                BlobError::Protocol(m) => BlobError::Protocol(m.clone()),
                BlobError::Io(e) => BlobError::Io(io(e)),
                BlobError::RangeOutOfBounds { offset, len } => BlobError::RangeOutOfBounds {
                    offset: *offset,
                    len: *len,
                },
            }),
            Self::Serialize(e) => Self::Internal(format!("manifest serde: {e}")),
            Self::HashMismatch { expected, actual } => Self::HashMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            },
            Self::VersionConflict {
                manifest_id,
                attempted,
                latest,
            } => Self::VersionConflict {
                manifest_id: *manifest_id,
                attempted: *attempted,
                latest: *latest,
            },
            Self::UnsupportedSchemaVersion {
                found,
                max_supported,
            } => Self::UnsupportedSchemaVersion {
                found: *found,
                max_supported: *max_supported,
            },
            Self::MalformedManifest(m) => Self::MalformedManifest(m.clone()),
            Self::Cache(e) => Self::Cache(io(e)),
            Self::Origin(m) => Self::Origin(m.clone()),
            Self::Internal(m) => Self::Internal(m.clone()),
            Self::RangeOutOfBounds {
                hash,
                offset,
                len,
                size,
            } => Self::RangeOutOfBounds {
                hash: hash.clone(),
                offset: *offset,
                len: *len,
                size: *size,
            },
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
