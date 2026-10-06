use super::ids::{HostId, SandboxId, SessionId, SnapshotId, TeleportId};
use super::session::SessionState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeleportKind {
    Snapshot,
    Live,
}
impl TeleportKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Live => "live",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeleportReason {
    RetireHost,
    AdminDrain,
    Ui,
}
impl TeleportReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RetireHost => "retire_host",
            Self::AdminDrain => "admin_drain",
            Self::Ui => "ui",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeleportPhase {
    Admitted,
    Captured,
    Restored,
    Committed,
    Attached,
    Done,
    RollingBack,
    Aborted,
    Failed,
}
impl TeleportPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Captured => "captured",
            Self::Restored => "restored",
            Self::Committed => "committed",
            Self::Attached => "attached",
            Self::Done => "done",
            Self::RollingBack => "rolling_back",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }
}
impl TeleportPhase {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted | Self::Failed)
    }
    pub const fn dest_reserving_phases() -> &'static [&'static str] {
        &["admitted", "captured", "restored", "rolling_back"]
    }
    pub fn reserving_phases_sql() -> String {
        Self::dest_reserving_phases()
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",")
    }
    /// Phases in which the SOURCE still holds a frozen VM or a live export
    /// after the session's own reservation moved to the destination at
    /// commit. The source budget stays reserved until release so a
    /// placement cannot reuse RAM the paused VM still occupies.
    pub const fn source_reserving_phases() -> &'static [&'static str] {
        &["committed", "attached"]
    }
    pub fn source_reserving_phases_sql() -> String {
        Self::source_reserving_phases()
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",")
    }
    pub const fn can_transition_to(self, target: Self) -> bool {
        use TeleportPhase::*;
        match self {
            Admitted => matches!(target, Admitted | Captured | RollingBack),
            Captured => matches!(target, Restored | RollingBack),
            Restored => matches!(target, Committed | RollingBack),
            Committed => matches!(target, Attached | Failed),
            Attached => matches!(target, Done | Failed),
            RollingBack => matches!(target, Aborted | Failed),
            Done | Aborted | Failed => false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeleportRow {
    pub id: TeleportId,
    pub session_id: SessionId,
    pub kind: TeleportKind,
    pub reason: TeleportReason,
    pub phase: TeleportPhase,
    pub source_host_id: HostId,
    pub source_sandbox_id: SandboxId,
    pub dest_host_id: HostId,
    pub dest_sandbox_id: Option<SandboxId>,
    pub pinned_dest: bool,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i32,
    pub snapshot_id: Option<SnapshotId>,
    pub export_id: Option<String>,
    pub live_payload: Option<serde_json::Value>,
    pub attempts: i32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::TeleportPhase::*;
    use super::*;
    const ALL: [TeleportPhase; 9] = [
        Admitted,
        Captured,
        Restored,
        Committed,
        Attached,
        Done,
        RollingBack,
        Aborted,
        Failed,
    ];
    #[test]
    fn phase_table() {
        for from in ALL {
            for to in ALL {
                let expected = matches!(
                    (from, to),
                    (Admitted, Admitted)
                        | (Admitted, Captured)
                        | (Admitted, RollingBack)
                        | (Captured, Restored)
                        | (Captured, RollingBack)
                        | (Restored, Committed)
                        | (Restored, RollingBack)
                        | (Committed, Attached)
                        | (Committed, Failed)
                        | (Attached, Done)
                        | (Attached, Failed)
                        | (RollingBack, Aborted)
                        | (RollingBack, Failed)
                );
                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
        for phase in ALL {
            assert_eq!(
                phase.is_terminal(),
                matches!(phase, Done | Aborted | Failed)
            );
        }
    }
    #[test]
    fn reserving_phases_match() {
        let sql = TeleportPhase::reserving_phases_sql();
        let sql_phases: Vec<_> = sql.split(',').map(|s| s.trim_matches('\'')).collect();
        assert_eq!(sql_phases, TeleportPhase::dest_reserving_phases());
        for phase in ALL {
            assert_eq!(
                sql_phases.contains(&phase.as_str()),
                matches!(phase, Admitted | Captured | Restored | RollingBack)
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeleportOutcome {
    Done,
    Aborted,
    Failed,
}

/// Admission holds the session and destination placement locks until commit.
#[derive(Clone, Debug)]
pub struct TeleportAdmitRequest {
    pub id: TeleportId,
    pub session_id: SessionId,
    pub reason: TeleportReason,
    pub epoch: i64,
    pub candidates: Vec<HostId>,
    pub pinned_dest: Option<HostId>,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i64,
    pub max_open_per_dest: u32,
    pub live_capable: bool,
}
#[derive(Clone, Debug)]
pub enum TeleportAdmitOutcome {
    Admitted(Box<TeleportRow>),
    NoFit,
    SessionNotActive(SessionState),
    Fenced,
}
#[derive(Clone, Debug, Default)]
pub struct TeleportPatch {
    pub dest_sandbox_id: Option<SandboxId>,
    pub snapshot_id: Option<SnapshotId>,
    pub export_id: Option<String>,
    pub live_payload: Option<serde_json::Value>,
    pub kind: Option<TeleportKind>,
    pub error: Option<String>,
}
/// One fenced settlement of a failed move. The session's terminal state
/// (always detaching the binding), the source tombstone, the row's `failed`
/// phase, and both events land in ONE transaction under the session's
/// `current_epoch`, so a stale driver can never write a tombstone or a
/// terminal row after a successor took the lane.
#[derive(Clone, Debug, Default)]
pub struct TeleportSettle {
    pub error: String,
    /// `Some(target)` settles the session; `None` leaves it as it is (a
    /// crashed predecessor or the dead-host sweep already settled it).
    pub session: Option<crate::types::session::SessionState>,
    pub entomb_source: bool,
}
#[derive(Clone, Copy, Debug)]
pub enum SourceRelease {
    DestroyAcked,
    SourceHostGone,
}
