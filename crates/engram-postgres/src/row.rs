//! Row -> domain-type conversions. Kept separate so the query bodies in
//! `lib.rs` stay readable.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use engram_core::types::{
    HostCapacity, HostMetadata, HostRecord, HostStatus, ImageStatus, ImageVersion, PersistedEvent,
    Session, SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, ImageVersionId, MetaError, SessionId, SnapshotId};
use sqlx::postgres::PgRow;
use sqlx::Row;
use uuid::Uuid;

fn col_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
    MetaError::Db(Box::new(e))
}

pub(crate) fn session_from_row(row: &PgRow) -> Result<Session, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let host_id: Option<Uuid> = row.try_get("host_id").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let last_active_at: DateTime<Utc> = row.try_get("last_active_at").map_err(col_err)?;
    Ok(Session {
        id: SessionId(id),
        repo: row.try_get("repo").map_err(col_err)?,
        branch: row.try_get("branch").map_err(col_err)?,
        user_id: row.try_get("user_id").map_err(col_err)?,
        status: parse_session_status(&status)?,
        image_version: row.try_get("image_version").map_err(col_err)?,
        host_id: host_id.map(HostId),
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
    let blob_url: Option<String> = row.try_get("blob_url").map_err(col_err)?;
    let size_bytes: i64 = row.try_get("size_bytes").map_err(col_err)?;
    let replicated_at: Option<DateTime<Utc>> = row.try_get("replicated_at").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let last_accessed_at: DateTime<Utc> = row.try_get("last_accessed_at").map_err(col_err)?;
    Ok(SnapshotRecord {
        id: SnapshotId(id),
        session_id: SessionId(session_id),
        host_id: host_id.map(HostId),
        local_path: local_path.map(PathBuf::from),
        blob_url,
        image_version: row.try_get("image_version").map_err(col_err)?,
        size_bytes: size_bytes.max(0) as u64,
        replicated_at,
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

pub(crate) fn image_from_row(row: &PgRow) -> Result<ImageVersion, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    Ok(ImageVersion {
        id: ImageVersionId(id),
        repo: row.try_get("repo").map_err(col_err)?,
        tag: row.try_get("tag").map_err(col_err)?,
        blob_url: row.try_get("blob_url").map_err(col_err)?,
        status: parse_image_status(&status)?,
        created_at,
    })
}

fn parse_session_status(s: &str) -> Result<SessionStatus, MetaError> {
    Ok(match s {
        "pending" => SessionStatus::Pending,
        "active" => SessionStatus::Active,
        "idle" => SessionStatus::Idle,
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

fn parse_image_status(s: &str) -> Result<ImageStatus, MetaError> {
    Ok(match s {
        "building" => ImageStatus::Building,
        "ready" => ImageStatus::Ready,
        "retired" => ImageStatus::Retired,
        other => {
            return Err(MetaError::Serialization(format!(
                "unknown image status: {other}"
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
    fn image_status_parses_every_variant() {
        for (s, expected) in [
            ("building", ImageStatus::Building),
            ("ready", ImageStatus::Ready),
            ("retired", ImageStatus::Retired),
        ] {
            assert_eq!(parse_image_status(s).unwrap(), expected);
            assert_eq!(parse_image_status(expected.as_str()).unwrap(), expected);
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

    #[test]
    fn unknown_image_status_returns_serialization_error() {
        assert!(matches!(
            parse_image_status("READY"),
            Err(MetaError::Serialization(_))
        ));
    }
}
