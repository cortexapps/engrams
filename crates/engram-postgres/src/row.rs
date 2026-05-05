//! Row -> domain-type conversions. Kept separate so the query bodies in
//! `lib.rs` stay readable.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use engram_core::types::session::{HarnessSpec, SessionKind, WorkspaceSpec};
use engram_core::types::{
    EnabledImage, HarnessPack, HostCapacity, HostMetadata, HostRecord, HostStatus, PersistedEvent,
    RegistryCredential, Session, SessionSecrets, SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId, SnapshotId};
use sqlx::postgres::PgRow;
use sqlx::Row;
use uuid::Uuid;

fn col_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
    MetaError::Db(Box::new(e))
}

pub(crate) fn session_from_row(row: &PgRow) -> Result<Session, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let host_id: Option<Uuid> = row.try_get("host_id").map_err(col_err)?;
    let sandbox_id: Option<Uuid> = row.try_get("sandbox_id").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let last_active_at: DateTime<Utc> = row.try_get("last_active_at").map_err(col_err)?;
    let session_kind: String = row.try_get("session_kind").map_err(col_err)?;
    let checkpoint_branch: Option<String> = row.try_get("checkpoint_branch").map_err(col_err)?;
    let image_uri: String = row.try_get("image_uri").map_err(col_err)?;
    let workspace_json: serde_json::Value = row.try_get("workspace").map_err(col_err)?;
    let harness_json: serde_json::Value = row.try_get("harness").map_err(col_err)?;
    let workspace: WorkspaceSpec = serde_json::from_value(workspace_json)
        .map_err(|e| MetaError::Serialization(format!("workspace: {e}")))?;
    let harness: HarnessSpec = serde_json::from_value(harness_json)
        .map_err(|e| MetaError::Serialization(format!("harness: {e}")))?;
    Ok(Session {
        id: SessionId(id),
        user_id: row.try_get("user_id").map_err(col_err)?,
        status: parse_session_status(&status)?,
        host_id: host_id.map(HostId),
        sandbox_id: sandbox_id.map(SandboxId),
        image: image_uri,
        workspace,
        harness,
        session_kind: SessionKind::parse(&session_kind).map_err(MetaError::Serialization)?,
        checkpoint_branch,
        created_at,
        last_active_at,
    })
}

pub(crate) fn host_from_row(row: &PgRow) -> Result<HostRecord, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let cloud_meta: serde_json::Value = row.try_get("cloud_metadata").map_err(col_err)?;
    let total: i32 = row.try_get("capacity_total_gb").map_err(col_err)?;
    let used: i32 = row.try_get("capacity_used_gb").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let last_heartbeat_at: DateTime<Utc> = row.try_get("last_heartbeat_at").map_err(col_err)?;
    let cloud_metadata: HostMetadata =
        serde_json::from_value(cloud_meta).map_err(|e| MetaError::Serialization(e.to_string()))?;
    Ok(HostRecord {
        id: HostId(id),
        hostname: row.try_get("hostname").map_err(col_err)?,
        cloud_metadata,
        capacity: HostCapacity {
            total_gb: total.max(0) as u32,
            used_gb: used.max(0) as u32,
        },
        status: parse_host_status(&status)?,
        last_heartbeat_at,
    })
}

pub(crate) fn snapshot_from_row(row: &PgRow) -> Result<SnapshotRecord, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let session_id: Uuid = row.try_get("session_id").map_err(col_err)?;
    let host_id: Option<Uuid> = row.try_get("host_id").map_err(col_err)?;
    let local_path: Option<String> = row.try_get("local_path").map_err(col_err)?;
    let size_bytes: i64 = row.try_get("size_bytes").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let last_accessed_at: DateTime<Utc> = row.try_get("last_accessed_at").map_err(col_err)?;
    Ok(SnapshotRecord {
        id: SnapshotId(id),
        session_id: SessionId(session_id),
        host_id: host_id.map(HostId),
        local_path: local_path.map(PathBuf::from),
        image_version: row.try_get("image_version").map_err(col_err)?,
        size_bytes: size_bytes.max(0) as u64,
        created_at,
        last_accessed_at,
    })
}

pub(crate) fn persisted_event_from_row(row: &PgRow) -> Result<PersistedEvent, MetaError> {
    let idx: i64 = row.try_get("idx").map_err(col_err)?;
    let kind: String = row.try_get("kind").map_err(col_err)?;
    let payload: serde_json::Value = row.try_get("payload").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    Ok(PersistedEvent {
        idx,
        kind,
        payload,
        created_at,
    })
}

pub(crate) fn registry_credential_from_row(row: &PgRow) -> Result<RegistryCredential, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at").map_err(col_err)?;
    let registry_host: String = row.try_get("registry_host").map_err(col_err)?;
    let auth_kind: String = row.try_get("auth_kind").map_err(col_err)?;
    let auth_config: serde_json::Value = row.try_get("auth_config").map_err(col_err)?;
    // The JSONB payload was written by `serde_json::to_value(&auth)`
    // upstream, so it's already in the `serde(tag = "kind")` shape.
    // We sanity-check that the typed `auth_kind` column matches the
    // JSON's `kind` field — drift here means a write went around the
    // typed Rust path (raw SQL? schema bug?) and we'd rather fail
    // loud than silently dispatch on the wrong variant.
    let json_kind = auth_config
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if json_kind != auth_kind {
        return Err(MetaError::Serialization(format!(
            "registry_credentials row {id}: auth_kind column = {auth_kind:?} but auth_config.kind = {json_kind:?}"
        )));
    }
    let auth = serde_json::from_value(auth_config)
        .map_err(|e| MetaError::Serialization(format!("auth_config decode: {e}")))?;
    Ok(RegistryCredential {
        id,
        registry_host,
        auth,
        created_at,
        updated_at,
    })
}

pub(crate) fn enabled_image_from_row(row: &PgRow) -> Result<EnabledImage, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let last_refreshed_at: DateTime<Utc> = row.try_get("last_refreshed_at").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at").map_err(col_err)?;
    Ok(EnabledImage {
        id,
        image_uri: row.try_get("image_uri").map_err(col_err)?,
        manifest_toml: row.try_get("manifest_toml").map_err(col_err)?,
        manifest_digest: row.try_get("manifest_digest").map_err(col_err)?,
        last_refreshed_at,
        created_at,
        updated_at,
    })
}

pub(crate) fn session_secrets_from_row(row: &PgRow) -> Result<SessionSecrets, MetaError> {
    let session_id: Uuid = row.try_get("session_id").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    Ok(SessionSecrets {
        session_id: SessionId(session_id),
        wrapped_dek: row.try_get("wrapped_dek").map_err(col_err)?,
        nonce: row.try_get("nonce").map_err(col_err)?,
        ciphertext: row.try_get("ciphertext").map_err(col_err)?,
        key_id: row.try_get("key_id").map_err(col_err)?,
        created_at,
    })
}

pub(crate) fn harness_pack_from_row(row: &PgRow) -> Result<HarnessPack, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at").map_err(col_err)?;
    Ok(HarnessPack {
        id,
        name: row.try_get("name").map_err(col_err)?,
        registry_uri: row.try_get("registry_uri").map_err(col_err)?,
        description: row.try_get("description").map_err(col_err)?,
        created_at,
        updated_at,
    })
}

fn parse_session_status(s: &str) -> Result<SessionStatus, MetaError> {
    Ok(match s {
        "pending" => SessionStatus::Pending,
        "active" => SessionStatus::Active,
        "idle" => SessionStatus::Idle,
        "dead" => SessionStatus::Dead,
        "completed" => SessionStatus::Completed,
        "failed" => SessionStatus::Failed,
        other => {
            return Err(MetaError::Serialization(format!(
                "unknown session status: {other}"
            )));
        }
    })
}

fn parse_host_status(s: &str) -> Result<HostStatus, MetaError> {
    Ok(match s {
        "ready" => HostStatus::Ready,
        "draining" => HostStatus::Draining,
        "dead" => HostStatus::Dead,
        other => {
            return Err(MetaError::Serialization(format!(
                "unknown host status: {other}"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each enum variant must round-trip through the wire format the
    /// migration uses. If you add a new variant to the enum, you must
    /// also extend the SQL CHECK constraint and these parsers.
    #[test]
    fn session_status_parses_every_variant() {
        let variants = [
            ("pending", SessionStatus::Pending),
            ("active", SessionStatus::Active),
            ("idle", SessionStatus::Idle),
            ("completed", SessionStatus::Completed),
            ("failed", SessionStatus::Failed),
        ];
        for (s, expected) in variants {
            assert_eq!(parse_session_status(s).unwrap(), expected);
            // Round-trip: the as_str() output must parse back.
            assert_eq!(parse_session_status(expected.as_str()).unwrap(), expected);
        }
    }

    #[test]
    fn host_status_parses_every_variant() {
        for (s, expected) in [
            ("ready", HostStatus::Ready),
            ("draining", HostStatus::Draining),
            ("dead", HostStatus::Dead),
        ] {
            assert_eq!(parse_host_status(s).unwrap(), expected);
            assert_eq!(parse_host_status(expected.as_str()).unwrap(), expected);
        }
    }

    #[test]
    fn unknown_session_status_returns_serialization_error() {
        match parse_session_status("running") {
            Err(MetaError::Serialization(msg)) => {
                assert!(msg.contains("running"), "error must echo the bad value");
            }
            other => panic!("expected Serialization error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_host_status_returns_serialization_error() {
        assert!(matches!(
            parse_host_status("Drain"),
            Err(MetaError::Serialization(_))
        ));
        assert!(matches!(
            parse_host_status(""),
            Err(MetaError::Serialization(_))
        ));
    }
}
