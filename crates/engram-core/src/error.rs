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
