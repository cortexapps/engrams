//! Dumb, total field copies between the axum/serde-facing api types
//! (`crate::api::sessions`, `engram_core::types::Session`) and the
//! generated proto types (`engram_protocol::app`). No business logic
//! lives here — every function is a mechanical field-by-field copy.
//!
//! The proto `Session` is the JSON wire shape minus `user_id` (ADR 0039
//! §2.1: attribution leaves the contract). Timestamps cross as ISO-8601
//! strings, exactly as the JSON wire serializes them; the string-literal
//! status/mode unions stay strings via the core types' `as_str()`.

use engram_protocol::app;

use crate::api::sessions::{CreateSessionRequest, ListSessionsResponse, SessionListItem};
use crate::error::ApiError;
use engram_core::types::session::SessionMode;

/// `engram_core::types::Session` → proto `Session`. Drops `user_id`
/// (off-contract) and `live_disk_manifest` (not on the wire shape).
pub(crate) fn session_to_proto(s: &engram_core::types::Session) -> app::Session {
    app::Session {
        id: s.id.to_string(),
        status: s.status.as_str().to_string(),
        host_id: s.host_id.map(|h| h.to_string()),
        sandbox_id: s.sandbox_id.map(|sb| sb.to_string()),
        image: s.image.clone(),
        mode: s.mode.as_str().to_string(),
        // ISO-8601, matching the JSON wire (chrono's Serialize is RFC3339).
        created_at: s.created_at.to_rfc3339(),
        last_active_at: s.last_active_at.to_rfc3339(),
    }
}

/// api `SessionListItem` → proto `SessionListItem`. `owner_kind` is a
/// TS-mirror-only field — left unset from Rust (the api type has no such
/// field).
pub(crate) fn session_list_item_to_proto(item: SessionListItem) -> app::SessionListItem {
    app::SessionListItem {
        session: Some(session_to_proto(&item.session)),
        owner_email: item.owner_email,
        owner_name: item.owner_name,
        owner_kind: None,
    }
}

/// api `ListSessionsResponse` → proto `ListSessionsResponse`.
pub(crate) fn list_sessions_to_proto(resp: ListSessionsResponse) -> app::ListSessionsResponse {
    app::ListSessionsResponse {
        sessions: resp
            .sessions
            .into_iter()
            .map(session_list_item_to_proto)
            .collect(),
    }
}

/// proto `CreateSessionRequest` → api `CreateSessionRequest`. The
/// `mode` string parses to [`SessionMode`] (empty defaults to `Agent`,
/// matching the axum `#[serde(default)]`); an unknown mode is a
/// `BadRequest`. `harness_secret_id` is intentionally dropped here — its
/// unsealing lands in Task 13 with SecretService; the proto field is
/// accepted on the wire but not yet acted on (gRPC create works for
/// no-harness / pre-authed images until then).
pub(crate) fn create_request_from_proto(
    r: app::CreateSessionRequest,
) -> Result<CreateSessionRequest, ApiError> {
    let mode = match r.mode.as_str() {
        "" | "agent" => SessionMode::Agent,
        "dev_vm" => SessionMode::DevVm,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown session mode {other:?}; expected \"agent\" or \"dev_vm\""
            )))
        }
    };
    let secrets = if r.secrets.is_empty() {
        None
    } else {
        Some(r.secrets.into_iter().collect())
    };
    Ok(CreateSessionRequest {
        image: r.image_uri,
        mode,
        prompt: r.prompt,
        secrets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::session::SessionMode;
    use engram_core::types::{Session, SessionState};
    use engram_core::{HostId, SandboxId, SessionId};

    fn populated_session() -> Session {
        Session {
            id: SessionId::new(),
            user_id: Some("user-123".into()),
            status: SessionState::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "localhost:5001/demo:warm".into(),
            mode: SessionMode::DevVm,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    #[test]
    fn session_round_trips_every_field() {
        let s = populated_session();
        let p = session_to_proto(&s);

        assert_eq!(p.id, s.id.to_string());
        assert_eq!(p.status, "active");
        assert_eq!(p.host_id, Some(s.host_id.unwrap().to_string()));
        assert_eq!(p.sandbox_id, Some(s.sandbox_id.unwrap().to_string()));
        assert_eq!(p.image, s.image);
        assert_eq!(p.mode, "dev_vm");
        assert_eq!(p.created_at, s.created_at.to_rfc3339());
        assert_eq!(p.last_active_at, s.last_active_at.to_rfc3339());
    }

    #[test]
    fn session_optionals_unset_when_none() {
        let mut s = populated_session();
        s.host_id = None;
        s.sandbox_id = None;
        let p = session_to_proto(&s);
        assert_eq!(p.host_id, None);
        assert_eq!(p.sandbox_id, None);
    }

    #[test]
    fn list_item_carries_owner_identity_and_leaves_kind_unset() {
        let item = SessionListItem {
            session: populated_session(),
            owner_email: Some("a@b.com".into()),
            owner_name: Some("Ada".into()),
        };
        let p = session_list_item_to_proto(item);
        assert!(p.session.is_some());
        assert_eq!(p.owner_email.as_deref(), Some("a@b.com"));
        assert_eq!(p.owner_name.as_deref(), Some("Ada"));
        // owner_kind is TS-mirror-only — never set from Rust.
        assert_eq!(p.owner_kind, None);
    }

    #[test]
    fn list_response_maps_all_rows() {
        let resp = ListSessionsResponse {
            sessions: vec![
                SessionListItem {
                    session: populated_session(),
                    owner_email: None,
                    owner_name: None,
                },
                SessionListItem {
                    session: populated_session(),
                    owner_email: Some("x@y.z".into()),
                    owner_name: None,
                },
            ],
        };
        let p = list_sessions_to_proto(resp);
        assert_eq!(p.sessions.len(), 2);
        assert_eq!(p.sessions[1].owner_email.as_deref(), Some("x@y.z"));
    }
}
