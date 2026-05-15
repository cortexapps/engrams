//! Postgres-backed [`MetadataStore`] implementation.
//!
//! Queries are written against the schema in `deploy/migrations/0001_initial.sql`.
//! sqlx's compile-time checking is intentionally disabled here — runtime
//! queries let the crate build without a live database, which keeps the
//! workspace usable for early-stage development. We can flip to
//! `query!`/`query_as!` once CI provisions a Postgres service and we
//! ship a `.sqlx/` cache.

use async_trait::async_trait;
use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::{
    EnabledImage, HarnessPack, HostCapacity, HostRecord, HostStatus, PersistedEvent,
    RegistryCredential, Session, SessionSecrets, SessionSpec, SessionStatus, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

mod row;

#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn connect(url: &str) -> Result<Self, MetaError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await
            .map_err(|e| MetaError::Db(Box::new(e)))?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Run sqlx migrations from `deploy/migrations`.
    pub async fn migrate(&self) -> Result<(), MetaError> {
        sqlx::migrate!("../../deploy/migrations")
            .run(&self.pool)
            .await
            .map_err(|e| MetaError::Migration(e.to_string()))
    }
}

fn db_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
    MetaError::Db(Box::new(e))
}

#[async_trait]
impl MetadataStore for PostgresStore {
    async fn ping(&self) -> Result<(), MetaError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(db_err)
    }

    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let harness_json = serde_json::to_value(&spec.harness)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, user_id, status, host_id,
                 image_uri, harness,
                 created_at, last_active_at)
            VALUES ($1, $2, $3, NULL, $4, $5, $6, $6)
            "#,
        )
        .bind(id)
        .bind(spec.user_id.as_deref())
        .bind(SessionStatus::Pending.as_str())
        .bind(&spec.image)
        .bind(harness_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(SessionId(id))
    }

    async fn create_session_active(
        &self,
        session_id: SessionId,
        spec: SessionSpec,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError> {
        let now = Utc::now();
        let harness_json = serde_json::to_value(&spec.harness)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, user_id, status, host_id, sandbox_id,
                 image_uri, harness,
                 created_at, last_active_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(spec.user_id.as_deref())
        .bind(SessionStatus::Active.as_str())
        .bind(host_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(&spec.image)
        .bind(harness_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, user_id, status, host_id, sandbox_id,
                   image_uri, harness,
                   created_at, last_active_at
            FROM sessions WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        row::session_from_row(&row)
    }

    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, user_id, status, host_id, sandbox_id,
                   image_uri, harness,
                   created_at, last_active_at
            FROM sessions
            WHERE status IN ('pending','active','idle')
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::session_from_row).collect()
    }

    async fn list_active_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId)>, MetaError> {
        // ADR 0009 reconcile pass query. Per-host, every heartbeat:
        // ~50 sandboxes/host × 5s cadence × N hosts = trivial DB load.
        // Indexed via `idx_sessions_host_status` (existing).
        let rows = sqlx::query(
            r#"
            SELECT id, sandbox_id
            FROM sessions
            WHERE host_id = $1
              AND status = 'active'
              AND sandbox_id IS NOT NULL
            "#,
        )
        .bind(host_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let session: Uuid = r
                .try_get("id")
                .map_err(|e| MetaError::Serialization(format!("active-assignments: id: {e}")))?;
            let sandbox: Uuid = r.try_get("sandbox_id").map_err(|e| {
                MetaError::Serialization(format!("active-assignments: sandbox_id: {e}"))
            })?;
            out.push((SessionId::from(session), SandboxId::from(sandbox)));
        }
        Ok(out)
    }

    async fn set_session_status(
        &self,
        id: SessionId,
        status: SessionStatus,
    ) -> Result<(), MetaError> {
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2, last_active_at = NOW(), updated_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(status.as_str())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        let n = sqlx::query(
            r#"
            UPDATE sessions SET host_id = $2, updated_at = NOW() WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.map(|h| h.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<(), MetaError> {
        let n = sqlx::query(
            r#"
            UPDATE sessions SET sandbox_id = $2, updated_at = NOW() WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(sandbox_id.map(|s| s.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError> {
        let cloud_meta = serde_json::to_value(&host.cloud_metadata)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO hosts (id, hostname, cloud_metadata,
                               capacity_total_gb, capacity_used_gb,
                               capacity_total_mib, capacity_used_mib,
                               running_sandboxes_count,
                               last_heartbeat_at, status, host_addr, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())
            ON CONFLICT (hostname) DO UPDATE SET
                cloud_metadata          = EXCLUDED.cloud_metadata,
                capacity_total_gb       = EXCLUDED.capacity_total_gb,
                capacity_used_gb        = EXCLUDED.capacity_used_gb,
                capacity_total_mib      = EXCLUDED.capacity_total_mib,
                capacity_used_mib       = EXCLUDED.capacity_used_mib,
                running_sandboxes_count = EXCLUDED.running_sandboxes_count,
                last_heartbeat_at       = EXCLUDED.last_heartbeat_at,
                status                  = EXCLUDED.status,
                host_addr               = COALESCE(EXCLUDED.host_addr, hosts.host_addr),
                updated_at              = NOW()
            "#,
        )
        .bind(host.id.as_uuid())
        .bind(&host.hostname)
        .bind(cloud_meta)
        .bind(host.capacity.total_gb as i32)
        .bind(host.capacity.used_gb as i32)
        .bind(host.capacity.total_mib as i64)
        .bind(host.capacity.used_mib as i64)
        .bind(host.capacity.running_sandboxes as i32)
        .bind(host.last_heartbeat_at)
        .bind(host.status.as_str())
        .bind(host.host_addr.as_deref())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        // `ORDER BY id` is the cheapest stable sort: id is the PK, so
        // the index walk is free, and UUIDs give a deterministic order
        // across heartbeats. Without this, the heap-scan order shifts
        // every ~5s as `UPDATE ... last_heartbeat_at = NOW()` touches
        // rows, which makes the SPA's host list flip-flop on each poll.
        let rows = sqlx::query(
            r#"
            SELECT id, hostname, cloud_metadata,
                   capacity_total_gb, capacity_used_gb,
                   capacity_total_mib, capacity_used_mib,
                   running_sandboxes_count,
                   last_heartbeat_at, status, host_addr
            FROM hosts WHERE status IN ('ready','draining')
            ORDER BY id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::host_from_row).collect()
    }

    async fn set_host_status(&self, id: HostId, status: HostStatus) -> Result<(), MetaError> {
        let n = sqlx::query(r#"UPDATE hosts SET status = $2, updated_at = NOW() WHERE id = $1"#)
            .bind(id.as_uuid())
            .bind(status.as_str())
            .execute(&self.pool)
            .await
            .map_err(db_err)?
            .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn touch_host_heartbeat(
        &self,
        id: HostId,
        status: HostStatus,
        capacity: HostCapacity,
    ) -> Result<(), MetaError> {
        let n = sqlx::query(
            r#"UPDATE hosts
                  SET status = $2,
                      capacity_total_mib = $3,
                      capacity_used_mib = $4,
                      running_sandboxes_count = $5,
                      last_heartbeat_at = NOW(),
                      updated_at = NOW()
                WHERE id = $1"#,
        )
        .bind(id.as_uuid())
        .bind(status.as_str())
        .bind(capacity.total_mib as i64)
        .bind(capacity.used_mib as i64)
        .bind(capacity.running_sandboxes as i32)
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn list_stale_hosts(&self, threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        // `make_interval` keeps the threshold parameterised without
        // string-templating an INTERVAL literal. Cast to BIGINT so a
        // very-large threshold (well past i32::MAX) doesn't overflow.
        let rows = sqlx::query(
            r#"
            SELECT id, hostname, cloud_metadata,
                   capacity_total_gb, capacity_used_gb,
                   capacity_total_mib, capacity_used_mib,
                   running_sandboxes_count,
                   last_heartbeat_at, status, host_addr
              FROM hosts
             WHERE status IN ('ready','draining')
               AND last_heartbeat_at < NOW() - make_interval(secs => $1::bigint)
            "#,
        )
        .bind(threshold_secs as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::host_from_row).collect()
    }

    async fn mark_host_dead_and_reassign_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SessionId>, MetaError> {
        // Single transaction: hosts.status -> dead, every session row
        // pointing at this host gets host_id cleared and status flipped
        // to dead. Returning the affected session ids lets
        // the caller emit per-session StatusChanged events without a
        // second query. The hosts UPDATE deliberately omits a rows-
        // affected check — calling this on a host already marked dead
        // is a benign no-op (the sessions UPDATE returns an empty list).
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query(r#"UPDATE hosts SET status = 'dead', updated_at = NOW() WHERE id = $1"#)
            .bind(host_id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        let rows = sqlx::query(
            r#"
            UPDATE sessions
               SET host_id    = NULL,
                   sandbox_id = NULL,
                   status     = 'dead',
                   last_active_at = NOW()
             WHERE host_id = $1
               AND status NOT IN ('completed','failed')
            RETURNING id
            "#,
        )
        .bind(host_id.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;

        let ids = rows
            .iter()
            .map(|r| {
                let uuid: uuid::Uuid = sqlx::Row::try_get(r, "id").map_err(db_err)?;
                Ok(SessionId::from(uuid))
            })
            .collect::<Result<Vec<SessionId>, MetaError>>()?;
        Ok(ids)
    }

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
        // ADR 0007: single-tier durability. Every snapshot row
        // references chunked manifests in `BlobStorage` via the
        // `disk_manifest_*` / `memory_manifest_*` quartet. The
        // previous hot-tier (`local_path`) + cold-tier (envelope-
        // encrypted blob ref) columns retired with Phase 7
        // (migration 0020).
        sqlx::query(
            r#"
            INSERT INTO snapshots
                (id, session_id, host_id,
                 image_version, size_bytes, created_at, last_accessed_at,
                 disk_manifest_id, disk_manifest_version,
                 memory_manifest_id, memory_manifest_version,
                 recoverable)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id) DO UPDATE SET
                last_accessed_at        = EXCLUDED.last_accessed_at,
                disk_manifest_id        = EXCLUDED.disk_manifest_id,
                disk_manifest_version   = EXCLUDED.disk_manifest_version,
                memory_manifest_id      = EXCLUDED.memory_manifest_id,
                memory_manifest_version = EXCLUDED.memory_manifest_version,
                recoverable             = EXCLUDED.recoverable,
                updated_at              = NOW()
            "#,
        )
        .bind(snap.id.as_uuid())
        .bind(snap.session_id.as_uuid())
        .bind(snap.host_id.map(|h| h.as_uuid()))
        .bind(&snap.image_version)
        .bind(snap.size_bytes as i64)
        .bind(snap.created_at)
        .bind(snap.last_accessed_at)
        .bind(snap.disk_manifest.map(|m| m.manifest_id))
        .bind(snap.disk_manifest.map(|m| m.version as i64))
        .bind(snap.memory_manifest.map(|m| m.manifest_id))
        .bind(snap.memory_manifest.map(|m| m.version as i64))
        .bind(snap.recoverable)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_snapshots_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, session_id, host_id,
                   image_version, size_bytes,
                   created_at, last_accessed_at,
                   disk_manifest_id, disk_manifest_version,
                   memory_manifest_id, memory_manifest_version,
                   recoverable
            FROM snapshots WHERE session_id = $1 ORDER BY created_at DESC
            "#,
        )
        .bind(sid.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::snapshot_from_row).collect()
    }

    async fn latest_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, session_id, host_id,
                   image_version, size_bytes,
                   created_at, last_accessed_at,
                   disk_manifest_id, disk_manifest_version,
                   memory_manifest_id, memory_manifest_version,
                   recoverable
            FROM snapshots WHERE session_id = $1
            ORDER BY created_at DESC LIMIT 1
            "#,
        )
        .bind(sid.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::snapshot_from_row(&r)).transpose()
    }

    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        // The `idx_snapshots_disk_manifest` partial index (migration
        // 0018) makes this a fast scan over rows that actually have
        // a chunked manifest. Legacy rows (NULL disk_manifest_id)
        // are filtered out by the index predicate.
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT disk_manifest_id
            FROM snapshots
            WHERE disk_manifest_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                r.try_get::<uuid::Uuid, _>("disk_manifest_id")
                    .map_err(db_err)
            })
            .collect()
    }

    async fn list_live_memory_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        // Mirror of `list_live_disk_manifest_ids`. The partial
        // `idx_snapshots_memory_manifest` (migration 0019) keeps
        // the scan proportional to FC snapshot rows — VZ rows skip
        // memory manifests so they're invisible here.
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT memory_manifest_id
            FROM snapshots
            WHERE memory_manifest_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                r.try_get::<uuid::Uuid, _>("memory_manifest_id")
                    .map_err(db_err)
            })
            .collect()
    }

    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError> {
        // Atomic per-session idx allocation: bump `sessions.next_event_idx`
        // and use the *previous* value as this event's idx. The CTE
        // returns the allocated idx; the outer INSERT uses it. Whole
        // thing is a single statement → one round-trip, no locking
        // dance needed beyond the row-level lock Postgres takes for
        // the UPDATE.
        // Phase 3c HA: the same statement also fires `NOTIFY
        // session_events <payload>` so other coordinator replicas
        // (subscribed via `PgListener`) can re-broadcast the event to
        // their local SSE subscribers. Notifications fire at commit;
        // because the whole statement is one autocommit unit, the
        // NOTIFY arrives only after the INSERT is durable. Replicas
        // that LISTEN before this statement runs see the notification;
        // replicas that subscribe later catch up via the persistent
        // log + `?since=N`.
        let row = sqlx::query(
            r#"
            WITH next AS (
                UPDATE sessions
                   SET next_event_idx = next_event_idx + 1,
                       updated_at = NOW()
                 WHERE id = $1
             RETURNING next_event_idx - 1 AS allocated_idx
            ),
            inserted AS (
                INSERT INTO session_events (session_id, idx, kind, payload)
                SELECT $1, allocated_idx, $2, $3 FROM next
                RETURNING idx
            )
            SELECT i.idx,
                   pg_notify(
                       'session_events',
                       json_build_object('session_id', $1::text, 'idx', i.idx)::text
                   )
              FROM inserted i
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(kind)
        .bind(payload)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let idx: i64 = sqlx::Row::try_get(&row, "idx").map_err(db_err)?;
        Ok(idx)
    }

    async fn list_session_events_since(
        &self,
        session_id: SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT idx, kind, payload, created_at
              FROM session_events
             WHERE session_id = $1 AND idx > $2
             ORDER BY idx
             LIMIT $3
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::persisted_event_from_row).collect()
    }

    // ---------- registry credentials ----------

    async fn upsert_registry_credential(&self, cred: RegistryCredential) -> Result<(), MetaError> {
        // The polymorphic schema stores `auth_kind` as a typed column
        // (so SQL can `WHERE auth_kind = 'static'` without JSON
        // probing) and the variant payload as JSONB. We round-trip
        // the payload through `serde_json::to_value(&cred.auth)` —
        // the `serde(tag = "kind")` rendering on `RegistryAuthSpec`
        // produces a JSON object whose `kind` field already matches
        // the typed column, so the two are kept in sync.
        let auth_kind = cred.auth.kind();
        let auth_config = serde_json::to_value(&cred.auth)
            .map_err(|e| MetaError::Serialization(format!("auth_config: {e}")))?;
        sqlx::query(
            r#"
            INSERT INTO registry_credentials
                (id, registry_host, auth_kind, auth_config,
                 created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, NULL)
            ON CONFLICT (registry_host) DO UPDATE SET
                auth_kind   = EXCLUDED.auth_kind,
                auth_config = EXCLUDED.auth_config,
                updated_at  = NOW()
            "#,
        )
        .bind(cred.id)
        .bind(&cred.registry_host)
        .bind(auth_kind)
        .bind(auth_config)
        .bind(cred.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, registry_host, auth_kind, auth_config,
                   created_at, updated_at
              FROM registry_credentials
             ORDER BY registry_host
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::registry_credential_from_row).collect()
    }

    async fn registry_credential_for_host(
        &self,
        registry_host: &str,
    ) -> Result<Option<RegistryCredential>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, registry_host, auth_kind, auth_config,
                   created_at, updated_at
              FROM registry_credentials
             WHERE registry_host = $1
             LIMIT 1
            "#,
        )
        .bind(registry_host)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::registry_credential_from_row(&r))
            .transpose()
    }

    async fn delete_registry_credential(&self, registry_host: &str) -> Result<(), MetaError> {
        let res = sqlx::query("DELETE FROM registry_credentials WHERE registry_host = $1")
            .bind(registry_host)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    // ---------- harness packs ----------

    async fn upsert_harness_pack(&self, pack: HarnessPack) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO harness_packs
                (id, name, registry_uri, description, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, NULL)
            ON CONFLICT (name) DO UPDATE SET
                registry_uri = EXCLUDED.registry_uri,
                description  = EXCLUDED.description,
                updated_at   = NOW()
            "#,
        )
        .bind(pack.id)
        .bind(&pack.name)
        .bind(&pack.registry_uri)
        .bind(pack.description.as_deref())
        .bind(pack.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_harness_packs(&self) -> Result<Vec<HarnessPack>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, registry_uri, description, created_at, updated_at
              FROM harness_packs
             ORDER BY name
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::harness_pack_from_row).collect()
    }

    async fn get_harness_pack(&self, name: &str) -> Result<Option<HarnessPack>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, name, registry_uri, description, created_at, updated_at
              FROM harness_packs
             WHERE name = $1
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::harness_pack_from_row(&r)).transpose()
    }

    async fn delete_harness_pack(&self, name: &str) -> Result<(), MetaError> {
        let res = sqlx::query("DELETE FROM harness_packs WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    // ---------- enabled images ----------

    async fn upsert_enabled_image(&self, image: EnabledImage) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO enabled_images
                (id, image_uri, manifest_toml, manifest_digest,
                 last_refreshed_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NULL)
            ON CONFLICT (image_uri) DO UPDATE SET
                manifest_toml     = EXCLUDED.manifest_toml,
                manifest_digest   = EXCLUDED.manifest_digest,
                last_refreshed_at = EXCLUDED.last_refreshed_at,
                updated_at        = NOW()
            "#,
        )
        .bind(image.id)
        .bind(&image.image_uri)
        .bind(&image.manifest_toml)
        .bind(&image.manifest_digest)
        .bind(image.last_refreshed_at)
        .bind(image.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   last_refreshed_at, created_at, updated_at
              FROM enabled_images
             ORDER BY image_uri
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::enabled_image_from_row).collect()
    }

    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   last_refreshed_at, created_at, updated_at
              FROM enabled_images
             WHERE image_uri = $1
            "#,
        )
        .bind(image_uri)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::enabled_image_from_row(&r)).transpose()
    }

    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError> {
        let res = sqlx::query("DELETE FROM enabled_images WHERE image_uri = $1")
            .bind(image_uri)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn upsert_session_secrets(&self, secrets: SessionSecrets) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO session_secrets
                (session_id, wrapped_dek, nonce, ciphertext, key_id, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (session_id) DO UPDATE SET
                wrapped_dek = EXCLUDED.wrapped_dek,
                nonce       = EXCLUDED.nonce,
                ciphertext  = EXCLUDED.ciphertext,
                key_id      = EXCLUDED.key_id,
                created_at  = EXCLUDED.created_at
            "#,
        )
        .bind(secrets.session_id.as_uuid())
        .bind(&secrets.wrapped_dek)
        .bind(&secrets.nonce)
        .bind(&secrets.ciphertext)
        .bind(&secrets.key_id)
        .bind(secrets.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_session_secrets(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSecrets>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT session_id, wrapped_dek, nonce, ciphertext, key_id, created_at
            FROM session_secrets
            WHERE session_id = $1
            "#,
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::session_secrets_from_row(&r)).transpose()
    }

    async fn delete_session_secrets(&self, session_id: SessionId) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM session_secrets WHERE session_id = $1")
            .bind(session_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        // Idempotent: missing row is fine. Caller deletes on
        // session-terminate; if there were never overrides, no row
        // ever existed.
        Ok(())
    }
}
