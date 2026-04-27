use engram_core::{HostId, SessionId};
use serde::{Deserialize, Serialize};

/// Coordinator -> host: take this session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssignSession {
    pub session_id: SessionId,
    pub host_id: HostId,
    pub repo: String,
    pub branch: String,
    pub image_version: String,
    /// If `Some`, restore from this blob URL instead of starting from
    /// the warm pool.
    pub restore_from_blob: Option<String>,
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
    fn assign_session_round_trips_with_optional_blob_url() {
        let with_blob = AssignSession {
            session_id: SessionId::new(),
            host_id: HostId::new(),
            repo: "r".into(),
            branch: "main".into(),
            image_version: "warm-test".into(),
            restore_from_blob: Some("gs://bkt/snapshots/x".into()),
        };
        let json = serde_json::to_string(&with_blob).unwrap();
        let back: AssignSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, with_blob.session_id);
        assert_eq!(back.host_id, with_blob.host_id);
        assert_eq!(back.repo, with_blob.repo);
        assert_eq!(back.branch, with_blob.branch);
        assert_eq!(back.image_version, with_blob.image_version);
        assert_eq!(back.restore_from_blob, with_blob.restore_from_blob);

        // None must remain None — defending against `skip_serializing_if`
        // creep that would silently coerce missing -> None and hide
        // protocol drift between coordinator and host.
        let cold = AssignSession {
            restore_from_blob: None,
            ..with_blob
        };
        let cold_back: AssignSession =
            serde_json::from_str(&serde_json::to_string(&cold).unwrap()).unwrap();
        assert!(cold_back.restore_from_blob.is_none());
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
