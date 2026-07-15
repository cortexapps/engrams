//! ADR 0080 §C — types for the `MaterializeImage` host RPC: the
//! enable-time pipeline that turns a **standard** docker/OCI image
//! into a chunked bootable ext4 on a host.
//!
//! Sibling of [`crate::types::capture_progress`] (the
//! `BuildBaseSnapshot` shapes): zero-or-more [`MaterializeProgress`]
//! frames stream coord-ward while the host pulls/flattens/packs/
//! chunks, then exactly one terminal outcome — [`MaterializedImage`]
//! or a structured [`MaterializeFailure`] whose
//! [`kind`](MaterializeFailure::kind) drives the enable scanner's
//! retry-vs-bail-fast classification.

use serde::{Deserialize, Serialize};

use super::image::OciRuntimeDefaults;
use super::manifest::ManifestRef;

/// Which pipeline stage a [`MaterializeProgress`] frame reports.
/// Mirrors the `engram-rootfs-materializer` stages one-to-one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializeStage {
    /// Resolving the manifest + streaming layer blobs to scratch.
    Pull,
    /// Whiteout-aware layer application into one tree.
    Flatten,
    /// Layout seal (ADR 0093: replay + freeze; wire name kept).
    Pack,
    /// Chunking the packed ext4 into the content-addressed store.
    Chunk,
}

impl MaterializeStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Flatten => "flatten",
            Self::Pack => "pack",
            Self::Chunk => "chunk",
        }
    }

    /// Inverse of [`Self::as_str`] for the wire's string field.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pull" => Self::Pull,
            "flatten" => Self::Flatten,
            "pack" => Self::Pack,
            "chunk" => Self::Chunk,
            _ => return None,
        })
    }
}

impl std::fmt::Display for MaterializeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One progress frame of an in-flight materialize. The host emits one
/// per stage transition plus a keepalive re-send at least every 30 s
/// (like `BuildBaseSnapshot`'s `CaptureProgress`), so every frame the
/// enable scanner persists doubles as its claim-lease renewal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterializeProgress {
    pub stage: MaterializeStage,
    /// Free-form human detail ("layer 3/7", byte counts, …).
    pub detail: Option<String>,
    /// Chunk-stage progress: 16 MiB windows scanned / total windows of
    /// the ext4 (the denominator counts zero-elided windows too — they
    /// complete instantly, which just makes the bar honest about sparse
    /// regions). `None` outside the chunk stage. Rides the wire as
    /// OPTIONAL proto fields, so a version-skewed peer simply sees
    /// `None` — never a decode error.
    #[serde(default)]
    pub chunks_done: Option<u64>,
    #[serde(default)]
    pub chunks_total: Option<u64>,
}

/// Terminal success of one materialize: everything the enable
/// pipeline stamps onto the `enabled_images` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterializedImage {
    /// Content-derived ref of the packed disk's chunk manifest,
    /// committed durably (host chunk store writes through to
    /// BlobStorage). Content-derived ⇒ a re-materialize of unchanged
    /// content reproduces the SAME ref, so base-snapshot reuse
    /// (`find_enabled_image_by_content`) keeps working.
    pub disk_manifest: ManifestRef,
    /// Dockerfile `ENV` + `WORKDIR` from the image config blob.
    pub oci_defaults: OciRuntimeDefaults,
    /// Digest (`sha256:<hex>`) of the platform-resolved docker image
    /// manifest the host actually materialized — the row's
    /// `manifest_digest` (keeps digest-pinning for capture).
    pub manifest_digest: String,
    /// Size of the packed ext4 in bytes.
    pub ext4_size_bytes: u64,
}

/// Failure class of a [`MaterializeFailure`] — the retry policy hook.
/// Serialized as its snake_case name on the wire (`MaterializeImageFailed.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializeFailureKind {
    /// Another materialize is already running on this host (the ≤1
    /// concurrent gate). Retry re-picks a host.
    Busy,
    /// The scratch filesystem lacks the estimated peak headroom.
    /// Retryable: disk pressure clears (cache sweeps, evictions) and a
    /// retry may land on a different host.
    DiskFull,
    /// The summed compressed layer size exceeds the enable-time cap
    /// (`ENGRAM_MATERIALIZE_MAX_IMAGE_BYTES`). Deterministic — bail fast.
    TooLarge,
    /// Registry-side pull failure (manifest/blob fetch). Transient
    /// blips (connection resets, token expiry races) dominate here —
    /// the enqueue-time manifest probe already rejected bad URIs/auth
    /// — so this retries under the attempts budget.
    Pull,
    /// The image content itself can't materialize (unsupported layer
    /// mediaType, hostile tar, pack failure). Deterministic —
    /// retrying re-downloads the same bytes for nothing.
    Image,
    /// Chunk-store / BlobStorage write failure. Transient (GCS).
    Store,
    /// The RPC stream died before a terminal frame (host rolled,
    /// connection lost). Synthesized CLIENT-side, mirroring
    /// `WarmExecTransport`. Retryable.
    Transport,
    /// Anything else (scratch IO, join errors). Deterministic-unknown —
    /// bail fast so a real bug surfaces instead of burning the budget.
    Internal,
}

impl MaterializeFailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::DiskFull => "disk_full",
            Self::TooLarge => "too_large",
            Self::Pull => "pull",
            Self::Image => "image",
            Self::Store => "store",
            Self::Transport => "transport",
            Self::Internal => "internal",
        }
    }

    /// Inverse of [`Self::as_str`]; unknown strings map to `None` and
    /// the caller defaults to [`Self::Internal`] (bail fast — never
    /// invent retryability for a kind this build doesn't know).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "busy" => Self::Busy,
            "disk_full" => Self::DiskFull,
            "too_large" => Self::TooLarge,
            "pull" => Self::Pull,
            "image" => Self::Image,
            "store" => Self::Store,
            "transport" => Self::Transport,
            "internal" => Self::Internal,
            _ => return None,
        })
    }

    /// Whether the enable scanner should retry via the attempts budget
    /// (true) or flip the job failed on first occurrence (false).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Busy | Self::DiskFull | Self::Pull | Self::Store | Self::Transport
        )
    }
}

impl std::fmt::Display for MaterializeFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured terminal failure of a materialize — carried in
/// `SandboxError::MaterializeFailed` and the wire's
/// `MaterializeImageFailed` frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterializeFailure {
    pub kind: MaterializeFailureKind,
    pub message: String,
}

impl std::fmt::Display for MaterializeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "materialize failed ({}): {}", self.kind, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_strings_round_trip() {
        for s in [
            MaterializeStage::Pull,
            MaterializeStage::Flatten,
            MaterializeStage::Pack,
            MaterializeStage::Chunk,
        ] {
            assert_eq!(MaterializeStage::parse(s.as_str()), Some(s));
        }
        assert_eq!(MaterializeStage::parse("warm"), None);
    }

    #[test]
    fn failure_kind_strings_round_trip_and_unknown_is_none() {
        for k in [
            MaterializeFailureKind::Busy,
            MaterializeFailureKind::DiskFull,
            MaterializeFailureKind::TooLarge,
            MaterializeFailureKind::Pull,
            MaterializeFailureKind::Image,
            MaterializeFailureKind::Store,
            MaterializeFailureKind::Transport,
            MaterializeFailureKind::Internal,
        ] {
            assert_eq!(MaterializeFailureKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MaterializeFailureKind::parse("nope"), None);
    }

    /// The retry policy is the load-bearing contract: transient host /
    /// registry / store conditions retry, deterministic image problems
    /// bail fast.
    #[test]
    fn retryability_matrix() {
        use MaterializeFailureKind::*;
        for k in [Busy, DiskFull, Pull, Store, Transport] {
            assert!(k.is_retryable(), "{k} must retry via the attempts budget");
        }
        for k in [TooLarge, Image, Internal] {
            assert!(!k.is_retryable(), "{k} must bail fast");
        }
    }
}
