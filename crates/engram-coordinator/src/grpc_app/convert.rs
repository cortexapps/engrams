//! Dumb, total field copies between the axum/serde-facing api types
//! (`crate::api::sessions`, `engram_core::types::Session`) and the
//! generated proto types (`engram_protocol::app`). No business logic
//! lives here — every function is a mechanical field-by-field copy.
//!
//! The proto `Session` is the JSON wire shape minus `user_id` (ADR 0039
//! §2.1: attribution leaves the contract). Timestamps cross as ISO-8601
//! strings, exactly as the JSON wire serializes them; the string-literal
//! status/mode unions stay strings via the core types' `as_str()`.
//!
//! **Totality is enforced in BOTH directions.** Every converter that reads
//! from a source struct must exhaustively destructure it so that a field
//! added later breaks the build here instead of being silently dropped on
//! the floor. Deliberately-unused fields are bound as `field: _` with a
//! comment explaining the intentional drop.

use engram_protocol::app;

use crate::api::sessions::{CreateSessionRequest, ListSessionsResponse, SessionListItem};
use crate::cow_state::CowStateView;
use crate::error::ApiError;
use engram_core::types::session::SessionMode;

/// `engram_core::types::Session` → proto `Session`. Drops `user_id`
/// (off-contract per ADR 0039 §2.1: attribution leaves the contract) and
/// `live_disk_manifest` (internal coord state, not on the wire shape).
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `engram_core::types::Session` without updating this converter,
/// the build will fail here rather than silently drop the new field.
pub(crate) fn session_to_proto(s: &engram_core::types::Session) -> app::Session {
    let engram_core::types::Session {
        id,
        user_id: _, // ADR 0039 §2.1: attribution is off-contract; intentionally dropped.
        status,
        host_id,
        sandbox_id,
        image,
        mode,
        created_at,
        last_active_at,
        live_disk_manifest: _, // Internal coord state (ADR 0016 Phase B); not on the wire shape.
    } = s;
    app::Session {
        id: id.to_string(),
        status: status.as_str().to_string(),
        host_id: host_id.map(|h| h.to_string()),
        sandbox_id: sandbox_id.map(|sb| sb.to_string()),
        image: image.clone(),
        mode: mode.as_str().to_string(),
        // ISO-8601, matching the JSON wire (chrono's Serialize is RFC3339).
        created_at: created_at.to_rfc3339(),
        last_active_at: last_active_at.to_rfc3339(),
    }
}

/// api `SessionListItem` → proto `SessionListItem`. `owner_kind` is a
/// TS-mirror-only field — left unset from Rust (the api type has no such
/// field).
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `SessionListItem` without updating this converter, the build
/// will fail here rather than silently drop the new field.
pub(crate) fn session_list_item_to_proto(item: SessionListItem) -> app::SessionListItem {
    let SessionListItem {
        session,
        owner_email,
        owner_name,
    } = item;
    app::SessionListItem {
        session: Some(session_to_proto(&session)),
        owner_email,
        owner_name,
        owner_kind: None, // TS-mirror-only field; no Rust equivalent in SessionListItem.
    }
}

/// api `ListSessionsResponse` → proto `ListSessionsResponse`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `ListSessionsResponse` without updating this converter, the
/// build will fail here.
pub(crate) fn list_sessions_to_proto(resp: ListSessionsResponse) -> app::ListSessionsResponse {
    let ListSessionsResponse { sessions } = resp;
    app::ListSessionsResponse {
        sessions: sessions
            .into_iter()
            .map(session_list_item_to_proto)
            .collect(),
    }
}

/// proto `CreateSessionRequest` → api `CreateSessionRequest`. The
/// `mode` string parses to [`SessionMode`] (empty defaults to `Agent`,
/// matching the axum `#[serde(default)]`); an unknown mode is a
/// `BadRequest`.
///
/// `harness_secret_id` is read and rejected at the RPC layer (in
/// `grpc_app/session.rs`) before this function is called — if the caller
/// passes a non-empty value they get `Status::Unimplemented` immediately.
/// It is NOT bound in the destructure here (the caller has already handled
/// it); to keep the totality guard intact the proto struct is destructured
/// exhaustively, binding `harness_secret_id: _` as the signal that the
/// caller has taken responsibility for it.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `app::CreateSessionRequest` without updating this converter,
/// the build will fail here rather than silently drop the new field.
pub(crate) fn create_request_from_proto(
    r: app::CreateSessionRequest,
) -> Result<CreateSessionRequest, ApiError> {
    // Totality guard: destructure ALL proto fields. `harness_secret_id` is
    // checked and rejected by the caller (create_session RPC) before this
    // function is reached; bind it as `_` here to acknowledge the drop.
    let app::CreateSessionRequest {
        image_uri,
        mode,
        prompt,
        harness_secret_id: _, // Checked + rejected at RPC layer (Task 13 wires it via SecretService).
        secrets,
    } = r;
    let mode = match mode.as_str() {
        "" | "agent" => SessionMode::Agent,
        "dev_vm" => SessionMode::DevVm,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown session mode {other:?}; expected \"agent\" or \"dev_vm\""
            )))
        }
    };
    let secrets = if secrets.is_empty() {
        None
    } else {
        Some(secrets.into_iter().collect())
    };
    Ok(CreateSessionRequest {
        image: image_uri,
        mode,
        prompt,
        secrets,
    })
}

/// [`CowStateView`] → proto [`CowStateView`] (session.proto).
///
/// Exhaustive destructure below is the totality guard — a new field on
/// `CowStateView` must be handled here or the build fails.
pub(crate) fn cow_state_to_proto(v: &CowStateView) -> app::CowStateView {
    let CowStateView {
        sandbox_id,
        session_id,
        disk_manifest_id,
        disk_manifest_version,
        dirty_chunks,
        dirty_bytes,
        last_flush_at,
        base_chunks,
        base_chunks_local,
        memory_manifest_id,
        memory_manifest_version,
        last_snapshot_at,
    } = v;
    app::CowStateView {
        sandbox_id: sandbox_id.to_string(),
        session_id: session_id.map(|s| s.to_string()),
        disk_manifest_id: disk_manifest_id.clone(),
        disk_manifest_version: *disk_manifest_version,
        dirty_chunks: *dirty_chunks,
        dirty_bytes: *dirty_bytes,
        last_flush_at: last_flush_at.map(|t| t.to_rfc3339()),
        base_chunks: *base_chunks,
        base_chunks_local: *base_chunks_local,
        memory_manifest_id: memory_manifest_id.clone(),
        memory_manifest_version: *memory_manifest_version,
        last_snapshot_at: last_snapshot_at.map(|t| t.to_rfc3339()),
    }
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
