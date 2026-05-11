//! WebSocket-backed `RegistryAuthResolver` for the standalone
//! host-agent.
//!
//! The host-agent doesn't carry `MetadataStore` access or the
//! deployment KEK, so it can't instantiate
//! `engram-oci-auth::PgAuthResolver` directly. Instead, it issues a
//! [`engram_protocol::RequestKind::ResolveRegistryAuth`] RPC over
//! the existing dialer connection; the coord side runs it through
//! its `PgAuthResolver` and replies with plaintext creds (which
//! the host then plugs into the in-flight OCI pull).
//!
//! Credential lifetime on the host: a single RPC round-trip per
//! pull. Nothing persists. ADR 0007.

use std::sync::Arc;

use async_trait::async_trait;
use engram_oci::{BasicCreds, OciError, RegistryAuthResolver};
use engram_protocol::server::HostSession;
use engram_protocol::{RequestKind, ResponseKind};
use tokio::sync::RwLock;

/// Shared, mutable handle to the live `HostSession`. The dialer
/// writes the latest session here when it connects, clears it when
/// the connection drops. The resolver reads it on each `resolve()`
/// call — when `None`, the host isn't connected and we surface that
/// as an OCI error rather than silently treating the pull as
/// anonymous.
pub type SessionHandle = Arc<RwLock<Option<HostSession>>>;

pub struct WsAuthResolver {
    session: SessionHandle,
}

impl WsAuthResolver {
    /// Build a resolver paired with a fresh `SessionHandle`. The
    /// handle is what the dialer writes the session into; the
    /// resolver only reads it. Returning both as a pair makes the
    /// ownership obvious at the call site.
    pub fn new() -> (Self, SessionHandle) {
        let handle: SessionHandle = Arc::new(RwLock::new(None));
        (
            Self {
                session: handle.clone(),
            },
            handle,
        )
    }
}

#[async_trait]
impl RegistryAuthResolver for WsAuthResolver {
    async fn resolve(&self, registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        // Take a snapshot of the session arc rather than holding
        // the read lock across `.await` — request() awaits the
        // coord's reply and we don't want to block reconnection
        // updates that need the write lock.
        let session = match self.session.read().await.clone() {
            Some(s) => s,
            None => {
                return Err(OciError::Distribution(
                    "host-agent not connected to coordinator; cannot resolve OCI auth via WS"
                        .into(),
                ));
            }
        };
        let kind = RequestKind::ResolveRegistryAuth {
            host: registry_host.to_string(),
        };
        match session.request(kind).await {
            Ok(ResponseKind::RegistryAuth { creds: Some(c) }) => Ok(Some(BasicCreds {
                username: c.username,
                password: c.password,
            })),
            Ok(ResponseKind::RegistryAuth { creds: None }) => Ok(None),
            Ok(other) => Err(OciError::Distribution(format!(
                "ResolveRegistryAuth got unexpected response shape: {other:?}"
            ))),
            Err(e) => Err(OciError::Distribution(format!(
                "ResolveRegistryAuth RPC failed: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_without_connected_session_surfaces_clear_error() {
        let (resolver, _handle) = WsAuthResolver::new();
        let err = match resolver.resolve("gcr.io").await {
            Ok(_) => panic!("unconnected host-agent must error, not return None"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("not connected"),
            "error should explain why: {msg}",
        );
    }
}
