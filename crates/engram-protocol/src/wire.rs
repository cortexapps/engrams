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

/// Version stamp for the bincode payloads crossing the coord ↔ host
/// gRPC boundary. Bumped on every bincode-payload-incompatible change
/// (a new field on `SandboxSpec`, etc.).
///
/// Issue #229: this is NO LONGER advisory. A Helm rollout is not
/// atomic — coord pods finish in ~1 min while a 40-node host DaemonSet
/// rolls over ~20 min — so a mixed-version fleet is a *normal*,
/// transient state, not a misconfiguration. During that window the
/// version is enforced on both ends:
///   - The coordinator stamps every coord→host gRPC request with this
///     value in the [`WIRE_VERSION_METADATA_KEY`] metadata header
///     (via `TraceparentInjector`).
///   - The host-agent's gRPC server rejects a request whose stamped
///     version differs from its own with `failed_precondition` BEFORE
///     any bincode decode, so the caller sees an explicit, retryable
///     `WireSkew` (→ HTTP 503) instead of a misleading 400 "invalid
///     sandbox spec" decode error.
///   - The host reports its version on every heartbeat; the scheduler
///     excludes version-mismatched hosts so a rolling deploy becomes a
///     graceful drain rather than a stream of hard failures.
// v3 (ADR 0056 Phase 3b): added `injects` to `SessionEgressPolicy` (coord→host).
// v4 (ADR 0056 Phase 4b): added `observes`. A mixed-version fleet fails fast on
// the version gate above rather than misparsing the positional bincode payload.
// v5 (ADR 0059): added `graphql_operation`/`graphql_field` to the inject + observe
// entries (+ `success_no_graphql_errors` on observe) for GraphQL operation gating.
// v6: added `capture_env_bincode` to `BuildBaseSnapshotRequest` — the resolved
// capture-time env injected into the `[warm]` hook at base-snapshot capture.
// v7: capture-VM egress for the `[warm]` hook — `WarmConfig` gains a
// `network: Option<NetworkPolicy>` (the `warm_bincode` field of
// `BuildBaseSnapshotRequest`) and `SessionEgressPolicy` gains `allow_all`. The
// host registers a matching egress policy (allow-all or allowlist) for the
// capture VM's guest IP so the warm boot can reach the network.
// v8 (issue #539): `BuildBaseSnapshot` becomes server-streaming
// (`BuildBaseSnapshotEvent` — `progress`/`done`/`failed`) instead of unary,
// carrying the `[warm]`-hook progress protocol's `CaptureProgress` events
// and a structured `CaptureFailed` terminal frame. Clean break — coord+host
// roll together, no dual-decode ladder.
// v9 (issue #529): `SnapshotMetadata` gains `paused_at: Option<DateTime<Utc>>` —
// the host's exact pause instant, carried over the coord↔host RPC boundary
// (the eviction/snapshot response) so the composed eviction path can resolve
// the `session_events` coherence cursor from it instead of coord wall-clock
// `now` sampled after the capture returns.
// v10 (ADR 0073 / epic #542): bind_session carries binding_epoch; AgentSpec
// carries binding_epoch; the shell-pin RPCs (AcquireShell/ReleaseShell/
// RenewShell) and RehandshakeHarness are deleted; heartbeat gains
// harness_attached. Lockstep coord+host roll, no fallback ladder.
// v11 (issue #548 / ADR 0078): the never-populated `local_snapshots`
// heartbeat mirror is retired end-to-end — the `Heartbeat.local_snapshots`
// wire field, `HostRecord`/`HostHeartbeat` fields, the `hosts.local_snapshots`
// PG column (migration 0091), and the fleet-view proto count (reserved 7).
// Clean break — coord+host roll together; skewed hosts drain off scheduling
// via `host_wire_version_ok` until the host MIG rolls.
// v12 (ADR 0079 / #543): fencing_epoch on session-scoped host RPCs — the
// SandboxIdMessage-shaped lifecycle RPCs (destroy / snapshot family /
// pause / resume) migrate to `FencedSandboxRequest{uuid, fencing_epoch,
// session_id}`; StartAgent / Restore / RestoreBaseForSession gain the
// same (session_id, fencing_epoch) pair. Lockstep coord+host roll.
// v13 (ADR 0080 Phase 2a): `BuildBaseSnapshotRequest` gains
// `capture_egress_bincode` — the capture VM's `[warm]`-hook egress policy
// is assembled COORDINATOR-side (one `SessionEgressPolicy` builder for
// sessions and captures alike) and shipped ready-to-register; the host's
// own `capture_egress_policy` builder (from `WarmConfig.network`) is
// retired, and the host no longer interprets `warm.env`/`warm.network`
// out of `warm_bincode`. Lockstep coord+host roll.
// v14 (ADR 0080 Phase 3b): new server-streaming `MaterializeImage` RPC —
// enable-time host-side materialization of a STANDARD docker/OCI image
// (pull → flatten → pack → chunk) replacing the coordinator's
// engram-artifact pull (`fetch_and_seal_artifact`/`materialize_disk_chunks`
// retired). Carries bincode `Option<ResolvedRegistryAuth>` coord→host and
// bincode `ManifestRef`/`OciRuntimeDefaults` host→coord. A proto RPC
// ADDITION is protobuf-compatible, but the bump makes the deploy posture
// explicit: an enable driven by a v14 coord must never land on a v13 host
// (which would answer `Unimplemented`), so skewed hosts drain off
// scheduling until the MIG rolls. Lockstep coord+host roll.
// v15 (ADR 0084): capture_jobs heartbeat dispatch/reporting (#546) —
// `Heartbeat.capture_job_reports`, `HeartbeatAck.capture_assignments`/
// `acked_capture_jobs`. BuildBaseSnapshot RPC deletion rides this bump
// (removed in the cutover commit).
// v16 (ADR 0095): `SnapshotMetadata.peer_hints` — peer-fill seed addrs
// for the restore destination (bincode field addition). The standing
// `PeerChunkGet` RPC + the heartbeat-ack `warm_peers` field ride this
// bump too (both are independently mixed-roll-safe — proto addition /
// serde-default JSON — but the bump makes the deploy posture explicit:
// a v16 coord never dispatches a peer-hinted restore to a v15 host,
// whose bincode decode would fail loudly). Lockstep coord+host roll.
// v17 (ADR 0100): `WriteFiles` coord↔host RPC and its bincode request /
// response mirrors. Lockstep coord+host roll.
// v18 (ADR 0093 addendum): `MaterializeImageRequest.min_disk_gib` — the
// image's `suggested_disk_gib` now floors the packed ext4 size. Proto
// field addition (mixed-roll-safe: an old host ignores it and packs
// content-sized), bumped so the deploy posture is explicit — an enable
// on a v17 host silently loses the floor.
// v19 (ADR 0103): durable ExecRequest ticket/resume fields and the
// CancelExec coord↔host RPC. Lockstep coord+host roll.
// v20 (ADR 0103 review hardening): ExecFrame carries terminal refusals as
// their own oneof variant instead of collapsing them into Exit(None).
// v21 (ADR 0109): `SessionEgressPolicy.google_adc` tells the host whether
// to expose the session-local Google metadata endpoint. Trailing bincode
// field addition. Lockstep coordinator and host roll.
// v22 (issue: engrams-review outage 2026-08-01): `CredentialMintSource`
// becomes externally tagged. The internally-tagged form (#931) bincode-
// ENCODED as a map but could never DECODE (`deserialize_any`), so every
// `SessionEgressPolicy` carrying a minted inject failed host-side at
// boot — no peer ever decoded the old `Some(mint_source)` bytes. The
// bump makes the mixed-fleet posture explicit. Lockstep coord+host roll.
// v23 (ADR 0109 seam): `SessionEgressPolicy.google_adc` becomes
// `metadata_flavor: Option<MetadataFlavor>`. The boolean conflated "does
// this session need a metadata endpoint?" with "is it Google's?", so a
// second cloud would have needed a second boolean and the proxy would
// have had to decide which one wins. The enum makes it one value, and a
// wildcard-free match makes a new variant a compile error. Bincode field
// REPLACEMENT (not an addition), so the roll is lockstep: a v22 host
// cannot decode a v23 policy.
// v24 (ADR 0106 addendum): `CredentialMintSource` gains the trailing
// `OauthConnector` variant (connector OAuth tokens resolved from the sealed
// credential store) on `SessionEgressPolicy` inject entries and the
// host→coord inject/refresh route. Trailing-variant addition: every existing
// encoding is unchanged, but a v23 host cannot decode a policy carrying the
// new variant, so the roll is lockstep.
// v25 (ADR 0112): `SandboxSpec.swap_mib` — the guest's ephemeral swap
// device size, opt-in per image. Trailing bincode field addition on the
// create/spec wire (and the JSON sidecar, which is serde-defaulted), so
// the roll is lockstep: a v24 host cannot decode a v25 create spec.
// v26 (ADR 0109 Cloud SQL addendum): `SessionEgressPolicy` gains exact
// host-side Cloud SQL tunnel authorities. Trailing bincode field addition;
// coordinator and host roll in lockstep.
pub const WIRE_VERSION: u32 = 26;

/// gRPC metadata (header) key carrying the caller's [`WIRE_VERSION`] on
/// every coord→host request (issue #229). ASCII, lowercase — tonic
/// rejects non-lowercase ASCII metadata keys. Absent on requests from a
/// coordinator that predates this field, which the host-agent tolerates
/// (it can't compare what it didn't receive).
pub const WIRE_VERSION_METADATA_KEY: &str = "x-engram-wire-version";

/// Marker prefix for the host-agent's wire-version-skew rejection
/// message (issue #229). The host returns a `failed_precondition` status
/// whose message starts with this prefix and carries the two versions in
/// a fixed `host=<u32> coord=<u32>` shape; the coord-side client matches
/// the prefix to map the status back to a typed
/// `engram_core::SandboxError::WireSkew` rather than a generic VM error.
pub const WIRE_SKEW_STATUS_PREFIX: &str = "wire_version skew:";

/// Render the host-agent's skew rejection message. `host` is the
/// host-agent's own [`WIRE_VERSION`]; `coord` is the version the
/// coordinator stamped on the request.
pub fn wire_skew_message(host: u32, coord: u32) -> String {
    format!("{WIRE_SKEW_STATUS_PREFIX} host={host} coord={coord}")
}

/// Parse a host/coord version pair out of a [`wire_skew_message`]. Returns
/// `None` if `msg` isn't a skew message (so a regular `failed_precondition`
/// from some other source falls through to the default mapping).
pub fn parse_wire_skew_message(msg: &str) -> Option<(u32, u32)> {
    let rest = msg.strip_prefix(WIRE_SKEW_STATUS_PREFIX)?.trim();
    let host = rest.strip_prefix("host=")?;
    let (host, coord) = host.split_once(' ')?;
    let coord = coord.trim().strip_prefix("coord=")?;
    Some((host.trim().parse().ok()?, coord.trim().parse().ok()?))
}

/// Wire-friendly mirror of [`engram_core::types::sandbox::ExecRequest`].
///
/// Defined here so the gRPC payload bincode roundtrips cleanly via
/// a stable shape — pinning the wire schema means a future change to
/// the in-process type doesn't silently change the wire format.
// `PartialEq`/`Eq`: not on the wire (derives don't touch byte layout, so the
// `wire_golden` pins are unaffected and no `WIRE_VERSION` bump is needed) —
// they let the ADR 0099 H3 `codec_roundtrip` suite assert encode→decode
// identity structurally.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireExecRequest {
    pub command: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Wall-clock timeout in milliseconds. `None` disables.
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub exec_id: Option<String>,
    #[serde(default)]
    pub stdout_offset: Option<u64>,
    #[serde(default)]
    pub stderr_offset: Option<u64>,
    #[serde(default)]
    pub wake: Option<bool>,
}

impl WireExecRequest {
    pub fn from_engine(req: engram_core::types::sandbox::ExecRequest) -> Self {
        Self {
            command: req.command,
            stdin: req.stdin,
            env: req.env,
            workdir: req.workdir,
            timeout_ms: req.timeout.map(|d| d.as_millis() as u64),
            exec_id: req.exec_id,
            stdout_offset: req.stdout_offset,
            stderr_offset: req.stderr_offset,
            wake: req.wake,
        }
    }

    pub fn into_engine(self) -> engram_core::types::sandbox::ExecRequest {
        engram_core::types::sandbox::ExecRequest {
            command: self.command,
            stdin: self.stdin,
            env: self.env,
            workdir: self.workdir,
            timeout: self.timeout_ms.map(Duration::from_millis),
            exec_id: self.exec_id,
            stdout_offset: self.stdout_offset,
            stderr_offset: self.stderr_offset,
            wake: self.wake,
        }
    }
}

/// Wire-friendly mirror of [`engram_core::types::sandbox::WriteFileSpec`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireWriteFileSpec {
    pub path: String,
    pub content: Vec<u8>,
    pub mode: Option<u32>,
}

/// Stable coord↔host payload for a batched file write.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireWriteFilesRequest {
    pub files: Vec<WireWriteFileSpec>,
}

impl WireWriteFilesRequest {
    pub fn from_engine(files: Vec<engram_core::types::sandbox::WriteFileSpec>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|file| WireWriteFileSpec {
                    path: file.path,
                    content: file.content,
                    mode: file.mode,
                })
                .collect(),
        }
    }

    pub fn into_engine(self) -> Vec<engram_core::types::sandbox::WriteFileSpec> {
        self.files
            .into_iter()
            .map(|file| engram_core::types::sandbox::WriteFileSpec {
                path: file.path,
                content: file.content,
                mode: file.mode,
            })
            .collect()
    }
}

/// Wire-friendly mirror of [`engram_core::types::sandbox::WriteFileResult`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireWriteFileResult {
    pub path: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Stable coord↔host response payload for a batched file write.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireWriteFilesResponse {
    pub results: Vec<WireWriteFileResult>,
}

impl WireWriteFilesResponse {
    pub fn from_engine(results: Vec<engram_core::types::sandbox::WriteFileResult>) -> Self {
        Self {
            results: results
                .into_iter()
                .map(|result| WireWriteFileResult {
                    path: result.path,
                    ok: result.ok,
                    error: result.error,
                })
                .collect(),
        }
    }

    pub fn into_engine(self) -> Vec<engram_core::types::sandbox::WriteFileResult> {
        self.results
            .into_iter()
            .map(|result| engram_core::types::sandbox::WriteFileResult {
                path: result.path,
                ok: result.ok,
                error: result.error,
            })
            .collect()
    }
}

/// Wire-side mirror of `engram_host_agent::orphan_reap::ReapStats`.
/// Defined here so `engram-protocol` doesn't drag a dep on the
/// host-agent crate.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
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
            exec_id: Some("exec-roundtrip".into()),
            stdout_offset: Some(7),
            stderr_offset: Some(9),
            wake: Some(true),
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
        assert_eq!(recovered.exec_id, original.exec_id);
        assert_eq!(recovered.stdout_offset, original.stdout_offset);
        assert_eq!(recovered.stderr_offset, original.stderr_offset);
        assert_eq!(recovered.wake, original.wake);
    }

    #[test]
    fn wire_write_files_round_trips_through_engine_types() {
        let files = vec![engram_core::types::sandbox::WriteFileSpec {
            path: "/workspace/.review/instructions.md".into(),
            content: b"review carefully".to_vec(),
            mode: Some(0o640),
        }];
        let request = WireWriteFilesRequest::from_engine(files.clone());
        let bytes = bincode::serialize(&request).unwrap();
        let decoded: WireWriteFilesRequest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.into_engine(), files);

        let results = vec![engram_core::types::sandbox::WriteFileResult {
            path: files[0].path.clone(),
            ok: false,
            error: Some("permission denied".into()),
        }];
        let response = WireWriteFilesResponse::from_engine(results.clone());
        let bytes = bincode::serialize(&response).unwrap();
        let decoded: WireWriteFilesResponse = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.into_engine(), results);
    }

    #[test]
    fn wire_skew_message_round_trips() {
        // Issue #229: the host renders the skew message; the coord parses
        // it back to the two versions. A round-trip must be lossless so a
        // skewed RPC surfaces as a typed `WireSkew`, never a 400.
        let msg = wire_skew_message(2, 3);
        assert_eq!(msg, "wire_version skew: host=2 coord=3");
        assert_eq!(parse_wire_skew_message(&msg), Some((2, 3)));
    }

    #[test]
    fn parse_wire_skew_message_rejects_non_skew_status() {
        // A `failed_precondition` from some other source (no skew prefix)
        // must NOT be misread as a version pair — it falls through to the
        // default error mapping.
        assert_eq!(parse_wire_skew_message("image not ready"), None);
        assert_eq!(parse_wire_skew_message("wire_version skew: garbage"), None);
        assert_eq!(
            parse_wire_skew_message("wire_version skew: host=x coord=3"),
            None
        );
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
