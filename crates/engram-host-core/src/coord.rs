//! The coordinator control-plane seam (ADR 0098 Phase 2).
//!
//! The host-agent's lifecycle flows make two decision-feeding calls into
//! the coordinator: `sandbox_ownership` (the export-TTL ownership check)
//! and `sandbox_owner` (the unknown-binding reconcile lookup). Both sit
//! behind [`CoordControlPlane`]; the concrete reqwest client
//! (`HttpCoordClient`, in `engram-host-agent`) implements it, and the
//! boundary simulator answers it from the real coordinator handler cores.
//!
//! The other host→coord HTTP routes (register/heartbeat/forge/upload/…)
//! are NOT part of this seam — they are cooperative data/control traffic
//! the simulator never scripts, so they stay on the concrete
//! `HttpCoordClient` inherent surface.

use async_trait::async_trait;
use engram_core::{HostId, SandboxId, SessionId};

/// Portable mirror of the host-agent's former `CoordClientError`. The
/// reqwest-specific `Transport(reqwest::Error)` variant is flattened to a
/// `String` here so the type is buildable on macOS and inside the sim
/// (which never touches reqwest); the concrete client still logs the
/// `is_connect`/`is_timeout` transport breakdown at the construction site
/// before mapping into `Transport(e.to_string())`. `Display` text is
/// byte-identical to the pre-extraction error.
#[derive(Debug)]
pub enum CoordError {
    Transport(String),
    Http {
        status: u16,
        body: String,
        what: &'static str,
    },
    Decode {
        error: String,
        what: &'static str,
    },
}

impl std::fmt::Display for CoordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport error: {e}"),
            Self::Http { status, body, what } => {
                write!(f, "{what} returned HTTP {status}: {body}")
            }
            Self::Decode { error, what } => write!(f, "{what} JSON decode failed: {error}"),
        }
    }
}

impl std::error::Error for CoordError {}

/// The two decision-feeding coordinator calls the host-agent's lifecycle
/// flows make. See the module doc for why this is a focused trait and not
/// the whole HTTP surface.
#[async_trait]
pub trait CoordControlPlane: Send + Sync {
    /// GET ownership — Flow E export-TTL check. `Ok(true)` = coord still
    /// binds this sandbox to the session (abort the export, un-pause);
    /// `Ok(false)` = ownership moved on; `Err` = coord unreachable.
    async fn sandbox_ownership(
        &self,
        host_id: HostId,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<bool, CoordError>;

    /// GET owner — Flow C unknown-binding lookup: "does ANY session own
    /// this sandbox on me?" Returns the owning session id so the reconcile
    /// loop can repopulate its binding table.
    async fn sandbox_owner(
        &self,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<Option<SessionId>, CoordError>;
}
