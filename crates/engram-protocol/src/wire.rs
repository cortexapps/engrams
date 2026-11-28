//! Payloads and protocol markers for the coordinator-to-host gRPC boundary.
//! Ordinary policy has an explicit wire type in [`crate::ordinary_policy`].
//! Managed RPCs use their own marker and authenticated transport.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ordinary coordinator-to-host payload contract. Hosts advertise this version
/// for registration and placement. Managed RPCs also require their own marker.
pub const WIRE_VERSION: u32 = 38;

/// Exact contract for the additive managed host RPCs.
pub const MANAGED_PROTOCOL_VERSION: u32 = 43;
/// Managed contract marker, independent of the ordinary wire version.
pub const MANAGED_PROTOCOL_METADATA_KEY: &str = "x-engram-managed-protocol";

/// Whether a host-agent build can adopt the microVMs the previous build left
/// running (ADR 0126). Two builds with the same number can reattach each
/// other's VMs; raise it in any change that stops a new build from adopting
/// the previous build's VMs, and an upgrade across it drains every host
/// first. 1: the session disk is a kernel-owned thin volume (ADR 0125); a
/// build that reports nothing counts as 0. 2: the uffd handler speaks only
/// substrate protocol version 2 and later, a post-copy `DrainDone` carries
/// the hot-set fields, a snapshot create always sends `defer_sync`, and a
/// migration item always carries its hash. 3: a session disk hydrates
/// behind the running guest from a ublk device the host-agent serves,
/// which an earlier build cannot serve again after a restart.
pub const REATTACH_GENERATION: u32 = 3;

/// Ordinary wire version on coordinator-to-host requests. Ordinary HTTP/2
/// tolerates an absent header; authenticated managed requests require it.
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

/// Marker prefix for the host-agent's harness-spawn rejection message
/// (ADR 0116 B-D4, the [`WIRE_SKEW_STATUS_PREFIX`] precedent). The host
/// returns a `failed_precondition` status whose message starts with this
/// prefix and carries the guest's `io::ErrorKind` Debug string in a fixed
/// `kind=<Kind>` token; the coord-side client matches the prefix to map
/// the status back to a typed `SandboxError::HarnessSpawn` rather than a
/// generic VM error. A coordinator that predates the marker sees an
/// ordinary `failed_precondition` string and behaves exactly as before —
/// no `WIRE_VERSION` bump.
pub const HARNESS_SPAWN_STATUS_PREFIX: &str = "harness_spawn:";

/// Render the host-agent's harness-spawn rejection message. `kind` is the
/// guest `io::ErrorKind` Debug string agentd sent (`"NotFound"`, …);
/// `message` is the human-readable spawn error.
pub fn harness_spawn_message(kind: &str, message: &str) -> String {
    format!("{HARNESS_SPAWN_STATUS_PREFIX} kind={kind} {message}")
}

/// Parse a (kind, message) pair out of a [`harness_spawn_message`].
/// Returns `None` if `msg` isn't a harness-spawn marker (so a regular
/// `failed_precondition` from some other source falls through to the
/// default mapping).
pub fn parse_harness_spawn_message(msg: &str) -> Option<(String, String)> {
    let rest = msg.strip_prefix(HARNESS_SPAWN_STATUS_PREFIX)?.trim();
    let rest = rest.strip_prefix("kind=")?;
    let (kind, message) = rest.split_once(' ')?;
    if kind.is_empty() {
        return None;
    }
    Some((kind.to_string(), message.trim().to_string()))
}

/// Exact failed-precondition marker. It is an outcome, not retirement proof.
pub const MANAGED_RECOVERY_REQUIRED_STATUS: &str = "managed_recovery_required";

/// Marker prefix for the host-agent's "memory image unusable" restore
/// refusal ([`engram_core::SandboxError::MemoryImageUnusable`], the
/// [`HARNESS_SPAWN_STATUS_PREFIX`] precedent). The host returns a
/// `failed_precondition` status whose message starts with this prefix;
/// the coord-side client maps it back to the typed variant, so the resume
/// verb can fall back to a disk-only cold boot. Unlike the harness-spawn
/// marker, it came with a `WIRE_VERSION` bump (v35): an older host refuses
/// the same snapshot untyped, so the exact-match gate fences it out.
pub const MEMORY_IMAGE_UNUSABLE_STATUS_PREFIX: &str = "memory_image_unusable:";

/// Render the host-agent's memory-image refusal message.
pub fn memory_image_unusable_message(message: &str) -> String {
    format!("{MEMORY_IMAGE_UNUSABLE_STATUS_PREFIX} {message}")
}

/// Parse the reason out of a [`memory_image_unusable_message`]. Returns
/// `None` if `msg` isn't that marker.
pub fn parse_memory_image_unusable_message(msg: &str) -> Option<String> {
    let rest = msg
        .strip_prefix(MEMORY_IMAGE_UNUSABLE_STATUS_PREFIX)?
        .trim();
    (!rest.is_empty()).then(|| rest.to_string())
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
    fn memory_image_unusable_message_round_trips() {
        let msg = memory_image_unusable_message("swap restore has no manifest");
        assert_eq!(
            parse_memory_image_unusable_message(&msg),
            Some("swap restore has no manifest".to_string())
        );
        // The core Display is prefix-compatible with the marker.
        let err = engram_core::SandboxError::MemoryImageUnusable("x y".into());
        assert_eq!(
            parse_memory_image_unusable_message(&err.to_string()),
            Some("x y".to_string())
        );
        assert_eq!(parse_memory_image_unusable_message("image not ready"), None);
        assert_eq!(
            parse_memory_image_unusable_message("memory_image_unusable:"),
            None
        );
        assert_eq!(
            parse_memory_image_unusable_message("harness_spawn: kind=NotFound x"),
            None
        );
    }

    #[test]
    fn harness_spawn_message_round_trips() {
        // ADR 0116 B-D4: the host renders the spawn-rejection marker; the
        // coord parses it back to (kind, message). Lossless round-trip so
        // the guest's io::ErrorKind survives to the resume re-plan.
        let msg = harness_spawn_message(
            "NotFound",
            "spawn_harness: spawn \"/opt/engram/dyn/0/bin/h\": No such file or directory",
        );
        assert_eq!(
            parse_harness_spawn_message(&msg),
            Some((
                "NotFound".to_string(),
                "spawn_harness: spawn \"/opt/engram/dyn/0/bin/h\": No such file or directory"
                    .to_string()
            ))
        );
    }

    #[test]
    fn parse_harness_spawn_message_rejects_non_marker_status() {
        // Unmarked / malformed statuses fall through to the default
        // mapping — same discipline as the skew parser.
        assert_eq!(parse_harness_spawn_message("image not ready"), None);
        assert_eq!(parse_harness_spawn_message("harness_spawn: garbage"), None);
        assert_eq!(parse_harness_spawn_message("harness_spawn: kind= x"), None);
        assert_eq!(
            parse_harness_spawn_message("harness_spawn: kind=NotFound"),
            None,
            "a kind with no message is malformed"
        );
        // The skew marker is not a spawn marker and vice versa.
        assert_eq!(
            parse_harness_spawn_message("wire_version skew: host=2 coord=3"),
            None
        );
        assert_eq!(
            parse_wire_skew_message(&harness_spawn_message("NotFound", "x")),
            None
        );
    }
}
