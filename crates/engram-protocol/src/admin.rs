//! Host-side admin operations the coord fans out across hosts.
//!
//! Distinct from `HostClient` because these aren't session-lifecycle
//! calls — they're per-host operational primitives the coord drives
//! on a cadence or via `POST /api/admin/*`. Implemented on the host
//! side; dispatched over gRPC via `HostService::ReapMaterializeDir`
//! (ADR 0013).
//!
//! `None` on the gRPC server's admin handler is the supported
//! zero-config default — the server returns `Status::Unimplemented`
//! for any admin RPC, which the coord surfaces as an aggregate
//! failure in the fanout response.

use async_trait::async_trait;

use crate::wire::WireReapStats;

#[async_trait]
pub trait HostAdminHandler: Send + Sync {
    /// Sweep the host's local materialize_dir. `live_disk_manifest_ids`
    /// is the coord's authoritative live set; anything not in it +
    /// older than `min_age_secs` gets reaped.
    async fn reap_materialize_dir(
        &self,
        min_age_secs: u64,
        live_disk_manifest_ids: Vec<uuid::Uuid>,
    ) -> Result<WireReapStats, String>;
}
