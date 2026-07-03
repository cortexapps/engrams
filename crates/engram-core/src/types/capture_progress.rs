//! Capture-time progress + failure taxonomy for base-snapshot capture
//! (issue #539). Crosses the coord↔host boundary as a bincode `bytes`
//! payload inside the streaming `BuildBaseSnapshot` RPC (see
//! `host_service.proto`'s `CaptureProgress`/`CaptureFailed` messages) and
//! is persisted onto the `enable_jobs` row so an operator can read a
//! `[warm]` hook's live stage + failing stage + output tail without host
//! log access.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which phase of `build_base_snapshot` a [`CaptureProgress`] event was
/// emitted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePhase {
    /// Booting the capture VM to agentd-ready.
    Boot,
    /// Running the image's `[warm]` hook (only entered when the image
    /// declares one).
    Warm,
    /// Pausing + flushing disk + chunking memory into the portable
    /// snapshot artifact.
    Snapshot,
}

impl CapturePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Warm => "warm",
            Self::Snapshot => "snapshot",
        }
    }
}

/// One named `[warm]`-hook stage, as reported by the progress protocol's
/// `start`/`done` sentinel lines (or implicitly closed by the next
/// `start`, or left open on a failure). Persisted in `enable_jobs.warm_stages`
/// (JSONB array) as the stage history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarmStageRecord {
    pub name: String,
    pub started_at: DateTime<Utc>,
    /// `None` while the stage is still open (either currently running, or
    /// abandoned by a capture that failed before closing it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: WarmStageOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarmStageOutcome {
    /// Still running (no `done`/next-`start`/failure observed yet).
    Running,
    /// Closed cleanly — an explicit `done` line, or implicitly closed by
    /// the next stage's `start`.
    Done,
    /// The capture failed while this stage was open (stall, stage
    /// deadline, global timeout, or the hook exited non-zero).
    Failed,
}

/// A liveness/progress event emitted during `build_base_snapshot`.
/// Streamed coord-ward at least every 30 s (host keepalive) so an
/// operator watching a live capture sees a stage + output tail no
/// staler than that, even during a silent (but not yet stalled) wait.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureProgress {
    pub phase: CapturePhase,
    /// Current `[warm]`-hook stage name while `phase == Warm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_stage: Option<String>,
    /// Last `msg=`/heartbeat free text, or a synthesized keepalive note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Rolling last 16 KiB of the hook's combined stdout+stderr
    /// (UTF-8-lossy). Full replacement each event, not a diff.
    #[serde(default)]
    pub output_tail: String,
    /// Stage history so far (closed + the currently-open stage).
    #[serde(default)]
    pub warm_stages: Vec<WarmStageRecord>,
}

/// Taxonomy of ways a base-snapshot capture can fail. Splits the
/// historically-ambiguous "hook exited with status None" into a
/// deterministic in-guest kill (`WarmGlobalTimeout`) vs. a transport
/// failure the exec stream died early on (`WarmExecTransport`) — the
/// only kind the enable-scanner treats as retryable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureFailureKind {
    /// The `[warm]` hook exited with a non-zero (but present) status.
    WarmExitNonZero,
    /// A conforming hook (has emitted >=1 progress line) went silent —
    /// no stdout/stderr bytes and no progress line — for `stall_secs`.
    WarmStall,
    /// The current stage exceeded its declared `deadline_secs`.
    WarmStageDeadline,
    /// The hook's `WarmConfig::timeout()` elapsed; agentd SIGKILLed the
    /// child in-guest and reported `Exit(None)`.
    WarmGlobalTimeout,
    /// `Exit(None)` — the child died to a signal — observed WELL BEFORE
    /// the hook's `timeout()` budget elapsed, so agentd's timeout
    /// backstop is not the cause (e.g. a guest OOM kill at minute 2 of a
    /// 55-minute budget). Distinct from `WarmGlobalTimeout` so an
    /// operator isn't steered to raise `timeout_secs` for a failure
    /// `timeout_secs` had nothing to do with.
    WarmKilled,
    /// The exec stream ended (or errored) before an `Exit` event arrived
    /// — a transport-level failure (vsock/gRPC connection lost), not a
    /// deterministic hook outcome. The only retryable kind.
    WarmExecTransport,
    /// The post-warm snapshot step (pause/flush/chunk/upload) failed.
    SnapshotFailed,
}

impl CaptureFailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WarmExitNonZero => "warm_exit_non_zero",
            Self::WarmStall => "warm_stall",
            Self::WarmStageDeadline => "warm_stage_deadline",
            Self::WarmGlobalTimeout => "warm_global_timeout",
            Self::WarmKilled => "warm_killed",
            Self::WarmExecTransport => "warm_exec_transport",
            Self::SnapshotFailed => "snapshot_failed",
        }
    }

    /// Only a mid-stream transport death is eligible for the
    /// enable-scanner's attempts-budget retry; every other kind is a
    /// deterministic outcome that retrying cannot fix (bail fast).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::WarmExecTransport)
    }

    /// Inverse of [`Self::as_str`] — round-trips the snake_case wire/DB
    /// representation. `None` for anything unrecognized (a future kind a
    /// newer host emits that this coordinator doesn't know about yet);
    /// callers fall back to a generic classification rather than erroring.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "warm_exit_non_zero" => Self::WarmExitNonZero,
            "warm_stall" => Self::WarmStall,
            "warm_stage_deadline" => Self::WarmStageDeadline,
            "warm_global_timeout" => Self::WarmGlobalTimeout,
            "warm_killed" => Self::WarmKilled,
            "warm_exec_transport" => Self::WarmExecTransport,
            "snapshot_failed" => Self::SnapshotFailed,
            _ => return None,
        })
    }
}

impl std::fmt::Display for CaptureFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A terminal, structured base-snapshot capture failure — the payload
/// carried by `SandboxError::CaptureFailed`, the gRPC `CaptureFailed`
/// stream frame, and (via the coordinator) `enable_jobs.error` +
/// `warm_stage`/`output_tail`. Replaces the old "(see host logs for
/// stderr)" dead end: the failing stage and the hook's last 16 KiB of
/// combined output travel with the error, no host-log access required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureFailure {
    pub kind: CaptureFailureKind,
    /// The `[warm]`-hook stage that was open when the failure occurred
    /// (`None` outside `phase == Warm`, e.g. a snapshot-step failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Rolling last 16 KiB of the hook's combined stdout+stderr at the
    /// moment of failure.
    #[serde(default)]
    pub tail: String,
    pub message: String,
}

impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.stage {
            Some(stage) => write!(
                f,
                "base-snapshot capture failed ({}) at [warm] stage `{stage}`: {}",
                self.kind, self.message
            ),
            None => write!(
                f,
                "base-snapshot capture failed ({}): {}",
                self.kind, self.message
            ),
        }
    }
}
