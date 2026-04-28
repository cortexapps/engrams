use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{HostId, SessionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Active,
    Idle,
    Completed,
    Failed,
    /// Phase 3d: the host that owned this session went dark and
    /// the dead-host detector cleared `host_id`. Next access picks
    /// a new host (snapshot affinity if any other host has the
    /// snapshot, else cold-tier blob restore) and transitions to
    /// `Active`.
    PendingReassign,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::PendingReassign => "pending_reassign",
        }
    }
}

/// User-facing request to create a session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSpec {
    pub repo: String,
    pub branch: String,
    pub user_id: Option<String>,
    /// Optional override for the warm image to use. If unset, the
    /// coordinator picks the latest `ready` version for the repo.
    pub image_version: Option<String>,
}

/// A persisted session row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub repo: String,
    pub branch: String,
    pub user_id: Option<String>,
    pub status: SessionStatus,
    pub image_version: String,
    pub host_id: Option<HostId>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_serializes_lowercase() {
        let payload = serde_json::to_value(SessionStatus::Active).unwrap();
        assert_eq!(payload, serde_json::json!("active"));
        let parsed: SessionStatus = serde_json::from_str(r#""idle""#).unwrap();
        assert_eq!(parsed, SessionStatus::Idle);
    }

    #[test]
    fn session_status_unknown_string_rejected() {
        let res: Result<SessionStatus, _> = serde_json::from_str(r#""running""#);
        assert!(res.is_err(), "unknown variants must fail to deserialize");
    }

    #[test]
    fn session_status_as_str_matches_serde_form() {
        for s in [
            SessionStatus::Pending,
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::Completed,
            SessionStatus::Failed,
        ] {
            let via_serde = serde_json::to_string(&s).unwrap();
            // strip the surrounding quotes from the JSON string
            let trimmed = via_serde.trim_matches('"');
            assert_eq!(s.as_str(), trimmed, "as_str must match wire format");
        }
    }

    #[test]
    fn session_roundtrips_through_json() {
        let original = Session {
            id: SessionId::new(),
            repo: "cortex/api".into(),
            branch: "main".into(),
            user_id: Some("u1".into()),
            status: SessionStatus::Active,
            image_version: "warm-20260101T000000Z".into(),
            host_id: Some(HostId::new()),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
        };
        let blob = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.repo, original.repo);
        assert_eq!(back.branch, original.branch);
        assert_eq!(back.user_id, original.user_id);
        assert_eq!(back.status, original.status);
        assert_eq!(back.image_version, original.image_version);
        assert_eq!(back.host_id, original.host_id);
    }
}
