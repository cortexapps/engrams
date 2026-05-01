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
use engram_core::types::session::{checkpoint_branch_for, SessionKind};
use engram_core::types::{
    HostRecord, HostStatus, ImageVersion, PersistedEvent, Session, SessionSpec, SessionStatus,
    SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use sqlx::postgres::{PgPool, PgPoolOptions};
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
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let kind = SessionKind::derive(&spec.workspace);
        let checkpoint_branch = match kind {
            SessionKind::Git => Some(checkpoint_branch_for(SessionId(id))),
            SessionKind::Readonly | SessionKind::Ephemeral => None,
        };
        let workspace_json = serde_json::to_value(&spec.workspace)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let harness_json = serde_json::to_value(&spec.harness)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, user_id, status, host_id,
                 image_repo, image_tag, workspace, harness,
                 session_kind, checkpoint_branch,
                 created_at, last_active_at)
            VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $10)
            "#,
        )
        .bind(id)
        .bind(spec.user_id.as_deref())
        .bind(SessionStatus::Pending.as_str())
        .bind(spec.image.repo())
        .bind(spec.image.tag())
        .bind(workspace_json)
        .bind(harness_json)
        .bind(kind.as_str())
        .bind(checkpoint_branch.as_deref())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(SessionId(id))
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, user_id, status, host_id, sandbox_id,
                   image_repo, image_tag, workspace, harness,
                   session_kind, checkpoint_branch,
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
                   image_repo, image_tag, workspace, harness,
                   session_kind, checkpoint_branch,
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
                               last_heartbeat_at, status, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
            ON CONFLICT (hostname) DO UPDATE SET
                cloud_metadata    = EXCLUDED.cloud_metadata,
                capacity_total_gb = EXCLUDED.capacity_total_gb,
                capacity_used_gb  = EXCLUDED.capacity_used_gb,
                last_heartbeat_at = EXCLUDED.last_heartbeat_at,
                status            = EXCLUDED.status,
                updated_at        = NOW()
            "#,
        )
        .bind(host.id.as_uuid())
        .bind(&host.hostname)
        .bind(cloud_meta)
        .bind(host.capacity.total_gb as i32)
        .bind(host.capacity.used_gb as i32)
        .bind(host.last_heartbeat_at)
        .bind(host.status.as_str())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, hostname, cloud_metadata,
                   capacity_total_gb, capacity_used_gb,
                   last_heartbeat_at, status
            FROM hosts WHERE status IN ('ready','draining')
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

    async fn list_stale_hosts(&self, threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        // `make_interval` keeps the threshold parameterised without
        // string-templating an INTERVAL literal. Cast to BIGINT so a
        // very-large threshold (well past i32::MAX) doesn't overflow.
        let rows = sqlx::query(
            r#"
            SELECT id, hostname, cloud_metadata,
                   capacity_total_gb, capacity_used_gb,
                   last_heartbeat_at, status
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
        sqlx::query(
            r#"
            INSERT INTO snapshots
                (id, session_id, host_id, local_path,
                 image_version, size_bytes, created_at, last_accessed_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE SET
                local_path       = EXCLUDED.local_path,
                last_accessed_at = EXCLUDED.last_accessed_at,
                updated_at       = NOW()
            "#,
        )
        .bind(snap.id.as_uuid())
        .bind(snap.session_id.as_uuid())
        .bind(snap.host_id.map(|h| h.as_uuid()))
        .bind(
            snap.local_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(&snap.image_version)
        .bind(snap.size_bytes as i64)
        .bind(snap.created_at)
        .bind(snap.last_accessed_at)
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
            SELECT id, session_id, host_id, local_path,
                   image_version, size_bytes,
                   created_at, last_accessed_at
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
            SELECT id, session_id, host_id, local_path,
                   image_version, size_bytes,
                   created_at, last_accessed_at
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

    async fn upsert_image_version(&self, version: ImageVersion) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO image_versions (id, repo, tag, blob_url, status, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (repo, tag) DO UPDATE SET
                blob_url   = EXCLUDED.blob_url,
                status     = EXCLUDED.status,
                updated_at = NOW()
            "#,
        )
        .bind(version.id.as_uuid())
        .bind(&version.repo)
        .bind(&version.tag)
        .bind(version.blob_url.as_deref())
        .bind(version.status.as_str())
        .bind(version.created_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn latest_ready_image(&self, repo: &str) -> Result<Option<ImageVersion>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, repo, tag, blob_url, status, created_at
            FROM image_versions
            WHERE repo = $1 AND status = 'ready'
            ORDER BY created_at DESC LIMIT 1
            "#,
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::image_from_row(&r)).transpose()
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
}
