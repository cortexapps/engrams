//! Row -> domain-type conversions. Kept separate so the query bodies in
//! `lib.rs` stay readable.

use chrono::{DateTime, Utc};
use engram_core::types::session::HarnessSpec;
use engram_core::types::{
    EnabledImage, HarnessPack, HostCapacity, HostMetadata, HostRecord, HostStatus, PersistedEvent,
    RegistryCredential, Session, SessionSecrets, SessionState, SnapshotRecord,
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
    let image_uri: String = row.try_get("image_uri").map_err(col_err)?;
    let harness_json: serde_json::Value = row.try_get("harness").map_err(col_err)?;
    let harness: HarnessSpec = serde_json::from_value(harness_json)
        .map_err(|e| MetaError::Serialization(format!("harness: {e}")))?;
    // ADR 0016 Phase B: live_disk_manifest_* columns added in
    // migration 0034. The both-or-neither CHECK constraint
    // guarantees these two columns are either both NULL or both
    // populated, so we collapse them into Option<ManifestRef>.
    // Older SELECT statements that don't project these columns
    // get `try_get` errors → fall back to None.
    let live_disk_manifest_id: Option<Uuid> = row.try_get("live_disk_manifest_id").ok().flatten();
    let live_disk_manifest_version: Option<i64> =
        row.try_get("live_disk_manifest_version").ok().flatten();
    let live_disk_manifest = match (live_disk_manifest_id, live_disk_manifest_version) {
        (Some(mid), Some(ver)) => Some(engram_core::types::manifest::ManifestRef {
            manifest_id: mid,
            version: ver as u64,
        }),
        _ => None,
    };
    Ok(Session {
        id: SessionId(id),
        user_id: row.try_get("user_id").map_err(col_err)?,
        status: parse_session_state(&status)?,
        host_id: host_id.map(HostId),
        sandbox_id: sandbox_id.map(SandboxId),
        image: image_uri,
        harness,
        created_at,
        last_active_at,
        live_disk_manifest,
    })
}

pub(crate) fn host_from_row(row: &PgRow) -> Result<HostRecord, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    let cloud_meta: serde_json::Value = row.try_get("cloud_metadata").map_err(col_err)?;
    let total_gb: i32 = row.try_get("capacity_total_gb").map_err(col_err)?;
    let used_gb: i32 = row.try_get("capacity_used_gb").map_err(col_err)?;
    let total_mib: i64 = row.try_get("capacity_total_mib").map_err(col_err)?;
    let used_mib: i64 = row.try_get("capacity_used_mib").map_err(col_err)?;
    let running_sandboxes: i32 = row.try_get("running_sandboxes_count").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let last_heartbeat_at: DateTime<Utc> = row.try_get("last_heartbeat_at").map_err(col_err)?;
    let cloud_metadata: HostMetadata =
        serde_json::from_value(cloud_meta).map_err(|e| MetaError::Serialization(e.to_string()))?;
    let host_addr: Option<String> = row.try_get("host_addr").map_err(col_err)?;
    Ok(HostRecord {
        id: HostId(id),
        hostname: row.try_get("hostname").map_err(col_err)?,
        cloud_metadata,
        capacity: HostCapacity {
            total_gb: total_gb.max(0) as u32,
            used_gb: used_gb.max(0) as u32,
            total_mib: total_mib.max(0) as u64,
            used_mib: used_mib.max(0) as u64,
            running_sandboxes: running_sandboxes.max(0) as u32,
        },
        status: parse_host_status(&status)?,
        last_heartbeat_at,
        host_addr,
    })
}

pub(crate) fn snapshot_from_row(row: &PgRow) -> Result<SnapshotRecord, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    // Migration 0028 made session_id nullable so template snapshots
    // produced by the M1.11 enabled_images cascade don't need a
    // sentinel FK. Existing session-bound rows continue to populate
    // it; template rows leave it NULL.
    let session_id: Option<Uuid> = row.try_get("session_id").map_err(col_err)?;
    let host_id: Option<Uuid> = row.try_get("host_id").map_err(col_err)?;
    let size_bytes: i64 = row.try_get("size_bytes").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    let last_accessed_at: DateTime<Utc> = row.try_get("last_accessed_at").map_err(col_err)?;
    // ADR 0007 columns (migration 0018 + 0019). Both pairs are
    // nullable; the DB constraint enforces "both or neither" so
    // half-populated rows can't happen.
    let disk_manifest_id: Option<Uuid> = row.try_get("disk_manifest_id").map_err(col_err)?;
    let disk_manifest_version: Option<i64> =
        row.try_get("disk_manifest_version").map_err(col_err)?;
    let disk_manifest = match (disk_manifest_id, disk_manifest_version) {
        (Some(id), Some(version)) => Some(engram_core::types::manifest::ManifestRef {
            manifest_id: id,
            version: version.max(0) as u64,
        }),
        _ => None,
    };
    let memory_manifest_id: Option<Uuid> = row.try_get("memory_manifest_id").map_err(col_err)?;
    let memory_manifest_version: Option<i64> =
        row.try_get("memory_manifest_version").map_err(col_err)?;
    let memory_manifest = match (memory_manifest_id, memory_manifest_version) {
        (Some(id), Some(version)) => Some(engram_core::types::manifest::ManifestRef {
            manifest_id: id,
            version: version.max(0) as u64,
        }),
        _ => None,
    };
    let recoverable: bool = row.try_get("recoverable").map_err(col_err)?;
    Ok(SnapshotRecord {
        id: SnapshotId(id),
        session_id: session_id.map(SessionId),
        host_id: host_id.map(HostId),
        image_version: row.try_get("image_version").map_err(col_err)?,
        size_bytes: size_bytes.max(0) as u64,
        created_at,
        last_accessed_at,
        disk_manifest,
        memory_manifest,
        recoverable,
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
    // ADR 0016 Phase C: both-or-neither CHECK in migration 0036 keeps
    // these coherent; tuple match unifies them into one Option.
    let disk_manifest_id: Option<Uuid> = row.try_get("disk_manifest_id").map_err(col_err)?;
    let disk_manifest_version: Option<i64> =
        row.try_get("disk_manifest_version").map_err(col_err)?;
    let disk_manifest = match (disk_manifest_id, disk_manifest_version) {
        (Some(id), Some(v)) => Some(engram_core::types::manifest::ManifestRef {
            manifest_id: id,
            version: v as u64,
        }),
        (None, None) => None,
        _ => {
            return Err(MetaError::Serialization(
                "enabled_images.disk_manifest_{id,version} CHECK violated — one side NULL".into(),
            ));
        }
    };
    // ADR 0020 P1: NOT NULL in the DB (migration 0038); Option here only
    // mirrors disk_manifest's build-then-stamp shape — a persisted row
    // always has it.
    let base_snapshot_id: Option<Uuid> = row.try_get("base_snapshot_id").map_err(col_err)?;
    Ok(EnabledImage {
        id,
        image_uri: row.try_get("image_uri").map_err(col_err)?,
        manifest_toml: row.try_get("manifest_toml").map_err(col_err)?,
        manifest_digest: row.try_get("manifest_digest").map_err(col_err)?,
        disk_manifest,
        base_snapshot_id: base_snapshot_id.map(engram_core::types::SnapshotId),
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

pub(crate) fn parse_session_state_for_lib(s: &str) -> Result<SessionState, MetaError> {
    parse_session_state(s)
}

fn parse_session_state(s: &str) -> Result<SessionState, MetaError> {
    Ok(match s {
        "pending" => SessionState::Pending,
        "created" => SessionState::Created,
        "guest_ready" => SessionState::GuestReady,
        "active" => SessionState::Active,
        "idle" => SessionState::Idle,
        "host_lost" => SessionState::HostLost,
        "evacuating" => SessionState::Evacuating,
        "dead" => SessionState::Dead,
        "completed" => SessionState::Completed,
        "failed" => SessionState::Failed,
        other => {
            return Err(MetaError::Serialization(format!(
                "unknown session state: {other}"
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
    /// migration uses. ADR 0015 M2 expanded the set: `created`,
    /// `guest_ready`, `host_lost` join the original six. If you add
    /// a new variant, extend this test and `parse_session_state`
    /// together — the column is `TEXT` with no CHECK constraint, so
    /// the parser is the only enforcement.
    #[test]
    fn session_state_parses_every_variant() {
        let variants = [
            ("pending", SessionState::Pending),
            ("created", SessionState::Created),
            ("guest_ready", SessionState::GuestReady),
            ("active", SessionState::Active),
            ("idle", SessionState::Idle),
            ("host_lost", SessionState::HostLost),
            ("evacuating", SessionState::Evacuating),
            ("completed", SessionState::Completed),
            ("failed", SessionState::Failed),
            ("dead", SessionState::Dead),
        ];
        for (s, expected) in variants {
            assert_eq!(parse_session_state(s).unwrap(), expected);
            // Round-trip: the as_str() output must parse back.
            assert_eq!(parse_session_state(expected.as_str()).unwrap(), expected);
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
    fn unknown_session_state_returns_serialization_error() {
        match parse_session_state("running") {
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
