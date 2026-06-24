//! Error enums for the four trait domains.
//!
//! Each implementation crate defines its own concrete error type as needed
//! and converts into one of these enums at the trait boundary. We avoid
//! `thiserror`/`anyhow` per project convention — errors are explicit
//! enums with hand-written `Display` and `Error` impls.

use std::error::Error as StdError;
use std::fmt;

/// Boxed source error for opaque underlying errors from SDK clients.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

// ---------- BackendError (CloudBackend) ----------

#[derive(Debug)]
pub enum BackendError {
    /// The requested operation is not supported by this backend
    /// (e.g. provisioning hosts on a static-fleet backend).
    NotSupported(&'static str),
    /// The cloud SDK or metadata server returned an error.
    Sdk(BoxError),
    /// Configuration was invalid or missing.
    Config(String),
    /// Unexpected response shape from a remote service.
    Protocol(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported(op) => write!(f, "operation not supported: {op}"),
            Self::Sdk(e) => write!(f, "cloud sdk error: {e}"),
            Self::Config(msg) => write!(f, "cloud config error: {msg}"),
            Self::Protocol(msg) => write!(f, "cloud protocol error: {msg}"),
        }
    }
}

impl StdError for BackendError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sdk(e) => Some(&**e),
            _ => None,
        }
    }
}

// ---------- MetaError (MetadataStore) ----------

#[derive(Debug)]
pub enum MetaError {
    NotFound,
    Conflict(String),
    Db(BoxError),
    Migration(String),
    Serialization(String),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "row not found"),
            Self::Conflict(msg) => write!(f, "metadata conflict: {msg}"),
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::Migration(msg) => write!(f, "migration error: {msg}"),
            Self::Serialization(msg) => write!(f, "metadata serialization error: {msg}"),
        }
    }
}

impl StdError for MetaError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Db(e) => Some(&**e),
            _ => None,
        }
    }
}

// ---------- SandboxError (SandboxBackend) ----------

#[derive(Debug)]
pub enum SandboxError {
    NotFound,
    AlreadyExists,
    LimitExceeded(String),
    /// Underlying VM tooling (Firecracker, KVM, etc.) failure.
    Vm(BoxError),
    /// Invalid spec passed to create/exec.
    InvalidSpec(String),
    /// Snapshot or restore failed.
    Snapshot(String),
    Io(std::io::Error),
    Timeout,
    /// ADR 0015 M5: no host has prefetched the image referenced by
    /// this session yet. Carries the manifest digest the scheduler
    /// was looking for so the API surface can render a hint about
    /// which artifact to wait on.
    ImageNotReady(String),
    /// ADR 0015 M3: the sandbox once existed but its owning host is
    /// no longer reachable (heartbeat-loss confirmed, dead_host
    /// detector fired, or operator-pause TTL elapsed). Distinguishes
    /// "host went away mid-session" (410 Gone) from "we never knew
    /// this sandbox" (404 NotFound) so callers can tell a transient
    /// routing miss from a permanent ownership lapse.
    HostLost,
    /// ADR 0050 C: a TRANSIENT, retryable failure reaching the host —
    /// the gRPC channel was `Unavailable` (lazy connect failed, or the
    /// channel was evicted mid-call), typically a freshly-scaled host
    /// whose server isn't serving this pod yet. Distinct from
    /// `Vm` (the host answered with a real VM error) and `HostLost`
    /// (the host is permanently gone): the call site retries this.
    Unavailable(String),
    /// Issue #229: the host-agent refused the RPC because its bincode
    /// `WIRE_VERSION` differs from the coordinator's — a mixed-version
    /// fleet during a non-atomic rolling deploy (coord pods finish in
    /// ~1 min, the host DaemonSet rolls over ~20 min). The host detected
    /// the skew at the RPC boundary and rejected the request BEFORE any
    /// bincode decode, so the failure surfaces as an explicit, retryable
    /// version mismatch instead of a misleading "invalid sandbox spec"
    /// decode error. Retryable (503): the scheduler drains off the
    /// stale host as the roll completes, so a retry lands on a matching
    /// host.
    WireSkew {
        host: u32,
        coord: u32,
    },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "sandbox not found"),
            Self::AlreadyExists => write!(f, "sandbox already exists"),
            Self::LimitExceeded(msg) => write!(f, "sandbox limit exceeded: {msg}"),
            Self::Vm(e) => write!(f, "sandbox vm error: {e}"),
            Self::InvalidSpec(msg) => write!(f, "invalid sandbox spec: {msg}"),
            Self::Snapshot(msg) => write!(f, "snapshot error: {msg}"),
            Self::Io(e) => write!(f, "sandbox io error: {e}"),
            Self::Timeout => write!(f, "sandbox operation timed out"),
            Self::ImageNotReady(digest) => write!(
                f,
                "image with manifest digest {digest} has not been prefetched by any host yet"
            ),
            Self::HostLost => write!(f, "sandbox host is no longer reachable"),
            Self::Unavailable(msg) => write!(f, "sandbox host temporarily unavailable: {msg}"),
            Self::WireSkew { host, coord } => write!(
                f,
                "wire_version skew: host={host} coord={coord} (mixed-version fleet \
                 during a rolling deploy); retry — the scheduler drains stale hosts"
            ),
        }
    }
}

impl StdError for SandboxError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Vm(e) => Some(&**e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SandboxError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------- BlobError (BlobStorage) ----------

#[derive(Debug)]
pub enum BlobError {
    /// Key does not exist (404 / NoSuchKey / fs ENOENT).
    NotFound,
    /// Underlying SDK or HTTP error.
    Sdk(BoxError),
    /// Configuration was invalid or missing (no creds, bad endpoint).
    Config(String),
    /// Backend returned an unexpected response shape.
    Protocol(String),
    /// Local filesystem error (only the `local` backend; SDK backends
    /// fold IO errors into `Sdk`).
    Io(std::io::Error),
}

impl fmt::Display for BlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "blob not found"),
            Self::Sdk(e) => write!(f, "blob sdk error: {e}"),
            Self::Config(msg) => write!(f, "blob config error: {msg}"),
            Self::Protocol(msg) => write!(f, "blob protocol error: {msg}"),
            Self::Io(e) => write!(f, "blob io error: {e}"),
        }
    }
}

impl StdError for BlobError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Sdk(e) => Some(&**e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BlobError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------- SecretError (SecretStore) ----------

#[derive(Debug)]
pub enum SecretError {
    /// The configured backend is unreachable / failed.
    Backend(BoxError),
    /// The supplied `ref` (or namespacing-derived key) was malformed
    /// for this backend.
    InvalidRef(String),
    /// Backend returned an unexpected response shape.
    Protocol(String),
    /// Backend declined to authenticate this caller.
    Unauthorized(String),
    /// Backend reported the secret exists but the value couldn't be
    /// decoded (e.g. binary secret retrieved but UTF-8 expected).
    BadValue(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "secret backend error: {e}"),
            Self::InvalidRef(r) => write!(f, "invalid secret ref: {r}"),
            Self::Protocol(m) => write!(f, "secret backend protocol error: {m}"),
            Self::Unauthorized(m) => write!(f, "secret backend unauthorized: {m}"),
            Self::BadValue(m) => write!(f, "secret value not usable: {m}"),
        }
    }
}

impl StdError for SecretError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Backend(e) => Some(&**e),
            _ => None,
        }
    }
}

// ---------- IntegrationError (ADR 0056 Integration trait) ----------

#[derive(Debug)]
pub enum IntegrationError {
    /// The provider backend (HTTP API / SDK) failed.
    Backend(BoxError),
    /// The provider declined to authenticate (bad app key, missing
    /// installation, expired/insufficient credential).
    Unauthorized(String),
    /// The repo, installation, or resource does not exist.
    NotFound(String),
    /// Backend returned an unexpected response shape.
    Protocol(String),
    /// The provider rejected the request semantically (HTTP 422) — e.g. a
    /// mint scoped to a permission the installation doesn't grant.
    Rejected(String),
    /// The request args / resource reference was malformed.
    InvalidSpec(String),
    /// The integration does not implement this operation (e.g. an
    /// inject-source provider has no `mint_credential`).
    Unsupported,
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "integration backend error: {e}"),
            Self::Unauthorized(m) => write!(f, "integration unauthorized: {m}"),
            Self::NotFound(m) => write!(f, "integration resource not found: {m}"),
            Self::Protocol(m) => write!(f, "integration protocol error: {m}"),
            Self::Rejected(m) => write!(f, "integration action rejected: {m}"),
            Self::InvalidSpec(m) => write!(f, "invalid integration request: {m}"),
            Self::Unsupported => write!(f, "operation not supported by this integration"),
        }
    }
}

impl StdError for IntegrationError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Backend(e) => Some(&**e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn io_err() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "denied")
    }

    #[test]
    fn sandbox_io_from_conversion_preserves_kind() {
        let sandbox: SandboxError = io_err().into();
        match sandbox {
            SandboxError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::PermissionDenied),
            other => panic!("expected Io, got {other}"),
        }
    }

    #[test]
    fn meta_db_source_chain_exposes_box_error() {
        let inner: BoxError = Box::new(io::Error::other("boom"));
        let err = MetaError::Db(inner);
        let src = err.source().expect("source present");
        let downcast = src.downcast_ref::<io::Error>().expect("io::Error source");
        assert_eq!(downcast.to_string(), "boom");
    }

    #[test]
    fn variants_with_no_source_return_none() {
        assert!(MetaError::NotFound.source().is_none());
        assert!(MetaError::Conflict("x".into()).source().is_none());
        assert!(MetaError::Migration("m".into()).source().is_none());
        assert!(MetaError::Serialization("s".into()).source().is_none());
        assert!(SandboxError::NotFound.source().is_none());
        assert!(SandboxError::Timeout.source().is_none());
        assert!(BackendError::NotSupported("op").source().is_none());
        assert!(BackendError::Config("c".into()).source().is_none());
    }

    #[test]
    fn display_messages_include_context() {
        assert_eq!(
            BackendError::NotSupported("provision_host").to_string(),
            "operation not supported: provision_host",
        );
        assert_eq!(MetaError::NotFound.to_string(), "row not found");
        assert!(SandboxError::InvalidSpec("bad cpu".into())
            .to_string()
            .contains("bad cpu"));
    }
}
