use super::ids::{HostId, SandboxId, SessionId, SnapshotId, TeleportId};
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
    pub const fn can_transition_to(self, target: Self) -> bool {
        use TeleportPhase::*;
        match self {
            Admitted => matches!(target, Captured | RollingBack),
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
                    (Admitted, Captured)
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
