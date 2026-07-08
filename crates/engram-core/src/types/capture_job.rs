//! ADR 0081: capture as a durable, host-executed, epoch-fenced job row
//! (`capture_jobs`), dispatched and reported over the heartbeat instead
//! of a connection-coupled RPC stream. A dropped `BuildBaseSnapshot`
//! stream today keeps running detached host-side while the coordinator
//! re-drives from scratch — booting a second capture VM with no
//! anti-affinity and no host-side awareness that the first attempt is
//! stale. This module is the shared, I/O-free shape of that row plus
//! the wire types that cross the heartbeat boundary; the executor and
//! scanner that actually drive it land in a later commit.
//!
//! This commit is purely additive/dormant: nothing constructs or
//! consumes these types yet.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{CaptureJobId, HostId, SnapshotId};
use super::image::{ImageConfig, OciRuntimeDefaults};

/// `capture_jobs.stage`: `assigned -> booting -> warming -> freezing ->
/// done | failed`. The cold-base hit path skips `booting`'s cold-boot
/// half; warm-less images skip `warming`; `freezing` includes the
/// write-through flush (chunks are durable at write per ADR 0078 P1).
/// `done`/`failed` are terminal — every coordinator write to the row is
/// fenced `WHERE ... AND stage NOT IN ('done', 'failed')`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureJobStage {
    Assigned,
    Booting,
    Warming,
    Freezing,
    Done,
    Failed,
}

impl CaptureJobStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::Booting => "booting",
            Self::Warming => "warming",
            Self::Freezing => "freezing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Inverse of [`Self::as_str`] — round-trips the DB/wire
    /// representation. `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "assigned" => Self::Assigned,
            "booting" => Self::Booting,
            "warming" => Self::Warming,
            "freezing" => Self::Freezing,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// `done` or `failed` — the fencing predicate every write applies
    /// (`stage NOT IN ('done', 'failed')`), mirrored here so callers
    /// don't have to restate the two-variant match.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

impl std::fmt::Display for CaptureJobStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `capture_jobs.stage_progress` (JSONB): free-form per-stage progress,
/// units varying by stage (a `[warm]`-hook stage name + log tail during
/// `warming`, a chunking/upload detail during `freezing`, etc.).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureJobProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Rolling tail of the stage's log/hook output (UTF-8-lossy),
    /// mirroring `CaptureProgress::output_tail`'s role for the old
    /// stream-based protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
}

/// A capture job's terminal outcome, carried on the last
/// [`CaptureJobReport`] the host sends. `Done`'s `result_bincode` is
/// the bincode-encoded `CaptureJobResult` (executor-defined artifact —
/// snapshot/manifest identity — landing in a later commit); this crate
/// only needs to move the opaque bytes into `capture_jobs.result_bincode`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CaptureTerminalReport {
    Done {
        result_bincode: Vec<u8>,
    },
    Failed {
        error: String,
        error_stage: String,
        retryable: bool,
    },
}

/// One host -> coordinator report on a capture job's progress,
/// re-advertised every heartbeat (the `CheckpointAdvert`/
/// `acked_checkpoints` pattern verbatim, `engram_protocol::heartbeat`)
/// until `HeartbeatAck.acked_capture_jobs` names it. `epoch` fences
/// every resulting coordinator write against a stale (reassigned-away)
/// attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureJobReport {
    pub job_id: CaptureJobId,
    pub epoch: i64,
    pub stage: CaptureJobStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<CaptureJobProgress>,
    /// Stamped by the capturing host once known (`None` for VZ/Process,
    /// which have no FC `SNAPSHOT_VERSION` concept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fc_snapshot_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<CaptureTerminalReport>,
}

/// One coordinator -> host dispatch on `HeartbeatAck`: "you own this
/// job at this epoch." An unknown-to-the-host assignment (a fresh
/// claim, or a bumped epoch on one it's already running) drives the
/// host to call the claim endpoint for the full `CaptureJobSpec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureJobAssignment {
    pub job_id: CaptureJobId,
    pub epoch: i64,
}

/// The `capture_jobs` row in full — mirrors migration `0095_capture_jobs.sql`
/// column-for-column.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureJobRow {
    pub id: CaptureJobId,
    pub enable_job_id: uuid::Uuid,
    pub image_uri: String,
    /// Content-derived manifest ref of the materialized rootfs (ADR
    /// 0080), rendered as text (`ManifestRef`'s canonical
    /// `<uuid>@v<num>` Display form) — opaque at this layer, parsed by
    /// the executor.
    pub disk_manifest: String,
    /// The ADR 0080 `ImageConfig` this job captures under (refs only;
    /// never resolved secrets — those are resolved fresh at claim
    /// time and never persisted to this row).
    pub image_config: ImageConfig,
    pub oci_defaults: OciRuntimeDefaults,
    pub host_id: HostId,
    /// Fencing token; bumped on every reassignment.
    pub epoch: i64,
    pub stage: CaptureJobStage,
    pub stage_started_at: DateTime<Utc>,
    pub stage_progress: Option<CaptureJobProgress>,
    pub last_progress_at: DateTime<Utc>,
    pub attempts: u32,
    /// Terminal classification, written from the host's terminal
    /// report; `None` while non-terminal.
    pub retryable: Option<bool>,
    pub error: Option<String>,
    pub error_stage: Option<String>,
    /// Stamped by the capturing host; `None` for VZ/Process.
    pub fc_snapshot_version: Option<String>,
    /// The bincode-encoded `CaptureJobResult`, set on `stage == Done`.
    pub result_bincode: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The write-set for [`crate::traits::MetadataStore::insert_capture_job`]
/// — everything needed to create a fresh, `assigned`-stage row. The id,
/// epoch (starts at 1), stage, and timestamps are struck by the store;
/// callers only supply what the job captures and where it's placed.
#[derive(Clone, Debug)]
pub struct NewCaptureJob {
    pub enable_job_id: uuid::Uuid,
    pub image_uri: String,
    pub disk_manifest: String,
    pub image_config: ImageConfig,
    pub oci_defaults: OciRuntimeDefaults,
    pub host_id: HostId,
}

/// The `cold_bases` row (ADR 0081 section B): a content-keyed,
/// boot-to-agentd-ready Full snapshot, reusable across every warm
/// image whose cold-base content key matches (env-agnostic — the
/// warm hook always re-runs against a fresh env on top of it).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColdBaseRow {
    /// `sha256(disk_manifest.content_ref || canonical(image_config.resources)
    /// || fc_snapshot_version || backend_kind)` — computed by the
    /// executor, opaque here.
    pub content_key: String,
    pub snapshot_id: SnapshotId,
    pub disk_manifest: String,
    pub memory_manifest: String,
    pub fc_snapshot_version: String,
    pub captured_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_job_stage_round_trips_every_variant() {
        let variants = [
            CaptureJobStage::Assigned,
            CaptureJobStage::Booting,
            CaptureJobStage::Warming,
            CaptureJobStage::Freezing,
            CaptureJobStage::Done,
            CaptureJobStage::Failed,
        ];
        for v in variants {
            let s = v.as_str();
            assert_eq!(CaptureJobStage::parse(s), Some(v));
            assert_eq!(v.to_string(), s);
        }
        assert_eq!(CaptureJobStage::parse("bogus"), None);
    }

    #[test]
    fn only_done_and_failed_are_terminal() {
        assert!(!CaptureJobStage::Assigned.is_terminal());
        assert!(!CaptureJobStage::Booting.is_terminal());
        assert!(!CaptureJobStage::Warming.is_terminal());
        assert!(!CaptureJobStage::Freezing.is_terminal());
        assert!(CaptureJobStage::Done.is_terminal());
        assert!(CaptureJobStage::Failed.is_terminal());
    }

    #[test]
    fn capture_terminal_report_round_trips_through_json() {
        let done = CaptureTerminalReport::Done {
            result_bincode: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&done).unwrap();
        let back: CaptureTerminalReport = serde_json::from_str(&json).unwrap();
        match back {
            CaptureTerminalReport::Done { result_bincode } => {
                assert_eq!(result_bincode, vec![1, 2, 3]);
            }
            other => panic!("expected Done, got {other:?}"),
        }

        let failed = CaptureTerminalReport::Failed {
            error: "boom".into(),
            error_stage: "warming".into(),
            retryable: true,
        };
        let json = serde_json::to_string(&failed).unwrap();
        let back: CaptureTerminalReport = serde_json::from_str(&json).unwrap();
        match back {
            CaptureTerminalReport::Failed {
                error,
                error_stage,
                retryable,
            } => {
                assert_eq!(error, "boom");
                assert_eq!(error_stage, "warming");
                assert!(retryable);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
