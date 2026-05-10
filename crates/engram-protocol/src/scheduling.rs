use engram_core::{HostId, SessionId};
use serde::{Deserialize, Serialize};

/// Coordinator -> host: take this session.
///
/// ADR 0005 retired `repo` + `branch` (the platform doesn't run any
/// git operations) and dropped the unused `restore_from_blob` field.
/// Stage 6 will reintroduce a `RestoreFrom { Hot, Cold, ... }` enum
/// for cross-host cold resume — a separate `Frame::Request::CopyBlobToLocal`
/// is what actually drives that path; `AssignSession` stays narrow.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssignSession {
    pub session_id: SessionId,
    pub host_id: HostId,
    /// OCI manifest digest the session boots from. Used by the host's
    /// image cache to pull the right rootfs.
    pub image_version: String,
}

/// Coordinator -> host: drop this session (e.g. migrated elsewhere).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevokeSession {
    pub session_id: SessionId,
    pub upload_snapshot: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_session_round_trips() {
        let assign = AssignSession {
            session_id: SessionId::new(),
            host_id: HostId::new(),
            image_version: "warm-test".into(),
        };
        let json = serde_json::to_string(&assign).unwrap();
        let back: AssignSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, assign.session_id);
        assert_eq!(back.host_id, assign.host_id);
        assert_eq!(back.image_version, assign.image_version);
    }

    #[test]
    fn revoke_session_round_trips() {
        let r = RevokeSession {
            session_id: SessionId::new(),
            upload_snapshot: true,
        };
        let back: RevokeSession =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back.session_id, r.session_id);
        assert!(back.upload_snapshot);
    }
}
