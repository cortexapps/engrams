//! Shared bincode-encoded payload types crossing the coord ↔ host
//! gRPC boundary (ADR 0013).
//!
//! The gRPC service in `host_service.proto` carries these inside
//! `bytes` fields — bincode round-trips Rust types without needing
//! a proto mirror for every nested struct (SandboxSpec,
//! SnapshotMetadata, SessionEgressPolicy, AgentSpec, etc. stay
//! authoritative). Coord + host deploy together (OSS image tag →
//! engrams-internal Helm pin) so bincode skew can't happen across
//! versions.
//!
//! Pre-0013 this file held the full `Frame`/`RequestKind`/`ResponseKind`/
//! `NotifyKind` enum surface for the bincode-over-WS protocol. ADR 0013
//! retired that protocol; what's left here is just the shared shapes.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Version stamp shipped from the host-agent in
/// `POST /api/hosts/register`. Coord compares against its own and
/// tolerates mismatches with a warning — gRPC carries its own
/// schema discipline so this is informational, not a hard gate.
///
/// Bumped on every bincode-payload-incompatible change (new field
/// on `SandboxSpec`, etc.). Coord + host deploy together so
/// mismatched versions are a misconfiguration, not a normal state.
pub const WIRE_VERSION: u32 = 2;

/// Wire-friendly mirror of [`engram_core::types::sandbox::ExecRequest`].
///
/// Defined here so the gRPC payload bincode roundtrips cleanly via
/// a stable shape — pinning the wire schema means a future change to
/// the in-process type doesn't silently change the wire format.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireExecRequest {
    pub command: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Wall-clock timeout in milliseconds. `None` disables.
    pub timeout_ms: Option<u64>,
}

impl WireExecRequest {
    pub fn from_engine(req: engram_core::types::sandbox::ExecRequest) -> Self {
        Self {
            command: req.command,
            stdin: req.stdin,
            env: req.env,
            workdir: req.workdir,
            timeout_ms: req.timeout.map(|d| d.as_millis() as u64),
        }
    }

    pub fn into_engine(self) -> engram_core::types::sandbox::ExecRequest {
        engram_core::types::sandbox::ExecRequest {
            command: self.command,
            stdin: self.stdin,
            env: self.env,
            workdir: self.workdir,
            timeout: self.timeout_ms.map(Duration::from_millis),
        }
    }
}

/// Wire-side mirror of `engram_host_agent::orphan_reap::ReapStats`.
/// Defined here so `engram-protocol` doesn't drag a dep on the
/// host-agent crate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WireReapStats {
    pub files_scanned: u64,
    pub files_deleted: u64,
    pub bytes_freed: u64,
    pub files_skipped_unparseable: u64,
    pub files_skipped_too_young: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_exec_request_round_trips_through_engine_type() {
        let original = engram_core::types::sandbox::ExecRequest {
            command: vec!["echo".into(), "hi".into()],
            stdin: Some(b"input".to_vec()),
            env: HashMap::from([("A".into(), "B".into())]),
            workdir: Some("/tmp".into()),
            timeout: Some(Duration::from_millis(5_500)),
        };
        let wire = WireExecRequest::from_engine(original.clone());
        let bytes = bincode::serialize(&wire).unwrap();
        let back: WireExecRequest = bincode::deserialize(&bytes).unwrap();
        let recovered = back.into_engine();
        assert_eq!(recovered.command, original.command);
        assert_eq!(recovered.stdin, original.stdin);
        assert_eq!(recovered.env, original.env);
        assert_eq!(recovered.workdir, original.workdir);
        assert_eq!(recovered.timeout, original.timeout);
    }

    #[test]
    fn wire_reap_stats_round_trips() {
        let stats = WireReapStats {
            files_scanned: 12,
            files_deleted: 5,
            bytes_freed: 1024,
            files_skipped_unparseable: 1,
            files_skipped_too_young: 2,
        };
        let bytes = bincode::serialize(&stats).unwrap();
        let back: WireReapStats = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.files_scanned, stats.files_scanned);
        assert_eq!(back.files_deleted, stats.files_deleted);
        assert_eq!(back.bytes_freed, stats.bytes_freed);
    }
}
