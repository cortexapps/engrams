//! Row -> domain-type conversions. Kept separate so the query bodies in
//! `lib.rs` stay readable.

use chrono::{DateTime, Utc};
use engram_core::types::session::SessionMode;
use engram_core::types::session_op::{OpKind, OpState, SessionOp};
use engram_core::types::{
    CaptureJobProgress, CaptureJobRow, CaptureJobStage, ColdBaseRow, EnableJob, EnableJobState,
    EnabledImage, HostCapacity, HostMetadata, HostRecord, HostStatus, HostUtilization,
    PersistedEvent, RegistryCredential, Session, SessionSecrets, SessionState, SnapshotRecord,
};
use engram_core::{CaptureJobId, HostId, MetaError, SandboxId, SessionId, SnapshotId};
use sqlx::postgres::PgRow;
use sqlx::Row;
use uuid::Uuid;

pub(crate) fn col_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
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
    // ADR 0021 P1.3: `mode` replaced the JSONB `harness` column (see
    // migration 0039). Plain text, CHECK-constrained to the
    // SessionMode wire-string set — match it back here.
    let mode_text: String = row.try_get("mode").map_err(col_err)?;
    let mode = match mode_text.as_str() {
        s if s == SessionMode::Agent.as_str() => SessionMode::Agent,
        s if s == SessionMode::DevVm.as_str() => SessionMode::DevVm,
        other => {
            return Err(MetaError::Serialization(format!(
                "sessions.mode: unknown value {other:?}"
            )));
        }
    };
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
    // Issue #535 (b): migration 0082's TEXT[] NOT NULL DEFAULT '{}' column.
    // Missing-column-tolerant (defaults empty) so a SELECT that doesn't
    // project it (e.g. `list_evacuating_sessions`/`list_evicting_sessions`,
    // which don't need it) still decodes.
    // ADR 0074 parking ladder: park_rung added in migration 0087.
    // SELECTs that don't project it (or pre-migration rows) fall back
    // to 0 = "not parked".
    let park_rung: i16 = row.try_get("park_rung").unwrap_or(0);
    let parked_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("parked_at").ok().flatten();
    // Session titles: suggested_title added in migration 0101. Missing-column-
    // tolerant so a SELECT that doesn't project it still decodes.
    let suggested_title: Option<String> = row.try_get("suggested_title").ok().flatten();
    Ok(Session {
        id: SessionId(id),
        status: parse_session_state(&status)?,
        host_id: host_id.map(HostId),
        sandbox_id: sandbox_id.map(SandboxId),
        image: image_uri,
        mode,
        created_at,
        last_active_at,
        live_disk_manifest,
        park_rung,
        parked_at,
        suggested_title,
    })
}

pub(crate) fn queued_session_from_row(
    row: &PgRow,
) -> Result<engram_core::types::session::QueuedSession, MetaError> {
    let session = session_from_row(row)?;
    let origin_str: String = row.try_get("queue_origin").map_err(col_err)?;
    let origin = engram_core::types::session::QueueOrigin::parse(&origin_str).ok_or_else(|| {
        MetaError::Serialization(format!("queue_origin: unknown value {origin_str:?}"))
    })?;
    let mem_budget_mib: i64 = row.try_get("mem_budget_mib").map_err(col_err)?;
    let cpu_budget_vcpus: i32 = row.try_get("cpu_budget_vcpus").map_err(col_err)?;
    let queued_at: DateTime<Utc> = row.try_get("queued_at").map_err(col_err)?;
    Ok(engram_core::types::session::QueuedSession {
        session,
        origin,
        mem_budget_mib,
        cpu_budget_vcpus,
        queued_at,
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
    let util_disk_total_mib: i64 = row.try_get("util_disk_total_mib").map_err(col_err)?;
    let util_disk_used_mib: i64 = row.try_get("util_disk_used_mib").map_err(col_err)?;
    let util_mem_total_mib: i64 = row.try_get("util_mem_total_mib").map_err(col_err)?;
    let util_mem_used_mib: i64 = row.try_get("util_mem_used_mib").map_err(col_err)?;
    let util_cpu_pct: f32 = row.try_get("util_cpu_pct").map_err(col_err)?;
    let util_allocatable_mib: i64 = row.try_get("allocatable_mib").map_err(col_err)?;
    // Issue #540 (host RAM ledger attribution, migration 0078).
    // `base_shm_pending_mib` has no PG column (transient host-local
    // state, already folded into `util_allocatable_mib` above) — it
    // stays 0 across a DB round-trip; the host's own `/metrics` is the
    // source of truth for it.
    let util_base_shm_mib: i64 = row.try_get("util_base_shm_mib").map_err(col_err)?;
    let util_parked_pss_mib: i64 = row.try_get("util_parked_pss_mib").map_err(col_err)?;
    let util_running_pss_mib: i64 = row.try_get("util_running_pss_mib").map_err(col_err)?;
    let status: String = row.try_get("status").map_err(col_err)?;
    let last_heartbeat_at: DateTime<Utc> = row.try_get("last_heartbeat_at").map_err(col_err)?;
    let cloud_metadata: HostMetadata =
        serde_json::from_value(cloud_meta).map_err(|e| MetaError::Serialization(e.to_string()))?;
    let host_addr: Option<String> = row.try_get("host_addr").map_err(col_err)?;
    // ADR 0047: heartbeat-persisted scheduling state + the
    // coordinator-owned cordon bit (migration 0060).
    let ready_images: Vec<String> =
        serde_json::from_value(row.try_get("ready_images").map_err(col_err)?)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
    let current_bundles = serde_json::from_value(row.try_get("current_bundles").map_err(col_err)?)
        .map_err(|e| MetaError::Serialization(e.to_string()))?;
    let cordoned: bool = row.try_get("cordoned").map_err(col_err)?;
    let total_vcpus: i32 = row.try_get("total_vcpus").map_err(col_err)?;
    // Issue #229: the host's reported bincode wire version (migration 0066).
    let wire_version: i32 = row.try_get("wire_version").map_err(col_err)?;
    // Issue #538: whether this host runs the image-prefetch supervisor
    // (migration 0081).
    let stages_images: bool = row.try_get("stages_images").map_err(col_err)?;
    // ADR 0068 (migration 0080): the self-verified capability vector.
    // `#[serde(default)]` on every `HostCapabilities` field means a
    // pre-0068 row's `'{}'::jsonb` default decodes cleanly to
    // `schema: 0` — the same soft posture `wire_version == 0` gets.
    let capabilities: engram_core::types::host::HostCapabilities =
        serde_json::from_value(row.try_get("capabilities").map_err(col_err)?)
            .map_err(|e| MetaError::Serialization(format!("hosts.capabilities decode: {e}")))?;
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
        utilization: HostUtilization {
            disk_total_mib: util_disk_total_mib.max(0) as u64,
            disk_used_mib: util_disk_used_mib.max(0) as u64,
            mem_total_mib: util_mem_total_mib.max(0) as u64,
            mem_used_mib: util_mem_used_mib.max(0) as u64,
            allocatable_mib: util_allocatable_mib.max(0) as u64,
            cpu_pct: util_cpu_pct.max(0.0),
            base_shm_mib: util_base_shm_mib.max(0) as u64,
            base_shm_pending_mib: 0,
            parked_pss_mib: util_parked_pss_mib.max(0) as u64,
            running_pss_mib: util_running_pss_mib.max(0) as u64,
        },
        status: parse_host_status(&status)?,
        last_heartbeat_at,
        host_addr,
        ready_images,
        current_bundles,
        cordoned,
        total_vcpus: total_vcpus.max(0) as u32,
        wire_version: wire_version.max(0) as u32,
        stages_images,
        capabilities,
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
    // ADR 0035: jsonb [{"drive_id", "sha256"}] — the bundle-GC pin
    // entries this snapshot contributes.
    let aux_bundles_json: serde_json::Value = row.try_get("aux_bundles").map_err(col_err)?;
    let aux_bundles = serde_json::from_value(aux_bundles_json)
        .map_err(|e| MetaError::Serialization(format!("snapshots.aux_bundles decode: {e}")))?;
    // ADR 0028 A.log (migration 0053): the event-log leg of the
    // coherence triple. NULL on pre-0053 rows + template snapshots.
    let events_cursor: Option<i64> = row.try_get("events_cursor").map_err(col_err)?;
    // ADR 0068 (migration 0080): the capturing host's FC snapshot-version
    // pairing key. NULL on pre-0068 rows, VZ/Process captures, and
    // captures recorded without a known host.
    let fc_snapshot_version: Option<String> =
        row.try_get("fc_snapshot_version").map_err(col_err)?;
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
        aux_bundles,
        events_cursor,
        fc_snapshot_version,
    })
}

/// ADR 0079: a `session_ops` row → [`SessionOp`]. Every query that
/// projects an op row uses `lib.rs`'s `OP_COLUMNS` list, so all fourteen
/// columns are always present — strict decode, no missing-column
/// tolerance. `kind`/`state` are exhaustive parses: an unknown wire
/// string is a hard `Serialization` error, never a silent default (a
/// defaulted state could resurrect a terminal op into the executor).
pub(crate) fn session_op_from_row(row: &PgRow) -> Result<SessionOp, MetaError> {
    let session_id: Uuid = row.try_get("session_id").map_err(col_err)?;
    let kind_s: String = row.try_get("kind").map_err(col_err)?;
    let kind = OpKind::parse(&kind_s)
        .ok_or_else(|| MetaError::Serialization(format!("unknown session_ops.kind {kind_s:?}")))?;
    let state_s: String = row.try_get("state").map_err(col_err)?;
    let state = OpState::parse(&state_s).ok_or_else(|| {
        MetaError::Serialization(format!("unknown session_ops.state {state_s:?}"))
    })?;
    Ok(SessionOp {
        id: row.try_get("id").map_err(col_err)?,
        session_id: SessionId(session_id),
        kind,
        payload: row.try_get("payload").map_err(col_err)?,
        state,
        step: row.try_get("step").map_err(col_err)?,
        epoch: row.try_get("epoch").map_err(col_err)?,
        attempts: row.try_get("attempts").map_err(col_err)?,
        not_before: row.try_get("not_before").map_err(col_err)?,
        idempotency_key: row.try_get("idempotency_key").map_err(col_err)?,
        claimed_by: row.try_get("claimed_by").map_err(col_err)?,
        error: row.try_get("error").map_err(col_err)?,
        created_at: row.try_get("created_at").map_err(col_err)?,
        finished_at: row.try_get("finished_at").map_err(col_err)?,
    })
}

pub(crate) fn persisted_event_from_row(row: &PgRow) -> Result<PersistedEvent, MetaError> {
    let idx: i64 = row.try_get("idx").map_err(col_err)?;
    let kind: String = row.try_get("kind").map_err(col_err)?;
    let payload: serde_json::Value = row.try_get("payload").map_err(col_err)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(col_err)?;
    // ADR 0028 A.log (migration 0054). `recovery_epoch` is NOT NULL
    // DEFAULT 0; `rewound_at` is nullable (set on tombstone). i32 in
    // PG → i64 on the wire.
    let recovery_epoch: i32 = row.try_get("recovery_epoch").map_err(col_err)?;
    let rewound_at: Option<DateTime<Utc>> = row.try_get("rewound_at").map_err(col_err)?;
    Ok(PersistedEvent {
        idx,
        kind,
        payload,
        created_at,
        recovery_epoch: recovery_epoch as i64,
        rewound_at,
    })
}

pub(crate) fn artifact_from_row(row: &PgRow) -> Result<engram_core::types::ArtifactRow, MetaError> {
    Ok(engram_core::types::ArtifactRow {
        id: row.try_get("id").map_err(col_err)?,
        blob_key: row.try_get("blob_key").map_err(col_err)?,
        media_type: row.try_get("media_type").map_err(col_err)?,
        size_bytes: row.try_get("size_bytes").map_err(col_err)?,
        caption: row.try_get("caption").map_err(col_err)?,
        created_at: row.try_get("created_at").map_err(col_err)?,
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
    // ADR 0021 P2: NOT NULL in the DB (migration 0042) — every enabled image
    // carries its base snapshot's disk manifest (clean break, no fallback).
    // Option on the struct only mirrors base_snapshot_id's build-then-stamp
    // shape; a persisted row always has Some. Strict decode (no missing-column
    // tolerance): the live SELECTs always project both columns.
    let base_snapshot_disk_manifest_id: Uuid = row
        .try_get("base_snapshot_disk_manifest_id")
        .map_err(col_err)?;
    let base_snapshot_disk_manifest_version: i64 = row
        .try_get("base_snapshot_disk_manifest_version")
        .map_err(col_err)?;
    let base_snapshot_disk_manifest = Some(engram_core::types::manifest::ManifestRef {
        manifest_id: base_snapshot_disk_manifest_id,
        version: base_snapshot_disk_manifest_version as u64,
    });
    // ADR 0021 P2 (memory residency): nullable since migration 0049 — cold-boot
    // backends (VZ) capture a disk-only base snapshot with no memory image, so
    // both columns are NULL. FC populates them. Decode as Option; the residency
    // advertisement + prefetch skip the memory tier when absent.
    let base_snapshot_memory_manifest_id: Option<Uuid> = row
        .try_get("base_snapshot_memory_manifest_id")
        .map_err(col_err)?;
    let base_snapshot_memory_manifest_version: Option<i64> = row
        .try_get("base_snapshot_memory_manifest_version")
        .map_err(col_err)?;
    let base_snapshot_memory_manifest = base_snapshot_memory_manifest_id
        .zip(base_snapshot_memory_manifest_version)
        .map(
            |(manifest_id, version)| engram_core::types::manifest::ManifestRef {
                manifest_id,
                version: version as u64,
            },
        );
    // ADR 0021 P1.8: nullable soft-delete marker (migration 0041).
    // Missing-column-tolerant via try_get → `Ok(None)` from the
    // generic decode path so a row pulled before the migration runs
    // still decodes; live SELECTs always project the column.
    let soft_deleted_at: Option<DateTime<Utc>> = row.try_get("soft_deleted_at").unwrap_or(None);
    Ok(EnabledImage {
        id,
        image_uri: row.try_get("image_uri").map_err(col_err)?,
        // ADR 0080 (migration 0094): both JSONB columns are NOT NULL
        // with no default and the migration wiped pre-0080 rows, so
        // strict decode — a missing/malformed value is a real bug.
        image_config: jsonb_from_row(row, "image_config")?,
        oci_defaults: jsonb_from_row(row, "oci_defaults")?,
        manifest_digest: row.try_get("manifest_digest").map_err(col_err)?,
        disk_manifest,
        base_snapshot_id: base_snapshot_id.map(engram_core::types::SnapshotId),
        base_snapshot_disk_manifest,
        base_snapshot_memory_manifest,
        last_refreshed_at,
        created_at,
        updated_at,
        soft_deleted_at,
    })
}

/// Strictly decode a JSONB column into a serde type. Missing column or
/// malformed value are both hard errors — post-0094 the callers'
/// columns are NOT NULL with no legacy rows to tolerate.
fn jsonb_from_row<T: serde::de::DeserializeOwned>(row: &PgRow, col: &str) -> Result<T, MetaError> {
    let v: serde_json::Value = row.try_get(col).map_err(col_err)?;
    serde_json::from_value(v).map_err(|e| MetaError::Serialization(format!("{col}: {e}")))
}

/// Issue #539: `enable_jobs.warm_stages` is a nullable JSONB column —
/// `NULL` means "no stage history yet" (outside/before a capture ever
/// wrote progress). Every current query projects this column (added by
/// migration 0079 alongside the row's other four new columns, all of
/// which use `.map_err(col_err)` — see `enable_job_from_row`); a missing
/// column is a real bug (e.g. a future SELECT/RETURNING that forgets it),
/// not a legacy-row case to default through silently.
fn warm_stages_from_row(
    row: &PgRow,
) -> Result<Vec<engram_core::types::WarmStageRecord>, MetaError> {
    match row
        .try_get::<Option<serde_json::Value>, _>("warm_stages")
        .map_err(col_err)?
    {
        Some(v) => serde_json::from_value(v).map_err(|e| MetaError::Serialization(e.to_string())),
        None => Ok(Vec::new()),
    }
}

/// ADR 0088 UI follow-up: `enable_jobs.materialize_stages` — NOT NULL
/// DEFAULT '[]' (migration 0103), same `WarmStageRecord` array shape as
/// `warm_stages`.
fn materialize_stages_from_row(
    row: &PgRow,
) -> Result<Vec<engram_core::types::WarmStageRecord>, MetaError> {
    let v: serde_json::Value = row.try_get("materialize_stages").map_err(col_err)?;
    serde_json::from_value(v).map_err(|e| MetaError::Serialization(e.to_string()))
}

fn capture_phase_from_row(
    row: &PgRow,
) -> Result<Option<engram_core::types::CapturePhase>, MetaError> {
    match row
        .try_get::<Option<String>, _>("capture_phase")
        .map_err(col_err)?
    {
        Some(s) => Ok(match s.as_str() {
            "boot" => Some(engram_core::types::CapturePhase::Boot),
            "warm" => Some(engram_core::types::CapturePhase::Warm),
            "snapshot" => Some(engram_core::types::CapturePhase::Snapshot),
            _ => None,
        }),
        None => Ok(None),
    }
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

// ADR 0021 P1.5a retired `harness_pack_from_row` with the rest of
// the harness-packs registry.

// ADR 0051 retired `user_from_row` / `user_token_from_row` /
// `web_session_from_row` with the ADR 0031 user-identity surface (the
// `users` / `user_tokens` / `web_sessions` tables are dropped — see
// migration 0068).

pub(crate) fn parse_session_state_for_lib(s: &str) -> Result<SessionState, MetaError> {
    parse_session_state(s)
}

fn parse_session_state(s: &str) -> Result<SessionState, MetaError> {
    Ok(match s {
        "pending" => SessionState::Pending,
        "queued" => SessionState::Queued,
        "created" => SessionState::Created,
        "active" => SessionState::Active,
        "unreachable" => SessionState::Unreachable,
        "idle" => SessionState::Idle,
        "host_lost" => SessionState::HostLost,
        "evacuating" => SessionState::Evacuating,
        "evicting" => SessionState::Evicting,
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

/// ADR 0036: enable_jobs.state column ↔ `EnableJobState`. Exhaustive
/// — an unknown string is a hard `Serialization` error, never a
/// silent default (defaulting would resurrect terminal jobs into the
/// scanner's sweep).
pub(crate) fn parse_enable_job_state(s: &str) -> Result<EnableJobState, MetaError> {
    Ok(match s {
        "pending" => EnableJobState::Pending,
        "materializing" => EnableJobState::Materializing,
        "capturing" => EnableJobState::Capturing,
        "prestaging" => EnableJobState::Prestaging,
        "ready" => EnableJobState::Ready,
        "failed" => EnableJobState::Failed,
        other => {
            return Err(MetaError::Serialization(format!(
                "unknown enable job state: {other}"
            )));
        }
    })
}

pub(crate) fn enable_job_from_row(row: &PgRow) -> Result<EnableJob, MetaError> {
    let state: String = row.try_get("state").map_err(col_err)?;
    let chunks_total: Option<i32> = row.try_get("chunks_total").map_err(col_err)?;
    let chunks_done: i32 = row.try_get("chunks_done").map_err(col_err)?;
    let attempts: i32 = row.try_get("attempts").map_err(col_err)?;
    // Migration 0081: NOT NULL DEFAULT '{}'::jsonb, so every row has it.
    let prestage_hosts: serde_json::Value = row.try_get("prestage_hosts").map_err(col_err)?;
    Ok(EnableJob {
        id: row.try_get("id").map_err(col_err)?,
        image_uri: row.try_get("image_uri").map_err(col_err)?,
        manifest_digest: row.try_get("manifest_digest").map_err(col_err)?,
        state: parse_enable_job_state(&state)?,
        chunks_total: chunks_total.map(|v| v.max(0) as u32),
        chunks_done: chunks_done.max(0) as u32,
        attempts: attempts.max(0) as u32,
        error: row.try_get("error").map_err(col_err)?,
        image_config: jsonb_from_row(row, "image_config")?,
        force_recapture: row.try_get("force_recapture").map_err(col_err)?,
        prestage_hosts,
        capture_phase: capture_phase_from_row(row)?,
        warm_stage: row.try_get("warm_stage").map_err(col_err)?,
        warm_stage_started_at: row.try_get("warm_stage_started_at").map_err(col_err)?,
        warm_stages: warm_stages_from_row(row)?,
        materialize_stages: materialize_stages_from_row(row)?,
        materialize_host_id: row
            .try_get::<Option<Uuid>, _>("materialize_host_id")
            .map_err(col_err)?
            .map(HostId::from),
        output_tail: row.try_get("output_tail").map_err(col_err)?,
        created_at: row.try_get("created_at").map_err(col_err)?,
        updated_at: row.try_get("updated_at").map_err(col_err)?,
    })
}

/// ADR 0084: `capture_jobs.stage` column <-> `CaptureJobStage`.
/// Exhaustive — an unknown string is a hard `Serialization` error
/// rather than a silent default (which would resurrect a terminal or
/// misclassify a live job to the deadline scan).
pub(crate) fn parse_capture_job_stage(s: &str) -> Result<CaptureJobStage, MetaError> {
    CaptureJobStage::parse(s)
        .ok_or_else(|| MetaError::Serialization(format!("unknown capture job stage: {s}")))
}

/// Nullable JSONB decode for `capture_jobs.stage_progress`. `NULL`
/// means "no progress event yet for this stage."
fn capture_job_progress_from_row(row: &PgRow) -> Result<Option<CaptureJobProgress>, MetaError> {
    match row
        .try_get::<Option<serde_json::Value>, _>("stage_progress")
        .map_err(col_err)?
    {
        Some(v) => serde_json::from_value(v).map_err(|e| MetaError::Serialization(e.to_string())),
        None => Ok(None),
    }
}

pub(crate) fn capture_job_from_row(row: &PgRow) -> Result<CaptureJobRow, MetaError> {
    let id: Uuid = row.try_get("id").map_err(col_err)?;
    // Nullable since migration 0099: NULL == waiting for capacity.
    let host_id: Option<Uuid> = row.try_get("host_id").map_err(col_err)?;
    let stage: String = row.try_get("stage").map_err(col_err)?;
    let attempts: i32 = row.try_get("attempts").map_err(col_err)?;
    Ok(CaptureJobRow {
        id: CaptureJobId(id),
        enable_job_id: row.try_get("enable_job_id").map_err(col_err)?,
        image_uri: row.try_get("image_uri").map_err(col_err)?,
        manifest_digest: row.try_get("manifest_digest").map_err(col_err)?,
        disk_manifest: row.try_get("disk_manifest").map_err(col_err)?,
        image_config: jsonb_from_row(row, "image_config")?,
        oci_defaults: jsonb_from_row(row, "oci_defaults")?,
        host_id: host_id.map(HostId),
        mem_budget_mib: row.try_get("mem_budget_mib").map_err(col_err)?,
        cpu_budget_vcpus: row.try_get("cpu_budget_vcpus").map_err(col_err)?,
        waiting_since: row.try_get("waiting_since").map_err(col_err)?,
        epoch: row.try_get("epoch").map_err(col_err)?,
        stage: parse_capture_job_stage(&stage)?,
        stage_started_at: row.try_get("stage_started_at").map_err(col_err)?,
        stage_progress: capture_job_progress_from_row(row)?,
        last_progress_at: row.try_get("last_progress_at").map_err(col_err)?,
        attempts: attempts.max(0) as u32,
        retryable: row.try_get("retryable").map_err(col_err)?,
        error: row.try_get("error").map_err(col_err)?,
        error_stage: row.try_get("error_stage").map_err(col_err)?,
        fc_snapshot_version: row.try_get("fc_snapshot_version").map_err(col_err)?,
        result_bincode: row.try_get("result_bincode").map_err(col_err)?,
        created_at: row.try_get("created_at").map_err(col_err)?,
        updated_at: row.try_get("updated_at").map_err(col_err)?,
    })
}

pub(crate) fn cold_base_from_row(row: &PgRow) -> Result<ColdBaseRow, MetaError> {
    let snapshot_id: Uuid = row.try_get("snapshot_id").map_err(col_err)?;
    // Migration 0098: nullable for schema-evolution safety, but every
    // row `upsert_cold_base` writes always sets it — a NULL here means
    // a row written before 0098 landed (impossible in practice: the
    // table was dormant until this same change started writing it) or
    // a hand-edited row. Either way, treat it as unusable rather than
    // handing the executor a `Vec::new()` it would fail to bincode-
    // decode with a confusing error.
    let snapshot_bincode: Option<Vec<u8>> = row.try_get("snapshot_bincode").map_err(col_err)?;
    let snapshot_bincode = snapshot_bincode.ok_or_else(|| {
        MetaError::Serialization(format!(
            "cold_bases row {snapshot_id} has no snapshot_bincode (pre-migration-0098 row?)"
        ))
    })?;
    Ok(ColdBaseRow {
        content_key: row.try_get("content_key").map_err(col_err)?,
        snapshot_id: SnapshotId(snapshot_id),
        disk_manifest: row.try_get("disk_manifest").map_err(col_err)?,
        memory_manifest: row.try_get("memory_manifest").map_err(col_err)?,
        fc_snapshot_version: row.try_get("fc_snapshot_version").map_err(col_err)?,
        captured_at: row.try_get("captured_at").map_err(col_err)?,
        snapshot_bincode,
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
    /// `host_lost` join the original six. If you add a new variant,
    /// extend this test, `parse_session_state`, and the `sessions`
    /// status CHECK constraint together.
    #[test]
    fn session_state_parses_every_variant() {
        let variants = [
            ("pending", SessionState::Pending),
            ("queued", SessionState::Queued),
            ("created", SessionState::Created),
            ("active", SessionState::Active),
            ("idle", SessionState::Idle),
            ("host_lost", SessionState::HostLost),
            ("evacuating", SessionState::Evacuating),
            ("evicting", SessionState::Evicting),
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

    /// ADR 0036: every `EnableJobState` variant round-trips through
    /// the wire string, and unknown strings error rather than
    /// defaulting — a silent default would resurrect terminal jobs
    /// into the scanner's sweep. Extend this test and
    /// `parse_enable_job_state` together when adding variants.
    #[test]
    fn enable_job_state_parses_every_variant() {
        let variants = [
            ("pending", EnableJobState::Pending),
            ("materializing", EnableJobState::Materializing),
            ("capturing", EnableJobState::Capturing),
            ("prestaging", EnableJobState::Prestaging),
            ("ready", EnableJobState::Ready),
            ("failed", EnableJobState::Failed),
        ];
        for (s, expected) in variants {
            assert_eq!(parse_enable_job_state(s).unwrap(), expected);
            assert_eq!(parse_enable_job_state(expected.as_str()).unwrap(), expected);
        }
    }

    #[test]
    fn unknown_enable_job_state_returns_serialization_error() {
        match parse_enable_job_state("enabling") {
            Err(MetaError::Serialization(msg)) => {
                assert!(msg.contains("enabling"), "error must echo the bad value");
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
